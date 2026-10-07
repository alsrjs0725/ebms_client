//! 여러 서버를 하나의 가상 드라이브로 합친다. 서버마다 최상위 폴더 하나를 둔다.
//!
//! ```text
//! E:\ (또는 ~/ebms)
//! ├─ 서버 A\00123 Artist - Title\...
//! └─ 서버 B\00007 Other - Song\...
//! ```
//!
//! 같은 곡이 여러 서버에 있어도 합치지 않는다. 한 서버가 꺼지거나 세션이 만료돼도 다른 서버는 그대로다.
//!
//! ino는 `(slot << SLOT_SHIFT) | 서버 안의 ino`. 루트는 slot 0의 [`ROOT`].
//! slot은 서버 id별로 정해 두고 다시 쓰지 않으므로 서버를 빼고 넣어도 다른 서버의 ino는 그대로다.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use crate::fs::{Attr, DirEntry, Kind, ReadOnlyFs};
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
struct Members {
    /// 서버 id → 멤버
    by_id: BTreeMap<String, Member>,
    /// 서버 id → slot. 빠진 서버의 slot도 남겨 둔다.
    slots: HashMap<String, Ino>,
}

impl Members {
    fn by_slot(&self, slot: Ino) -> Option<&Member> {
        self.by_id.values().find(|m| m.slot == slot)
    }

    fn by_dir(&self, name: &str) -> Option<&Member> {
        let mut members = self.by_id.values();
        members
            .clone()
            .find(|m| m.dir == name)
            .or_else(|| members.find(|m| m.dir.eq_ignore_ascii_case(name)))
    }
}

/// 서버별 최상위 폴더로 합친 드라이브.
#[derive(Default)]
pub struct Drive {
    members: RwLock<Members>,
}

impl Drive {
    pub fn new() -> Self {
        Self::default()
    }

    /// 서버를 넣는다. 같은 id가 있으면 바꾼다. `name`은 최상위 폴더 이름이 된다.
    pub fn insert(&self, id: &str, name: &str, fs: Arc<dyn ReadOnlyFs>) {
        let mut m = self.members.write().unwrap_or_else(|e| e.into_inner());
        let next = m.slots.values().max().copied().unwrap_or(0) + 1;
        let slot = *m.slots.entry(id.to_string()).or_insert(next);
        let dir = folder_name(name, id);
        m.by_id.insert(id.to_string(), Member { dir, slot, fs });
    }

    pub fn remove(&self, id: &str) {
        let mut m = self.members.write().unwrap_or_else(|e| e.into_inner());
        m.by_id.remove(id);
    }

    /// 들어 있는 서버 id 목록.
    pub fn ids(&self) -> Vec<String> {
        let m = self.members.read().unwrap_or_else(|e| e.into_inner());
        m.by_id.keys().cloned().collect()
    }

    fn member(&self, ino: Ino) -> Option<(Arc<dyn ReadOnlyFs>, Ino)> {
        let m = self.members.read().unwrap_or_else(|e| e.into_inner());
        let member = m.by_slot(ino >> SLOT_SHIFT)?;
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
                let m = self.members.read().unwrap_or_else(|e| e.into_inner());
                let member = m.by_dir(name)?;
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

    fn readdir(&self, ino: Ino) -> Option<Vec<DirEntry>> {
        if ino == ROOT {
            let m = self.members.read().unwrap_or_else(|e| e.into_inner());
            let mut entries: Vec<DirEntry> = m
                .by_id
                .values()
                .map(|member| DirEntry {
                    ino: outer(member.slot, ROOT),
                    name: member.dir.clone(),
                    kind: Kind::Dir,
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            return Some(entries);
        }
        let (fs, slot) = self.member(ino)?;
        let entries = fs.readdir(ino & INNER_MASK)?;
        Some(
            entries
                .into_iter()
                .map(|mut e| {
                    e.ino = outer(slot, e.ino);
                    e
                })
                .collect(),
        )
    }

    fn read(&self, ino: Ino, offset: u64, size: u32) -> Result<Vec<u8>> {
        let (fs, _) = self
            .member(ino)
            .ok_or_else(|| Error::Other(format!("not a file: {ino}")))?;
        fs.read(ino & INNER_MASK, offset, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn readdir(&self, ino: Ino) -> Option<Vec<DirEntry>> {
            let (ino, name, kind) = match ino {
                ROOT => (SONG, "song", Kind::Dir),
                SONG => (FILE, "a.txt", Kind::File),
                _ => return None,
            };
            Some(vec![DirEntry {
                ino,
                name: name.into(),
                kind,
            }])
        }
        fn read(&self, ino: Ino, offset: u64, size: u32) -> Result<Vec<u8>> {
            assert_eq!(ino, FILE);
            let data = self.0.as_bytes();
            let start = (offset as usize).min(data.len());
            let end = (start + size as usize).min(data.len());
            Ok(data[start..end].to_vec())
        }
    }

    fn read_all(fs: &dyn ReadOnlyFs, path: &str) -> String {
        let a = fs.resolve(path).expect(path);
        String::from_utf8(fs.read(a.ino, 0, 1024).unwrap()).unwrap()
    }

    #[test]
    fn servers_are_top_level_folders() {
        let drive = Drive::new();
        drive.insert("a", "Server A", Arc::new(Fake("from a")));
        drive.insert("b", "B:/x", Arc::new(Fake("from b")));

        let names: Vec<String> = drive
            .readdir(ROOT)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["B__x", "Server A"]);

        assert_eq!(read_all(&drive, "Server A/song/a.txt"), "from a");
        assert_eq!(read_all(&drive, "b__x/song/a.txt"), "from b");
        assert!(drive.resolve("Server A/missing").is_none());

        // 부모를 따라 올라가면 루트
        let file = drive.resolve("Server A/song/a.txt").unwrap().ino;
        let song = drive.parent(file).unwrap();
        let top = drive.parent(song).unwrap();
        assert_eq!(drive.parent(top), Some(ROOT));
        assert_eq!(drive.readdir(song).unwrap()[0].ino, file);
    }

    #[test]
    fn inodes_stay_when_servers_change() {
        let drive = Drive::new();
        drive.insert("a", "A", Arc::new(Fake("a")));
        drive.insert("b", "B", Arc::new(Fake("b")));
        let before = drive.resolve("B/song/a.txt").unwrap().ino;

        drive.remove("a");
        assert!(drive.resolve("A").is_none());
        assert_eq!(drive.resolve("B/song/a.txt").unwrap().ino, before);
        assert!(drive.getattr(outer(1, FILE)).is_none());

        drive.insert("c", "C", Arc::new(Fake("c")));
        drive.insert("a", "A", Arc::new(Fake("a2")));
        assert_eq!(read_all(&drive, "A/song/a.txt"), "a2");
        assert_ne!(
            drive.resolve("C/song/a.txt").unwrap().ino,
            drive.resolve("A/song/a.txt").unwrap().ino
        );
        assert_eq!(drive.ids(), ["a", "b", "c"]);
    }
}
