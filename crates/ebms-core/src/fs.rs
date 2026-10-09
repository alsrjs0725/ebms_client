//! OS 가상 FS 백엔드(WinFsp, FUSE)가 호출하는 읽기 전용 파일시스템.
//!
//! 백엔드 스레드에서 호출한다. 다운로드가 필요한 읽기는 [`ReadOnlyFs::read_async`]로 tokio 런타임에
//! 맡기고 백엔드 스레드는 바로 돌아간다. 다운로드가 끝나면 런타임 쪽에서 완료 콜백을 부른다.
//! 동기 [`ReadOnlyFs::read`]는 CLI·테스트용이며, 호출 스레드가 tokio 런타임 스레드여서는 안 된다.

use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, RwLock};

use tokio::runtime::Handle;

use crate::fetch::Fetcher;
use crate::index::Index;
use crate::manifest::FileEntry;
use crate::paths::Paths;
use crate::tree::{Ino, ROOT, SongInfo, Source, Stat, Tree};
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
pub trait Caller: Send + Sync {
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

/// [`ReadOnlyFs::read_async`]의 완료 콜백. 정확히 한 번 불린다.
pub type ReadDone = Box<dyn FnOnce(Result<Vec<u8>>) + Send>;

/// OS 가상 FS 백엔드가 호출하는 읽기 전용 파일시스템. 서버 하나([`EbmsFs`])와
/// 여러 서버를 합친 드라이브([`crate::drive::Drive`])가 구현한다.
pub trait ReadOnlyFs: Send + Sync {
    fn getattr(&self, ino: Ino) -> Option<Attr>;
    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr>;
    fn parent(&self, ino: Ino) -> Option<Ino>;
    /// `offset`번째 항목부터 `add`에 넘긴다. `add`가 true를 돌려주면 멈춘다. 폴더가 아니면 false.
    fn readdir_from(&self, ino: Ino, offset: usize, add: &mut dyn FnMut(DirEntry) -> bool) -> bool;
    /// `offset`부터 최대 `size` 바이트를 읽는다. 필요하면 다운로드를 기다린다.
    /// 곡 전체 다운로드는 `caller`가 구동기일 때만 한다.
    fn read(&self, ino: Ino, offset: u64, size: u32, caller: &dyn Caller) -> Result<Vec<u8>>;

    /// [`read`](Self::read)와 같지만 다운로드를 기다리지 않는다. 결과는 `done`으로 넘긴다.
    /// 캐시에 있으면 호출 스레드에서 바로, 다운로드가 필요하면 끝난 뒤 런타임 스레드에서 `done`을 부른다.
    /// FS 콜백 스레드가 네트워크를 기다려 드라이브 전체가 멈추지 않도록 백엔드는 이것을 쓴다.
    fn read_async(
        &self,
        ino: Ino,
        offset: u64,
        size: u32,
        caller: Box<dyn Caller>,
        done: ReadDone,
    ) {
        done(self.read(ino, offset, size, &*caller));
    }

    /// 폴더 항목 전체.
    fn readdir(&self, ino: Ino) -> Option<Vec<DirEntry>> {
        let mut out = Vec::new();
        self.readdir_from(ino, 0, &mut |e| {
            out.push(e);
            false
        })
        .then_some(out)
    }

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
        let tree = Tree::open(index.clone())?;
        Ok(Self {
            tree: RwLock::new(Arc::new(tree)),
            index,
            fetcher,
            paths,
            rt,
        })
    }

    /// 동기화 후 트리를 다시 만든다. 같은 경로는 같은 ino를 유지한다.
    /// 인덱스를 읽으므로 런타임 밖(`spawn_blocking`)에서 부른다.
    pub fn reload(&self) -> Result<()> {
        let tree = Tree::open(self.index.clone())?;
        *self.tree.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(tree);
        Ok(())
    }

