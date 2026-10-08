//! OS 가상 FS 백엔드(WinFsp, FUSE)가 호출하는 읽기 전용 파일시스템.
//!
//! 백엔드 스레드에서 동기적으로 호출한다. 다운로드가 필요한 읽기는 tokio 런타임에 맡기고 결과를 기다린다.
//! 백엔드 스레드가 tokio 런타임 스레드여서는 안 된다.

use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, RwLock};

use tokio::runtime::Handle;

use crate::fetch::Fetcher;
use crate::index::Index;
use crate::paths::Paths;
use crate::tree::{Ino, Node, ROOT, Source, Tree};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: Kind,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub kind: Kind,
}

/// 읽기를 요청한 프로그램. 곡 전체 다운로드(티켓 1개)가 필요할 때만 묻는다.
pub trait Caller: Sync {
    /// 로그에 남길 이름 (예: `java[1234]`)
    fn name(&self) -> String;
    /// BMS 구동기인지. 구동기만 곡 전체 다운로드를 일으킬 수 있다.
    /// 백업·인덱서·썸네일러가 플레이 파일을 몇 바이트 읽는 것만으로 티켓을 쓰지 않도록.
    fn is_player(&self) -> bool;
}

/// CLI `cat`처럼 사용자가 직접 요청한 읽기. 구동기로 취급한다.
pub struct Trusted(pub &'static str);

impl Caller for Trusted {
    fn name(&self) -> String {
        self.0.to_string()
    }

    fn is_player(&self) -> bool {
        true
    }
}

/// OS 가상 FS 백엔드가 호출하는 읽기 전용 파일시스템. 서버 하나([`EbmsFs`])와
/// 여러 서버를 합친 드라이브([`crate::drive::Drive`])가 구현한다.
pub trait ReadOnlyFs: Send + Sync {
    fn getattr(&self, ino: Ino) -> Option<Attr>;
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr>;
    fn parent(&self, ino: Ino) -> Option<Ino>;
    fn readdir(&self, ino: Ino) -> Option<Vec<DirEntry>>;
    /// `offset`부터 최대 `size` 바이트를 읽는다. 필요하면 다운로드를 기다린다.
    /// 곡 전체 다운로드는 `caller`가 구동기일 때만 한다.
    fn read(&self, ino: Ino, offset: u64, size: u32, caller: &dyn Caller) -> Result<Vec<u8>>;

    /// `/`(또는 `\`)로 구분한 경로로 찾는다. 빈 문자열은 루트.
    fn resolve(&self, path: &str) -> Option<Attr> {
        let mut attr = self.getattr(ROOT)?;
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty()) {
            attr = self.lookup(attr.ino, part)?;
        }
        Some(attr)
    }
}

pub struct EbmsFs {
    tree: RwLock<Arc<Tree>>,
    index: Arc<Index>,
    fetcher: Arc<Fetcher>,
    paths: Paths,
    rt: Handle,
}

impl EbmsFs {
    pub fn new(index: Arc<Index>, fetcher: Arc<Fetcher>, paths: Paths, rt: Handle) -> Result<Self> {
        let tree = Tree::build(&index.songs()?, &index.charts()?, None);
        Ok(Self {
            tree: RwLock::new(Arc::new(tree)),
            index,
            fetcher,
            paths,
            rt,
        })
    }

    /// 동기화 후 트리를 다시 만든다. 같은 경로는 같은 ino를 유지한다.
    pub fn reload(&self) -> Result<()> {
        let prev = self.tree();
        let tree = Tree::build(&self.index.songs()?, &self.index.charts()?, Some(&prev));
        *self.tree.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(tree);
        Ok(())
    }

    pub fn tree(&self) -> Arc<Tree> {
        self.tree.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ReadOnlyFs for EbmsFs {
    fn getattr(&self, ino: Ino) -> Option<Attr> {
        self.tree().get(ino).map(|n| attr(ino, n))
    }

    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let tree = self.tree();
        let ino = tree.lookup(parent, name)?;
        tree.get(ino).map(|n| attr(ino, n))
    }

    fn parent(&self, ino: Ino) -> Option<Ino> {
        self.tree().get(ino).map(|n| n.parent())
    }

    fn readdir(&self, ino: Ino) -> Option<Vec<DirEntry>> {
        let tree = self.tree();
        let children = tree.children(ino)?;
        Some(
            children
                .filter_map(|(name, child)| {
                    let kind = if tree.get(child)?.is_dir() {
                        Kind::Dir
                    } else {
                        Kind::File
                    };
                    Some(DirEntry {
                        ino: child,
                        name: name.to_string(),
                        kind,
                    })
                })
                .collect(),
        )
    }

    fn read(&self, ino: Ino, offset: u64, size: u32, caller: &dyn Caller) -> Result<Vec<u8>> {
        let tree = self.tree();
        let Some(Node::File {
            size: file_size,
            source,
            ..
        }) = tree.get(ino)
        else {
            return Err(Error::Other(format!("not a file: {ino}")));
        };
        if offset >= *file_size {
            return Ok(Vec::new());
        }
        let len = (size as u64).min(file_size - offset);
        match source {
            Source::Chart {
                chunk_id,
                data_offset,
            } => read_at(
                &self.paths.chart_chunk(*chunk_id),
                data_offset + offset,
                len,
            ),
            Source::Asset { song_id, entry } => {
                let song = tree
                    .song(*song_id)
                    .cloned()
                    .ok_or_else(|| Error::Other(format!("unknown song {song_id}")))?;
                let fetcher = self.fetcher.clone();
                let entry = entry.clone();
                let path = self
                    .rt
                    .block_on(async move { fetcher.ensure(&song, &entry, caller).await })?;
                read_at(&path, offset, len)
            }
        }
    }
}

fn attr(ino: Ino, node: &Node) -> Attr {
    Attr {
        ino,
        kind: if node.is_dir() { Kind::Dir } else { Kind::File },
        size: node.size(),
    }
}

fn read_at(path: &std::path::Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}
