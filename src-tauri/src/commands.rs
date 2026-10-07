//! 설정 창이 부르는 명령. 오류는 문자열로 돌려 창에만 보여준다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ebms_core::Error;
use ebms_core::api::Me;
use ebms_core::hub::{Server, SyncStatus};
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::AppState;
use crate::mount::DriveInfo;

/// 브라우저 로그인을 기다리는 시간
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// 설정 창에서 계정 정보를 기다리는 시간. 꺼진 서버 때문에 창이 멈추지 않게.
const ME_TIMEOUT: Duration = Duration::from_secs(5);

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Serialize)]
pub struct ServerView {
    id: String,
    name: String,
    url: String,
    /// `logged_in` | `needs_login`(세션 만료) | `logged_out`
    login: &'static str,
    /// 계정, 연결된 OAuth, 남은 티켓, 이번 달 사전 다운로드
    me: Option<Me>,
    /// 서버에 닿지 않는 등 계정 정보를 못 가져온 이유
    error: Option<String>,
    sync: SyncStatus,
}

async fn view(server: Arc<Server>) -> ServerView {
    let api = server.api();
    let (me, error) = if api.has_session() {
        match tokio::time::timeout(ME_TIMEOUT, api.me()).await {
            Ok(Ok(me)) => (Some(me), None),
            Ok(Err(Error::Unauthorized)) => (None, None),
            Ok(Err(e)) => (None, Some(e.to_string())),
            Err(_) => (None, Some("server did not respond".into())),
        }
    } else {
        (None, None)
    };
    let login = if !api.has_session() {
        "logged_out"
    } else if api.needs_login() {
        "needs_login"
    } else {
        "logged_in"
    };
    ServerView {
        id: server.entry.id.clone(),
        name: server.entry.name.clone(),
        url: server.entry.url.clone(),
        login,
        me,
        error,
        sync: server.sync_status(),
    }
}

#[tauri::command]
pub async fn list_servers(state: State<'_, AppState>) -> CmdResult<Vec<ServerView>> {
    let jobs = state.hub.servers().into_iter().map(view);
    Ok(futures_util::future::join_all(jobs).await)
}

#[tauri::command]
pub async fn add_server(
    state: State<'_, AppState>,
    url: String,
    name: Option<String>,
) -> CmdResult<ServerView> {
    let server = state.hub.add(&url, name.as_deref()).await.map_err(err)?;
    Ok(view(server).await)
}

#[tauri::command]
pub async fn remove_server(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.hub.remove(&id).await.map_err(err)?;
    Ok(())
}

/// 브라우저로 로그인하고, 끝나면 바로 동기화를 시작한다.
#[tauri::command]
pub async fn login(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let hub = state.hub.clone();
    let device = ebms_core::auth::device_name("EBMS");
    hub.login(&id, &device, LOGIN_TIMEOUT, |url| {
        if let Err(e) = open::that_detached(url) {
            tracing::warn!(%e, url, "could not open browser");
        }
    })
    .await
    .map_err(err)?;
    let server = hub.server(&id).map_err(err)?;
    tauri::async_runtime::spawn(async move {
        if let Err(e) = server.sync().await {
            tracing::warn!(server = %server.entry.name, %e, "sync failed");
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn logout(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.hub.logout(&id).await.map_err(err)
}

/// OAuth 연결 관리는 웹 `/account`에서 한다.
#[tauri::command]
pub fn open_account(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let server = state.hub.server(&id).map_err(err)?;
    open::that_detached(format!("{}/account", server.entry.url)).map_err(err)
}

#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>, id: String) -> CmdResult<SyncStatus> {
    let server = state.hub.server(&id).map_err(err)?;
    server.sync().await.map_err(err)?;
    Ok(server.sync_status())
}

#[tauri::command]
pub fn drive_info(state: State<'_, AppState>) -> DriveInfo {
    state.mount.info()
}

#[tauri::command]
pub fn set_mount_point(state: State<'_, AppState>, path: String) -> CmdResult<DriveInfo> {
    let path = PathBuf::from(path.trim());
    if !path.is_absolute() {
        return Err("mount point must be an absolute path".into());
    }
    state.mount.change(&state.hub, path).map_err(err)?;
    Ok(state.mount.info())
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}
