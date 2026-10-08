//! 가상 드라이브 마운트. 지금은 Linux(FUSE)만. Windows(WinFsp)는 별도 단계.

use std::path::PathBuf;
use std::sync::Mutex;

use ebms_core::hub::Hub;
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct DriveInfo {
    /// 이 OS에서 가상 드라이브를 띄울 수 있는지
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
    #[cfg(target_os = "linux")]
    session: Option<ebms_vfs_fuse::BackgroundSession>,
    point: Option<PathBuf>,
    error: Option<String>,
}

/// 설정에 없으면 `$XDG_RUNTIME_DIR/ebms` (보통 `/run/user/<uid>/ebms`), 없으면 `~/ebms`.
/// 홈 안에 두면 백업·인덱서처럼 홈 전체를 훑는 프로그램이 플레이 파일을 읽게 된다.
fn default_point() -> Option<PathBuf> {
    dirs::runtime_dir()
        .or_else(dirs::home_dir)
        .map(|d| d.join("ebms"))
}

impl Mount {
    /// 설정의 마운트 위치에 띄운다. 실패는 설정 창에만 보여준다.
    pub fn start(&self, hub: &Hub) {
        let point = hub.config().mount_point.or_else(default_point);
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
        #[cfg(target_os = "linux")]
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.session = None;
        }
    }

    pub fn info(&self) -> DriveInfo {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        DriveInfo {
            supported: cfg!(target_os = "linux"),
            mount_point: inner.point.clone(),
            #[cfg(target_os = "linux")]
            mounted: inner.session.is_some(),
            #[cfg(not(target_os = "linux"))]
            mounted: false,
            error: inner.error.clone(),
        }
    }

    #[cfg(target_os = "linux")]
    fn mount_at(&self, hub: &Hub, point: Option<PathBuf>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // 먼저 내린다 (drop하면 언마운트)
        inner.session = None;
        inner.point = point.clone();
        inner.error = None;
        let Some(point) = point else {
            inner.error = Some("no home folder, choose a mount point".into());
            return;
        };
        match ebms_vfs_fuse::spawn_mount(hub.drive(), &point, hub.config().players()) {
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

    #[cfg(not(target_os = "linux"))]
    fn mount_at(&self, _hub: &Hub, point: Option<PathBuf>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.point = point;
    }
}
