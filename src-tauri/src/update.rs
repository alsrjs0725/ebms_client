//! 업데이트 창. 앱이 켜질 때 GitHub Releases에 새 정식 릴리스가 있으면 업데이트할지 묻는다.
//!
//! "업데이트"를 누르면 패키지를 받아 실행 파일 폴더의 파일을 바꾸고([`ebms_core::update::install`]),
//! 새 실행 파일을 `--after-update <pid>`로 띄운 뒤 끝난다. 새 프로세스는 이전 프로세스가 끝나기를
//! 기다렸다가(단일 인스턴스, 가상 드라이브) 뜬다. 실행 파일 옆 `data` 폴더는 건드리지 않는다.
//! 업데이트 확인은 Windows 릴리스 빌드에서만 한다.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use ebms_core::update::{self, Release};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

const WINDOW: &str = "update";
const APP_EXE: &str = if cfg!(windows) {
    "ebms-app.exe"
} else {
    "ebms-app"
};
/// 확인에 실패하면(켜진 직후 네트워크가 아직 안 붙은 경우 등) 다시 확인하기까지 기다리는 시간
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(120)];
/// 업데이트 뒤 새 프로세스가 이전 프로세스를 기다리는 최대 시간
const WAIT_OLD: Duration = Duration::from_secs(30);
const AFTER_UPDATE: &str = "--after-update";

/// 창에 보여줄 새 릴리스
#[derive(Default)]
pub struct Update {
    release: Mutex<Option<Release>>,
}

impl Update {
    fn release(&self) -> Option<Release> {
        self.release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
}

/// 앱을 띄우기 전에 부른다. 업데이트로 다시 실행된 것이면 이전 프로세스가 끝나기를 기다리고,
/// 지난 업데이트가 남긴 파일을 지운다.
pub fn finish_previous() {
    let mut args = std::env::args().skip_while(|a| a != AFTER_UPDATE).skip(1);
    if let Some(pid) = args.next().and_then(|p| p.parse::<u32>().ok()) {
        wait_for_exit(pid, WAIT_OLD);
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "updated");
    }
    if let Some(dir) = exe_dir() {
        update::cleanup(&dir);
    }
}

#[cfg(windows)]
fn wait_for_exit(pid: u32, timeout: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: 받은 핸들은 기다린 뒤 닫는다. 열지 못하면(이미 끝남) 그냥 넘어간다.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return;
        }
        WaitForSingleObject(handle, timeout.as_millis() as u32);
        CloseHandle(handle);
    }
}

#[cfg(not(windows))]
fn wait_for_exit(_pid: u32, _timeout: Duration) {}

/// 앱이 켜질 때 한 번 부른다.
pub fn check_on_start(app: &AppHandle) {
    if !cfg!(windows) || cfg!(debug_assertions) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        for delay in std::iter::once(Duration::ZERO).chain(RETRY_DELAYS) {
            tokio::time::sleep(delay).await;
            match update::check(env!("CARGO_PKG_VERSION")).await {
                Ok(Some(release)) => {
                    tracing::info!(version = %release.version, "update available");
                    *app.state::<Update>()
                        .release
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(release);
                    show(&app);
                    return;
                }
                Ok(None) => return,
                Err(e) => tracing::warn!(%e, "update check failed"),
            }
        }
    });
}

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("update.html".into()))
        .title("EBMS 업데이트")
        .inner_size(440.0, 360.0)
        .min_inner_size(340.0, 240.0)
        .build();
    if let Err(e) = built {
        tracing::warn!(%e, "could not open update window");
    }
}

/// 창에 보여줄 현재 버전과 새 릴리스
#[derive(serde::Serialize)]
pub struct UpdateInfo {
    current: &'static str,
    #[serde(flatten)]
    release: Release,
}

#[tauri::command]
pub fn update_info(state: State<'_, Update>) -> Option<UpdateInfo> {
    state.release().map(|release| UpdateInfo {
        current: env!("CARGO_PKG_VERSION"),
        release,
    })
}

#[tauri::command]
pub fn dismiss_update(app: AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.close();
    }
}

/// 새 릴리스를 받아 설치하고 새 실행 파일로 다시 시작한다. 성공하면 돌아오지 않는다(앱이 끝난다).
#[tauri::command]
pub async fn apply_update(app: AppHandle, state: State<'_, Update>) -> Result<(), String> {
    let release = state.release().ok_or("no update")?;
    let dir = exe_dir().ok_or("could not find the app folder")?;
    let data = update::download(&release)
        .await
        .map_err(|e| e.to_string())?;
    let install_dir = dir.clone();
    tauri::async_runtime::spawn_blocking(move || update::install(&data, &install_dir, APP_EXE))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    tracing::info!(version = %release.version, "update installed, restarting");
    std::process::Command::new(dir.join(APP_EXE))
        .arg(AFTER_UPDATE)
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|e| format!("installed, but could not restart: {e}"))?;
    // 가상 드라이브는 RunEvent::Exit에서 내린다.
    app.exit(0);
    Ok(())
}
