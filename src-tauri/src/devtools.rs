//! 숨은 개발자 도구 창. 설정 창에서 F12를 누를 때만 열린다 (화면에 여는 버튼은 없다).
//!
//! 가상 드라이브에서 열리는 파일을 실시간으로 보여준다. 기록은 이 창이 떠 있는 동안만 쌓이고
//! 최근 [`ebms_core::openlog::CAPACITY`]개만 메모리에 둔다.

use ebms_core::openlog::OpenEntry;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::AppState;

pub const WINDOW: &str = "devtools";

/// 개발자 도구 창을 연다. 이미 열려 있으면 앞으로 가져온다.
/// Windows에서 동기 명령으로 창을 만들면 멈추므로 async로 둔다.
#[tauri::command]
pub async fn open_devtools(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }
    let built = WebviewWindowBuilder::new(&app, WINDOW, WebviewUrl::App("devtools.html".into()))
        .title("EBMS 개발자 도구")
        .inner_size(900.0, 560.0)
        .min_inner_size(480.0, 300.0)
        .build();
    match built {
        Ok(_) => {
            app.state::<AppState>().hub.open_log().set_enabled(true);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(%e, "could not open devtools window");
            Err(e.to_string())
        }
    }
}

/// 창이 닫히면 기록을 멈춘다.
pub fn on_closed(app: &AppHandle) {
    app.state::<AppState>().hub.open_log().set_enabled(false);
}

/// `after`번 뒤에 열린 파일
#[tauri::command]
pub fn devtools_opens(state: State<'_, AppState>, after: u64) -> Vec<OpenEntry> {
    state.hub.open_log().since(after)
}

#[tauri::command]
pub fn devtools_clear(state: State<'_, AppState>) {
    state.hub.open_log().clear();
}
