//! 앱과 설정 창.
//!
//! 설정 창이 떠 있는 동안만 돈다. 등록된 서버를 모두 동기화하고 가상 드라이브를 띄우며,
//! 설정 창을 닫으면(X) 가상 드라이브를 내리고 프로세스까지 끝난다. 상주·트레이·자동 시작은 없다.
//! 켜질 때 서버 공지 중 확인하지 않은 것이 있으면 공지 창을 띄운다([`notices`]).
//! 이미 떠 있을 때 다시 실행하면 떠 있는 설정 창을 앞으로 가져온다.
//! 앱 데이터는 실행 파일 옆 `data` 폴더에 둔다([`AppDir::default_root`]).
//! 로그는 콘솔과 앱 데이터 폴더의 `logs/`에 함께 남긴다([`logging`]).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod logging;
mod mount;
mod notices;
mod update;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ebms_core::config::AppDir;
use ebms_core::hub::Hub;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

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
    let (root, migrated) = match std::env::var_os("EBMS_DATA") {
        Some(d) => (Some(PathBuf::from(d)), Ok(false)),
        None => {
            let root = AppDir::default_root();
            let migrated = match (&root, AppDir::legacy_root()) {
                (Some(root), Some(old)) => AppDir::new(root).migrate_from(&old),
                _ => Ok(false),
            };
            (root, migrated)
        }
    };
    // 옮긴 뒤에 로그를 연다. 로그 폴더가 먼저 생기면 폴더째 옮기지 못한다.
    let log_dir = logging::init(root.as_deref());
    match migrated {
        Ok(true) => tracing::info!(?root, "moved app data from the old location"),
        Ok(false) => {}
        Err(e) => tracing::warn!(%e, "could not move app data from the old location"),
    }

    if let Err(e) = run(root, log_dir) {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

fn run(root: Option<PathBuf>, log_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    // 단일 인스턴스 확인 전에 업데이트 전 프로세스가 끝나기를 기다린다.
    update::finish_previous();
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
    let app = tauri::Builder::default()
        // 이미 떠 있으면 새로 실행한 쪽은 끝나고, 떠 있는 앱이 설정 창을 연다.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_settings(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            mount: mount::Mount::default(),
            notices: notices::Notices::new(&hub),
            hub: hub.clone(),
            log_dir,
        })
        .manage(update::Update::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_servers,
            commands::add_server,
            commands::remove_server,
            commands::clear_cache,
            commands::login,
            commands::logout,
            commands::open_account,
            commands::sync_now,
            commands::pre_tables,
            commands::set_pre_tables,
            commands::pending_download,
            commands::cache_limit_gb,
            commands::set_cache_limit_gb,
            commands::players,
            commands::add_custom_player,
            commands::remove_custom_player,
            commands::drive_info,
            commands::set_mount_point,
            commands::log_info,
            commands::open_log_dir,
            notices::pending_notices,
            notices::ack_notices,
            update::update_info,
            update::dismiss_update,
            update::apply_update,
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
            build_settings(app.handle())?;
            notices::check_on_start(app.handle(), state.hub.clone());
            update::check_on_start(app.handle());
            Ok(())
        })
        // 설정 창을 닫으면 앱을 끝낸다. 가상 드라이브는 아래 `RunEvent::Exit`에서 내린다.
        .on_window_event(|window, event| {
            if window.label() == SETTINGS && matches!(event, WindowEvent::Destroyed) {
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())?;

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            app.state::<AppState>().mount.stop();
        }
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
    if let Err(e) = build_settings(app) {
        tracing::warn!(%e, "could not open settings window");
    }
}

fn build_settings(app: &AppHandle) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("index.html".into()))
        .title("EBMS 설정")
        .inner_size(560.0, 720.0)
        .min_inner_size(420.0, 480.0)
        .build()
        .map(|_| ())
}
