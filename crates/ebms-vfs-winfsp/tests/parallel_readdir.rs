//! 같은 폴더를 여러 핸들로 동시에 나열해도 항목이 빠지지 않는지 확인한다.
//! Qwilight는 키음마다 폴더를 `이름.*`로 동시에 검색하므로, 나열이 잘리면 키음이 빠진다.
//! WinFsp가 설치돼 있어야 하므로 기본으로는 돌지 않는다: `cargo test -p ebms-vfs-winfsp -- --ignored`
#![cfg(windows)]

use std::sync::Arc;

use ebms_core::Result;
use ebms_core::fs::{Attr, Caller, DirEntry, Kind, ReadOnlyFs};
use ebms_core::openlog::OpenLog;
use ebms_core::players::Players;
use ebms_core::tree::{Ino, ROOT};

const SONG: Ino = 2;
const FIRST_FILE: Ino = 10;
const FILES: u64 = 900;

/// 루트 아래 `song` 폴더 하나와 그 안의 파일 `FILES`개.
struct MockFs;

impl MockFs {
    fn name(ino: Ino) -> String {
        format!("Drum'n'bass_Vocal_Kontakt_#2_{:04}.ogg", ino - FIRST_FILE)
    }
}

impl ReadOnlyFs for MockFs {
    fn getattr(&self, ino: Ino) -> Option<Attr> {
        match ino {
            ROOT | SONG => Some(Attr {
                ino,
                kind: Kind::Dir,
                size: 0,
            }),
            i if (FIRST_FILE..FIRST_FILE + FILES).contains(&i) => Some(Attr {
                ino,
                kind: Kind::File,
                size: 1,
            }),
            _ => None,
        }
    }

    fn lookup(&self, parent: Ino, name: &str) -> Option<Attr> {
        let mut found = None;
        self.readdir_from(parent, 0, &mut |e| {
            if e.name.eq_ignore_ascii_case(name) {
                found = Some(e.ino);
                return true;
            }
            false
        });
        found.and_then(|ino| self.getattr(ino))
    }

    fn parent(&self, ino: Ino) -> Option<Ino> {
        match ino {
            ROOT => None,
            SONG => Some(ROOT),
            _ => Some(SONG),
        }
    }

    fn readdir_from(&self, ino: Ino, offset: usize, add: &mut dyn FnMut(DirEntry) -> bool) -> bool {
        match ino {
            ROOT => {
                if offset == 0 {
                    add(DirEntry {
                        ino: SONG,
                        name: "song".into(),
                        kind: Kind::Dir,
                    });
                }
                true
            }
            SONG => {
                for i in (FIRST_FILE + offset as u64)..(FIRST_FILE + FILES) {
                    if add(DirEntry {
                        ino: i,
                        name: Self::name(i),
                        kind: Kind::File,
                    }) {
                        break;
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn read(&self, _: Ino, _: u64, _: u32, _: &dyn Caller) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn modified(&self, ino: Ino) -> Option<std::time::SystemTime> {
        (ino == ROOT).then(list_time)
    }
}

/// 곡 목록이 마지막으로 바뀐 시각 (루트에만)
fn list_time() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000)
}

#[test]
#[ignore = "WinFsp 설치가 필요하다"]
fn parallel_readdir_lists_every_entry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let base = std::env::temp_dir().join(format!("ebms-readdir-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let mountpoint = base.join("mnt");
    let _mounted = ebms_vfs_winfsp::spawn_mount(
        Arc::new(MockFs),
        &mountpoint,
        Players::default(),
        OpenLog::default(),
        rt.handle().clone(),
    )
    .expect("mount");
    let dir = mountpoint.join("song");

    // 수정 시각은 마운트 시각이 아니라 고정값이다. 바뀌면 구동기가 켤 때마다 곡을 다시 불러온다.
    let root_time = std::fs::metadata(&mountpoint).unwrap().modified().unwrap();
    assert_eq!(root_time, list_time(), "root");
    let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    for p in [&dir, &file] {
        assert_eq!(
            std::fs::metadata(p).unwrap().modified().unwrap(),
            fixed,
            "{p:?}"
        );
    }

    let count = |d: &std::path::Path| std::fs::read_dir(d).unwrap().count();
    for _ in 0..3 {
        assert_eq!(count(&dir), FILES as usize, "sequential");
    }
    // 두 핸들을 열어 두고 번갈아 읽는다. 뒤 핸들의 첫 페이지는 WinFsp 커널 캐시에서 온다.
    {
        let mut a = std::fs::read_dir(&dir).unwrap();
        let mut b = std::fs::read_dir(&dir).unwrap();
        let (mut na, mut nb) = (0, 0);
        loop {
            let x = a.next().is_some();
            let y = b.next().is_some();
            na += x as usize;
            nb += y as usize;
            if !x && !y {
                break;
            }
        }
        assert_eq!((na, nb), (FILES as usize, FILES as usize), "interleaved");
    }

    let bad: Vec<usize> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let dir = &dir;
                s.spawn(move || {
                    (0..8)
                        .map(|_| count(dir))
                        .filter(|&n| n != FILES as usize)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(bad.is_empty(), "parallel listings came back short: {bad:?}");
}
