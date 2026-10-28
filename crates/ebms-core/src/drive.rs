//! 연결한 서버의 곡을 서버 이름 폴더 하나 아래에 보여주는 가상 드라이브.
//!
//! ```text
//! E:\
//! └─ 서버 이름\00123 Artist - Title\...
//! ```
//!
//! ino는 `(slot << SLOT_SHIFT) | 서버 안의 ino`. 루트는 slot 0의 [`ROOT`].
//! 서버를 넣을 때마다 slot을 새로 정하므로 다른 서버로 바꿔도 예전 서버의 ino가 새 서버의 파일을 가리키지 않는다.

use std::sync::{Arc, RwLock};

use crate::fs::{Attr, Caller, DirEntry, Kind, ReadDone, ReadOnlyFs};
use crate::tree::{Ino, ROOT};
use crate::{Error, Result};

const SLOT_SHIFT: u32 = 48;
const INNER_MASK: Ino = (1 << SLOT_SHIFT) - 1;

struct Member {
    /// 최상위 폴더 이름
    dir: String,
    slot: Ino,
    fs: Arc<dyn ReadOnlyFs>,
}

#[derive(Default)]
struct State {
    member: Option<Member>,
    /// 마지막으로 쓴 slot
    last_slot: Ino,
}

/// 서버 폴더 하나를 둔 드라이브.
#[derive(Default)]
pub struct Drive {
    state: RwLock<State>,
}

impl Drive {
    pub fn new() -> Self {
        Self::default()
    }

    /// 서버를 넣는다(있던 서버는 뺀다). `name`은 최상위 폴더 이름이 된다.
    pub fn set(&self, id: &str, name: &str, fs: Arc<dyn ReadOnlyFs>) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.last_slot += 1;
        let slot = s.last_slot;
        s.member = Some(Member {
            dir: folder_name(name, id),
            slot,
            fs,
        });
    }

    pub fn clear(&self) {
        self.state.write().unwrap_or_else(|e| e.into_inner()).member = None;
    }

    fn member(&self, ino: Ino) -> Option<(Arc<dyn ReadOnlyFs>, Ino)> {
        let s = self.state.read().unwrap_or_else(|e| e.into_inner());
        let member = s.member.as_ref().filter(|m| m.slot == ino >> SLOT_SHIFT)?;
        Some((member.fs.clone(), member.slot))
    }
}

fn outer(slot: Ino, inner: Ino) -> Ino {
    (slot << SLOT_SHIFT) | (inner & INNER_MASK)
}

fn lift(slot: Ino, mut a: Attr) -> Attr {
    a.ino = outer(slot, a.ino);
    a
}

fn root_attr() -> Attr {
    Attr {
        ino: ROOT,
        kind: Kind::Dir,
        size: 0,
    }
}

/// 서버 이름을 폴더 이름으로. 쓸 수 없는 이름이면 id를 쓴다.
fn folder_name(name: &str, id: &str) -> String {
    let clean = crate::tree::sanitize(name);
    if clean.is_empty() || clean == "." || clean == ".." {
        id.to_string()
    } else {
        clean
    }
}

