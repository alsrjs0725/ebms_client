//! Linux FUSE 백엔드 (macOS는 추후 검토). [`ebms_core::fs::ReadOnlyFs`]를 읽기 전용으로 마운트한다.
//! 보통 서버별 최상위 폴더로 합친 [`ebms_core::drive::Drive`]를 마운트한다.
#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ebms_core::fs::{Attr, Kind, ReadOnlyFs};
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
    uid: u32,
    gid: u32,
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
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.fs.read(ino.into(), offset, size) {
            Ok(data) => reply.data(&data),
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
        let Some(children) = self.fs.readdir(ino.into()) else {
            return reply.error(Errno::ENOTDIR);
        };
        let parent = self.fs.parent(ino.into()).unwrap_or(ino.into());
        let entries = [
            (u64::from(ino), FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ]
        .into_iter()
        .chain(children.into_iter().map(|e| {
            let kind = if e.kind == Kind::Dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            (e.ino, kind, e.name)
        }));
        for (i, (child, kind, name)) in entries.enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break;
            }
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
/// 마운트 위치 폴더가 없으면 만든다.
pub fn spawn_mount(
    fs: Arc<dyn ReadOnlyFs>,
    mountpoint: &Path,
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
    fuser::spawn_mount(FuseFs { fs, uid, gid }, mountpoint, &config)
}
