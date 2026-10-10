//! 상주 앱과 설정 창.
//!
//! 등록된 서버를 모두 동기화하고 가상 드라이브를 띄운 채 상주한다. 알림·트레이 아이콘은 없다.
//! 켜질 때 서버 공지 중 확인하지 않은 것이 있으면 공지 창을 띄운다([`notices`]).
//! 설정 창은 앱을 실행할 때 열리고, 닫으면 웹뷰를 해제한다. 이미 떠 있을 때 다시 실행해도 열린다.
//! 자동 시작은 `--background`로 실행해 창 없이 뜬다(서버가 하나도 없으면 그래도 연다).
//! 로그는 콘솔과 앱 데이터 폴더의 `logs/`에 함께 남긴다([`logging`]).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod logging;
mod mount;
mod notices;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ebms_core::config::AppDir;
use ebms_core::hub::Hub;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

/// 자동 동기화 간격
const SYNC_INTERVAL: Duration = Duration::from_secs(30 * 60);
const SETTINGS: &str = "settings";

pub struct AppState {
    pub hub: Arc<Hub>,
    /// 파일 로그 폴더. 파일 로그를 못 열었으면 None
    pub log_dir: Option<PathBuf>,
    pub mount: mount::Mount,
    pub notices: notices::Notices,
}

fn main() {
    let root = match std::env::var_os("EBMS_DATA") {
        Some(d) => Some(PathBuf::from(d)),
        None => AppDir::default_root(),
    };
    let log_dir = logging::init(root.as_deref());

    if let Err(e) = run(root, log_dir) {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

fn run(root: Option<PathBuf>, log_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    // 코어(다운로드, 가상 FS)와 Tauri가 같은 tokio 런타임을 쓴다.
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new()?));
    tauri::async_runtime::set(rt.handle().clone());

    let root = root.ok_or("no app data folder, set EBMS_DATA")?;
    let use_keyring = std::env::var_os("EBMS_NO_KEYRING").is_none();
    let hub = Arc::new(Hub::open(
        AppDir::new(root),
        use_keyring,
        rt.handle().clone(),
    )?);
    let show_settings =
        hub.config().servers.is_empty() || !std::env::args().skip(1).any(|a| a == "--background");

    let app = tauri::Builder::default()
        // 이미 떠 있으면 새로 실행한 쪽은 끝나고, 떠 있는 앱이 설정 창을 연다.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_settings(app);
        }))
        .manage(AppState {
            mount: mount::Mount::default(),
            notices: notices::Notices::new(&hub),
            hub: hub.clone(),
            log_dir,
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_servers,
            commands::add_server,
            commands::remove_server,
            commands::clear_cache,
            commands::login,
            commands::logout,
            commands::open_account,
            commands::sync_now,
            commands::drive_info,
            commands::set_mount_point,
            commands::log_info,
            commands::open_log_dir,
            commands::quit,
            notices::pending_notices,
            notices::ack_notices,
        ])
        .setup(move |app| {
            let state = app.state::<AppState>();
            state.mount.start(&state.hub);
            let hub = hub.clone();
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(SYNC_INTERVAL);
                loop {
                    tick.tick().await;
                    hub.sync_all().await;
                }
            });
            if show_settings {
                open_settings(app.handle());
            }
            notices::check_on_start(app.handle(), state.hub.clone());
            Ok(())
        })
        .build(tauri::generate_context!())?;

    app.run(|app, event| match event {
        // 설정 창을 닫아도 앱은 계속 돈다. 종료는 설정 창의 "EBMS 종료"로만.
        RunEvent::ExitRequested { api, code, .. } if code.is_none() => api.prevent_exit(),
        RunEvent::Exit => app.state::<AppState>().mount.stop(),
        _ => {}
    });
    Ok(())
}

/// 설정 창을 연다. 이미 열려 있으면 앞으로 가져온다.
pub fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("index.html".into()))
        .title("EBMS 설정")
        .inner_size(560.0, 720.0)
        .min_inner_size(420.0, 480.0)
        .build();
    if let Err(e) = built {
        tracing::warn!(%e, "could not open settings window");
    }
}