impl ReadOnlyFs for Drive {
    fn getattr(&self, ino: Ino) -> Option<Attr> {
        if ino == ROOT {
            return Some(root_attr());
        }
        let (fs, slot) = self.member(ino)?;
        fs.getattr(ino & INNER_MASK).map(|a| lift(slot, a))
    }

    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        if parent == ROOT {
            let (fs, slot) = {
                let s = self.state.read().unwrap_or_else(|e| e.into_inner());
                let member = s
                    .member
                    .as_ref()
                    .filter(|m| m.dir.eq_ignore_ascii_case(name))?;
                (member.fs.clone(), member.slot)
            };
            return fs.getattr(ROOT).map(|a| lift(slot, a));
        }
        let (fs, slot) = self.member(parent)?;
        fs.lookup(parent & INNER_MASK, name).map(|a| lift(slot, a))
    }

    fn parent(&self, ino: Ino) -> Option<Ino> {
        if ino == ROOT {
            return Some(ROOT);
        }
        let (fs, slot) = self.member(ino)?;
        let inner = ino & INNER_MASK;
        if inner == ROOT {
            return Some(ROOT);
        }
        fs.parent(inner).map(|p| outer(slot, p))
    }

    fn readdir_from(&self, ino: Ino, offset: usize, add: &mut dyn FnMut(DirEntry) -> bool) -> bool {
        if ino == ROOT {
            let entry = {
                let s = self.state.read().unwrap_or_else(|e| e.into_inner());
                s.member.as_ref().map(|m| DirEntry {
                    ino: outer(m.slot, ROOT),
                    name: m.dir.clone(),
                    kind: Kind::Dir,
                })
            };
            if let Some(e) = entry.filter(|_| offset == 0) {
                add(e);
            }
            return true;
        }
        let Some((fs, slot)) = self.member(ino) else {
            return false;
        };
        fs.readdir_from(ino & INNER_MASK, offset, &mut |mut e| {
            e.ino = outer(slot, e.ino);
            add(e)
        })
    }

    fn read(&self, ino: Ino, offset: u64, size: u32, caller: &dyn Caller) -> Result<Vec<u8>> {
        let (fs, _) = self
            .member(ino)
            .ok_or_else(|| Error::Other(format!("not a file: {ino}")))?;
        fs.read(ino & INNER_MASK, offset, size, caller)
    }

    fn read_async(
        &self,
        ino: Ino,
        offset: u64,
        size: u32,
        caller: Box<dyn Caller>,
        done: ReadDone,
    ) {
        match self.member(ino) {
            Some((fs, _)) => fs.read_async(ino & INNER_MASK, offset, size, caller, done),
            None => done(Err(Error::Other(format!("not a file: {ino}")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::Trusted;

    /// 루트 아래 `song/a.txt` 하나만 있는 서버.
    struct Fake(&'static str);

    const SONG: Ino = 2;
    const FILE: Ino = 3;

    impl ReadOnlyFs for Fake {
        fn getattr(&self, ino: Ino) -> Option<Attr> {
            let (kind, size) = match ino {
                ROOT | SONG => (Kind::Dir, 0),
                FILE => (Kind::File, self.0.len() as u64),
                _ => return None,
            };
            Some(Attr { ino, kind, size })
        }
        fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
            match (parent, name) {
                (ROOT, "song") => self.getattr(SONG),
                (SONG, "a.txt") => self.getattr(FILE),
                _ => None,
            }
        }
        fn parent(&self, ino: Ino) -> Option<Ino> {
            match ino {
                ROOT | SONG => Some(ROOT),
                FILE => Some(SONG),
                _ => None,
            }
        }
        fn readdir_from(
            &self,
            ino: Ino,
            offset: usize,
            add: &mut dyn FnMut(DirEntry) -> bool,
        ) -> bool {
            let (ino, name, kind) = match ino {
                ROOT => (SONG, "song", Kind::Dir),
                SONG => (FILE, "a.txt", Kind::File),
                _ => return false,
            };
            if offset == 0 {
                add(DirEntry {
                    ino,
                    name: name.into(),
                    kind,
                });
            }
            true
        }
        fn read(&self, ino: Ino, offset: u64, size: u32, _: &dyn Caller) -> Result<Vec<u8>> {
            assert_eq!(ino, FILE);
            let data = self.0.as_bytes();
            let start = (offset as usize).min(data.len());
            let end = (start + size as usize).min(data.len());
            Ok(data[start..end].to_vec())
        }
    }

    fn read_all(fs: &dyn ReadOnlyFs, path: &str) -> String {
        let a = fs.resolve(path).expect(path);
        String::from_utf8(fs.read(a.ino, 0, 1024, &Trusted("test")).unwrap()).unwrap()
    }

    #[test]
    fn server_is_a_top_level_folder() {
        let drive = Drive::new();
        assert!(drive.readdir(ROOT).unwrap().is_empty());
        drive.set("b", "B:/x", Arc::new(Fake("from b")));

        let names: Vec<String> = drive
            .readdir(ROOT)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["B__x"]);
        assert_eq!(read_all(&drive, "b__x/song/a.txt"), "from b");
        assert!(drive.resolve("B__x/missing").is_none());

        // 부모를 따라 올라가면 루트
        let file = drive.resolve("B__x/song/a.txt").unwrap().ino;
        let song = drive.parent(file).unwrap();
        let top = drive.parent(song).unwrap();
        assert_eq!(drive.parent(top), Some(ROOT));
        assert_eq!(drive.readdir(song).unwrap()[0].ino, file);
    }

    #[test]
    fn switching_servers_changes_inodes() {
        let drive = Drive::new();
        drive.set("a", "A", Arc::new(Fake("a")));
        let old = drive.resolve("A/song/a.txt").unwrap().ino;

        drive.set("b", "B", Arc::new(Fake("b")));
        assert!(drive.resolve("A").is_none());
        assert!(drive.getattr(old).is_none());
        assert_eq!(read_all(&drive, "B/song/a.txt"), "b");
        assert_ne!(drive.resolve("B/song/a.txt").unwrap().ino, old);

        drive.clear();
        assert!(drive.resolve("B").is_none());
        assert!(drive.readdir(ROOT).unwrap().is_empty());
    }
}