    pub fn tree(&self) -> Arc<Tree> {
        self.tree.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ReadOnlyFs for EbmsFs {
    fn getattr(&self, ino: Ino) -> Option<Attr> {
        self.tree().get(ino).map(attr)
    }

    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let tree = self.tree();
        let ino = tree.lookup(parent, name)?;
        tree.get(ino).map(attr)
    }

    fn parent(&self, ino: Ino) -> Option<Ino> {
        self.tree().parent(ino)
    }

    fn readdir_from(&self, ino: Ino, offset: usize, add: &mut dyn FnMut(DirEntry) -> bool) -> bool {
        self.tree()
            .read_dir(ino, offset, |child, name, is_dir| {
                add(DirEntry {
                    ino: child,
                    name: name.to_string(),
                    kind: if is_dir { Kind::Dir } else { Kind::File },
                })
            })
            .is_some()
    }

    fn read(&self, ino: Ino, offset: u64, size: u32, caller: &dyn Caller) -> Result<Vec<u8>> {
        match self.plan(ino, offset, size)? {
            Plan::Done(data) => Ok(data),
            Plan::Fetch { song, entry, len } => {
                let fetcher = self.fetcher.clone();
                let path = self
                    .rt
                    .block_on(async move { fetcher.ensure(&song, &entry, caller).await })?;
                read_at(&path, offset, len)
            }
        }
    }

    fn read_async(
        &self,
        ino: Ino,
        offset: u64,
        size: u32,
        caller: Box<dyn Caller>,
        done: ReadDone,
    ) {
        let (song, entry, len) = match self.plan(ino, offset, size) {
            Ok(Plan::Fetch { song, entry, len }) => (song, entry, len),
            Ok(Plan::Done(data)) => return done(Ok(data)),
            Err(e) => return done(Err(e)),
        };
        let fetcher = self.fetcher.clone();
        self.rt.spawn(async move {
            let result = match fetcher.ensure(&song, &entry, &*caller).await {
                Ok(path) => tokio::task::spawn_blocking(move || read_at(&path, offset, len))
                    .await
                    .unwrap_or_else(|e| Err(Error::Other(e.to_string()))),
                Err(e) => Err(e),
            };
            done(result);
        });
    }
}

/// 읽기를 바로 끝낼 수 있는지, 다운로드가 필요한지.
enum Plan {
    Done(Vec<u8>),
    Fetch {
        song: Arc<SongInfo>,
        entry: Arc<FileEntry>,
        len: u64,
    },
}

impl EbmsFs {
    /// 차트, 사전 청크, 캐시에 있는 에셋은 바로 읽는다. 받아야 하면 [`Plan::Fetch`].
    fn plan(&self, ino: Ino, offset: u64, size: u32) -> Result<Plan> {
        let Some((song, file_size, source)) = self.tree().file(ino) else {
            return Err(Error::Other(format!("not a file: {ino}")));
        };
        if offset >= file_size {
            return Ok(Plan::Done(Vec::new()));
        }
        let len = (size as u64).min(file_size - offset);
        match source {
            Source::Chart {
                chunk_id,
                data_offset,
            } => read_at(&self.paths.chart_chunk(chunk_id), data_offset + offset, len)
                .map(Plan::Done),
            Source::Pre {
                chunk_id,
                data_offset,
            } => {
                read_at(&self.paths.pre_chunk(chunk_id), data_offset + offset, len).map(Plan::Done)
            }
            Source::Asset { entry, .. } => {
                match self.fetcher.cache().get(song.song_id, &entry.path)? {
                    Some(path) => read_at(&path, offset, len).map(Plan::Done),
                    None => Ok(Plan::Fetch { song, entry, len }),
                }
            }
        }
    }
}

fn attr(s: Stat) -> Attr {
    Attr {
        ino: s.ino,
        kind: if s.is_dir { Kind::Dir } else { Kind::File },
        size: s.size,
    }
}

fn read_at(path: &std::path::Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}
