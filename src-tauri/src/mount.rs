//! 가상 드라이브 마운트. Linux는 FUSE, Windows는 WinFsp. macOS는 아직 없음.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ebms_core::hub::Hub;
use serde::Serialize;

#[cfg(target_os = "linux")]
type Session = ebms_vfs_fuse::BackgroundSession;
#[cfg(windows)]
type Session = ebms_vfs_winfsp::Mounted;
#[cfg(not(any(target_os = "linux", windows)))]
type Session = ();

/// 이 OS에서 가상 드라이브를 띄울 수 있는지
const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

#[derive(Clone, Debug, Default, Serialize)]
pub struct DriveInfo {
    pub supported: bool,
    pub mount_point: Option<PathBuf>,
    pub mounted: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Mount {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    session: Option<Session>,
    point: Option<PathBuf>,
    error: Option<String>,
}

/// 설정에 없으면 `$XDG_RUNTIME_DIR/ebms` (보통 `/run/user/<uid>/ebms`), 없으면 `~/ebms`.
/// 홈 안에 두면 백업·인덱서처럼 홈 전체를 훑는 프로그램이 플레이 파일을 읽게 된다.
#[cfg(not(windows))]
fn default_point() -> Option<PathBuf> {
    dirs::runtime_dir()
        .or_else(dirs::home_dir)
        .map(|d| d.join("ebms"))
}

/// 설정에 없으면 비어 있는 드라이브 문자 중 가장 뒤의 것.
#[cfg(windows)]
fn default_point() -> Option<PathBuf> {
    ebms_vfs_winfsp::free_drive_letter().map(PathBuf::from)
}

/// 설정 창에서 받은 마운트 위치를 확인한다. Windows는 `E`, `E:`, `E:\`를 모두 `E:`로.
pub fn parse_point(input: &str) -> Result<PathBuf, String> {
    let s = input.trim();
    if cfg!(windows) {
        let letter = s.trim_end_matches(['\\', '/']).trim_end_matches(':');
        if letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic() {
            return Ok(PathBuf::from(format!("{}:", letter.to_ascii_uppercase())));
        }
    }
    let path = PathBuf::from(s);
    if !path.is_absolute() {
        return Err("mount point must be a drive letter or an absolute path".into());
    }
    Ok(path)
}

impl Mount {
    /// 설정의 마운트 위치에 띄운다. 실패는 설정 창에만 보여준다.
    pub fn start(&self, hub: &Hub) {
        let configured = hub.config().mount_point;
        let point = configured.clone().or_else(default_point);
        // Windows는 고른 드라이브 문자를 저장해 둔다. 구동기에 등록한 곡 폴더 경로가 바뀌지 않도록.
        if cfg!(windows)
            && configured.is_none()
            && let Some(p) = &point
            && let Err(e) = hub.update_config(|c| c.mount_point = Some(p.clone()))
        {
            tracing::warn!(%e, "could not save mount point");
        }
        self.mount_at(hub, point);
    }

    /// 마운트 위치를 바꾸고 다시 띄운다.
    pub fn change(&self, hub: &Hub, point: PathBuf) -> ebms_core::Result<()> {
        hub.update_config(|c| c.mount_point = Some(point.clone()))?;
        self.mount_at(hub, Some(point));
        Ok(())
    }

    /// 앱을 끝낼 때 언마운트한다. 프로세스 종료로는 drop되지 않는다.
    pub fn stop(&self) {
        self.lock().session = None;
    }

    pub fn info(&self) -> DriveInfo {
        let inner = self.lock();
        DriveInfo {
            supported: SUPPORTED,
            mount_point: inner.point.clone(),
            mounted: inner.session.is_some(),
            error: inner.error.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn mount_at(&self, hub: &Hub, point: Option<PathBuf>) {
        let mut inner = self.lock();
        // 먼저 내린다 (drop하면 언마운트)
        inner.session = None;
        inner.point = point.clone();
        inner.error = None;
        if !SUPPORTED {
            return;
        }
        let Some(point) = point else {
            inner.error = Some("no mount point available, choose one".into());
            return;
        };
        match spawn(hub, &point) {
            Ok(session) => {
                tracing::info!(mountpoint = %point.display(), "mounted");
                inner.session = Some(session);
            }
            Err(e) => {
                tracing::warn!(mountpoint = %point.display(), %e, "mount failed");
                inner.error = Some(e.to_string());
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn(hub: &Hub, point: &Path) -> std::io::Result<Session> {
    ebms_vfs_fuse::spawn_mount(hub.drive(), point, hub.config().players())
}

#[cfg(windows)]
fn spawn(hub: &Hub, point: &Path) -> std::io::Result<Session> {
    ebms_vfs_winfsp::spawn_mount(hub.drive(), point, hub.config().players(), hub.runtime())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn spawn(_hub: &Hub, _point: &Path) -> std::io::Result<Session> {
    Err(std::io::Error::other("not supported on this OS"))
}
