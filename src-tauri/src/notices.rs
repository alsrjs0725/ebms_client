//! 공지 창. 앱이 켜질 때 서버 공지를 확인해, 확인하지 않은 공지가 있으면 창을 띄운다.
//!
//! 서버에 닿지 않으면(켜진 직후 네트워크가 아직 안 붙은 경우 등) 조금 뒤에 몇 번 더 확인한다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ebms_core::hub::Hub;
use ebms_core::notice::{self, NoticeStore, ServerNotices};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::AppState;

const WINDOW: &str = "notices";
/// 공지를 기다리는 시간
const TIMEOUT: Duration = Duration::from_secs(10);
/// 서버에 닿지 않았을 때 다시 확인하기까지 기다리는 시간
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(120)];

pub struct Notices {
    store: NoticeStore,
    /// 창에 보여줄, 아직 확인하지 않은 공지
    pending: Mutex<Option<ServerNotices>>,
}

impl Notices {
    pub fn new(hub: &Hub) -> Self {
        Self {
            store: NoticeStore::new(hub.app_dir()),
            pending: Mutex::new(None),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<ServerNotices>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 앱이 켜질 때 한 번 부른다.
pub fn check_on_start(app: &AppHandle, hub: Arc<Hub>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(server) = hub.server() else {
            return;
        };
        for delay in std::iter::once(Duration::ZERO).chain(RETRY_DELAYS) {
            tokio::time::sleep(delay).await;
            let state = app.state::<AppState>();
            match notice::check(&state.notices.store, &server, TIMEOUT).await {
                Ok(found) => {
                    if found.is_some() {
                        *state.notices.pending() = found;
                        show(&app);
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(server = %server.entry.name, %e, "could not check notices")
                }
            }
        }
    });
}

/// 공지 창을 연다. 이미 열려 있으면 새 공지를 다시 읽게 한다.
fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.emit_to(WINDOW, "notices-changed", ());
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("notice.html".into()))
        .title("EBMS 공지")
        .inner_size(480.0, 420.0)
        .min_inner_size(360.0, 240.0)
        .build();
    if let Err(e) = built {
        tracing::warn!(%e, "could not open notice window");
    }
}

#[tauri::command]
pub fn pending_notices(state: State<'_, AppState>) -> Option<ServerNotices> {
    state.notices.pending().clone()
}

/// 창에 보여준 공지(`shown`)를 확인한 것으로 남긴다. 그 사이 새 공지가 오지 않았으면 창을 닫는다.
#[tauri::command]
pub fn ack_notices(
    app: AppHandle,
    state: State<'_, AppState>,
    shown: ServerNotices,
) -> Result<(), String> {
    let saved = state.notices.store.mark_seen(&shown);
    let mut pending = state.notices.pending();
    if let Some(p) = pending.as_mut() {
        p.notices.retain(|n| !shown.notices.contains(n));
    }
    pending.take_if(|p| p.notices.is_empty());
    let done = pending.is_none();
    drop(pending);
    if let Some(window) = app.get_webview_window(WINDOW) {
        if done {
            let _ = window.close();
        } else {
            let _ = window.emit_to(WINDOW, "notices-changed", ());
        }
    }
    saved.map_err(|e| e.to_string())
}
