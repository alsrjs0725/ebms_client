//! Linux FUSE 백엔드 (macOS는 추후 검토). [`ebms_core::fs::ReadOnlyFs`]를 읽기 전용으로 마운트한다.
//! 보통 서버별 최상위 폴더로 합친 [`ebms_core::drive::Drive`]를 마운트한다.
//! 읽기를 요청한 프로세스를 `/proc`에서 확인해, 구동기만 곡 전체 다운로드(티켓 1개)를 일으킬 수 있게 한다.
#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ebms_core::Error;
use ebms_core::fs::{Attr, Caller, Kind, ReadOnlyFs};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, IoctlFlags,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyOpen, Request, SessionACL,
};
use tracing::warn;

pub use fuser::BackgroundSession;

/// 트리는 동기화 때만 바뀌므로 커널 캐시를 길게 둔다.
const TTL: Duration = Duration::from_secs(60);

struct FuseFs {
    fs: Arc<dyn ReadOnlyFs>,
    /// 곡 전체를 받을 수 있는 프로그램 이름 ([`ebms_core::players`])
    players: Vec<String>,
    uid: u32,
    gid: u32,
}

/// FUSE 요청을 보낸 프로세스. 곡 다운로드가 필요할 때만 `/proc`을 읽는다.
struct ProcCaller<'a> {
    pid: u32,
    players: &'a [String],
}

impl ProcCaller<'_> {
    fn exe(&self) -> Option<String> {
        std::fs::read_link(format!("/proc/{}/exe", self.pid))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn args(&self) -> Vec<String> {
        std::fs::read(format!("/proc/{}/cmdline", self.pid))
            .map(|raw| {
                raw.split(|&b| b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Caller for ProcCaller<'_> {
    fn name(&self) -> String {
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", self.pid));
        let comm = comm.as_deref().map(str::trim).unwrap_or("?");
        // java처럼 실행 파일만으로는 알 수 없는 경우를 위해 명령줄도 남긴다.
        let args = self.args().join(" ");
        let args: String = args.chars().take(200).collect();
        format!("{comm}[{}] {args}", self.pid)
    }

    fn is_player(&self) -> bool {
        ebms_core::players::is_player(self.players, self.exe().as_deref(), &self.args())
    }
}

impl FuseFs {
    fn file_attr(&self, a: Attr) -> FileAttr {
        let (kind, perm, nlink) = match a.kind {
            Kind::Dir => (FileType::Directory, 0o555, 2),
            Kind::File => (FileType::RegularFile, 0o444, 1),
        };
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 64 * 1024,
        }
    }
}

impl Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match name.to_str().and_then(|n| self.fs.lookup(parent.into(), n)) {
            Some(a) => reply.entry(&TTL, &self.file_attr(a), fuser::Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.fs.getattr(ino.into()) {
            Some(a) => reply.attr(&TTL, &self.file_attr(a)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        match self.fs.getattr(ino.into()) {
            // 내용이 바뀌지 않으므로 페이지 캐시를 유지한다.
            Some(a) if a.kind == Kind::File => {
                reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE)
            }
            Some(_) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let caller = ProcCaller {
            pid: req.pid(),
            players: &self.players,
        };
        match self.fs.read(ino.into(), offset, size, &caller) {
            Ok(data) => reply.data(&data),
            // 이미 로그를 남겼다. 다른 프로그램에는 권한 없음으로 보인다.
            Err(Error::NotPlayer { .. }) => reply.error(Errno::EACCES),
            Err(e) => {
                warn!(ino = u64::from(ino), %e, "read failed");
                reply.error(Errno::EIO);
            }
        }
    }

    /// 읽기 전용이라 비울 것이 없다.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// 터미널 ioctl 등은 지원하지 않는다. 기본 구현은 매번 경고 로그를 남기므로 조용히 거절한다.
    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        reply.error(Errno::ENOTTY);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = u64::from(ino);
        let mut next = offset;
        // `.`과 `..`이 0, 1번. 항목의 offset은 다음 항목 번호.
        if next == 0 {
            next = 1;
            if reply.add(INodeNo(ino), next, FileType::Directory, ".") {
                return reply.ok();
            }
        }
        if next == 1 {
            next = 2;
            let parent = self.fs.parent(ino).unwrap_or(ino);
            if reply.add(INodeNo(parent), next, FileType::Directory, "..") {
                return reply.ok();
            }
        }
        let is_dir = self.fs.readdir_from(ino, (next - 2) as usize, &mut |e| {
            let kind = if e.kind == Kind::Dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            next += 1;
            reply.add(INodeNo(e.ino), next, kind, e.name)
        });
        if !is_dir {
            return reply.error(Errno::ENOTDIR);
        }
        reply.ok();
    }
}

/// 이전 프로세스가 언마운트하지 못하고 죽어 남은 마운트(`ENOTCONN`)를 걷어낸다.
fn clear_stale(mountpoint: &Path) {
    let stale = matches!(
        std::fs::metadata(mountpoint),
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN)
    );
    if !stale {
        return;
    }
    warn!(mountpoint = %mountpoint.display(), "stale mount, unmounting");
    for cmd in ["fusermount3", "fusermount"] {
        let status = std::process::Command::new(cmd)
            .arg("-uz")
            .arg(mountpoint)
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return;
        }
    }
}

/// 백그라운드 스레드에서 마운트한다. 반환값을 drop하면 언마운트된다.
/// 마운트 위치 폴더가 없으면 만든다. `players`([`ebms_core::config::Config::players`])에
/// 해당하는 프로세스만 받지 않은 플레이 파일을 읽어 곡 전체를 받을 수 있다.
pub fn spawn_mount(
    fs: Arc<dyn ReadOnlyFs>,
    mountpoint: &Path,
    players: Vec<String>,
) -> std::io::Result<BackgroundSession> {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("ebms".into()),
        MountOption::Subtype("ebms".into()),
        MountOption::DefaultPermissions,
        MountOption::NoExec,
    ];
    config.acl = SessionACL::Owner;
    clear_stale(mountpoint);
    std::fs::create_dir_all(mountpoint)?;
    // 다운로드를 기다리는 읽기가 다른 요청을 막지 않도록 여러 스레드로.
    config.n_threads = Some(8);
    // SAFETY: getuid/getgid는 실패하지 않는다.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    fuser::spawn_mount(
        FuseFs {
            fs,
            players,
            uid,
            gid,
        },
        mountpoint,
        &config,
    )
}
