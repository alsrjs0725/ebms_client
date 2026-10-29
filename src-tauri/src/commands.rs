//! 설정 창이 부르는 명령. 오류는 문자열로 돌려 창에 보여준다. 로그인·서버 추가 실패는 로그에도 남긴다.

use std::sync::Arc;
use std::time::Duration;

use ebms_core::Error;
use ebms_core::api::{DifficultyTable, Me};
use ebms_core::hub::{LinkedChart, LocalUsage, Server, SongHit, SyncStatus};
use ebms_core::personal::{PERSONAL_EXTS, PersonalChart};
use ebms_core::sync::PendingDownload;
use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

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

/// 연결한 서버. 없으면 창에 보여줄 오류.
fn server(state: &AppState) -> CmdResult<Arc<Server>> {
    state
        .hub
        .server()
        .ok_or_else(|| "서버를 먼저 추가해 주세요".into())
}

#[derive(Serialize)]
pub struct ServerView {
    name: String,
    url: String,
    /// 루프백이 아닌 `http://` 주소. 세션키가 평문으로 오가므로 경고한다.
    insecure: bool,
    /// `logged_in` | `needs_login`(세션 만료) | `logged_out`
    login: &'static str,
    /// 계정, 연결된 OAuth, 남은 티켓, 이번 달 사전 다운로드
    me: Option<Me>,
    /// 서버에 닿지 않는 등 계정 정보를 못 가져온 이유
    error: Option<String>,
    sync: SyncStatus,
    /// 로컬 저장 공간. 못 읽으면 None
    usage: Option<LocalUsage>,
    /// 키체인을 못 써서 세션키를 평문 파일에 저장했음
    session_in_file: bool,
    /// 사전 다운로드할 난이도표 id. None이면 전곡
    pre_tables: Option<Vec<u32>>,
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
    let usage = {
        let server = server.clone();
        match tokio::task::spawn_blocking(move || server.usage()).await {
            Ok(Ok(u)) => Some(u),
            Ok(Err(e)) => {
                tracing::warn!(%e, "could not read local usage");
                None
            }
            Err(_) => None,
        }
    };
    ServerView {
        name: server.entry.name.clone(),
        url: server.entry.url.clone(),
        insecure: server.entry.is_insecure(),
        login,
        me,
        error,
        sync: server.sync_status(),
        usage,
        session_in_file: api.has_session() && server.session_in_file(),
        pre_tables: server.pre_tables().unwrap_or_else(|e| {
            tracing::warn!(%e, "could not read pre download tables");
            None
        }),
    }
}

/// 연결한 서버. 없으면 None.
#[tauri::command]
pub async fn server_info(state: State<'_, AppState>) -> CmdResult<Option<ServerView>> {
    Ok(match state.hub.server() {
        Some(server) => Some(view(server).await),
        None => None,
    })
}

#[tauri::command]
pub async fn add_server(
    state: State<'_, AppState>,
    url: String,
    name: Option<String>,
) -> CmdResult<ServerView> {
    let server = state.hub.add(&url, name.as_deref()).await.map_err(|e| {
        tracing::warn!(url, %e, "could not add server");
        err(e)
    })?;
    Ok(view(server).await)
}

#[tauri::command]
pub async fn remove_server(state: State<'_, AppState>, purge: bool) -> CmdResult<()> {
    state.hub.remove(purge).await.map_err(err)?;
    Ok(())
}

/// 서버의 캐시를 비운다. 지운 바이트 수.
#[tauri::command]
pub async fn clear_cache(state: State<'_, AppState>) -> CmdResult<u64> {
    let server = server(&state)?;
    server.clear_cache().await.map_err(err)
}

/// 브라우저로 로그인하고, 끝나면 바로 동기화를 시작한다.
#[tauri::command]
pub async fn login(state: State<'_, AppState>) -> CmdResult<()> {
    let hub = state.hub.clone();
    hub.login(LOGIN_TIMEOUT, |url| {
        if let Err(e) = open::that_detached(url) {
            tracing::warn!(%e, url, "could not open browser");
        }
    })
    .await
    .map_err(|e| {
        tracing::warn!(%e, "login failed");
        err(e)
    })?;
    tauri::async_runtime::spawn(async move { hub.sync().await });
    Ok(())
}

#[tauri::command]
pub async fn logout(state: State<'_, AppState>) -> CmdResult<()> {
    state.hub.logout().await.map_err(err)
}

/// OAuth 연결 관리는 웹 `/account`에서 한다.
#[tauri::command]
pub fn open_account(state: State<'_, AppState>) -> CmdResult<()> {
    let server = server(&state)?;
    open::that_detached(format!("{}/account", server.entry.url)).map_err(err)
}

#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>) -> CmdResult<SyncStatus> {
    let server = server(&state)?;
    server.sync().await.map_err(err)?;
    Ok(server.sync_status())
}

#[derive(Serialize)]
pub struct PreTablesView {
    tables: Vec<DifficultyTable>,
    /// 고른 난이도표 id. None이면 전곡
    selected: Option<Vec<u32>>,
}

/// 서버의 난이도표 목록과 지금 고른 사전 다운로드 범위.
#[tauri::command]
pub async fn pre_tables(state: State<'_, AppState>) -> CmdResult<PreTablesView> {
    let server = server(&state)?;
    let tables = server.api().pre_tables().await.map_err(err)?;
    Ok(PreTablesView {
        tables: tables.tables,
        selected: server.pre_tables().map_err(err)?,
    })
}

/// 사전 다운로드 범위를 바꾸고 바로 동기화한다. `selected`가 None이면 전곡.
#[tauri::command]
pub async fn set_pre_tables(
    state: State<'_, AppState>,
    selected: Option<Vec<u32>>,
) -> CmdResult<SyncStatus> {
    let server = server(&state)?;
    server.set_pre_tables(selected.as_deref()).map_err(err)?;
    server.sync().await.map_err(err)?;
    Ok(server.sync_status())
}

/// 연결한 개인 채보 목록.
#[tauri::command]
pub async fn personal_charts(state: State<'_, AppState>) -> CmdResult<Vec<LinkedChart>> {
    let server = server(&state)?;
    tokio::task::spawn_blocking(move || server.personal_charts())
        .await
        .map_err(err)?
        .map_err(err)
}

/// 개인 채보를 연결할 곡 찾기.
#[tauri::command]
pub async fn search_songs(state: State<'_, AppState>, query: String) -> CmdResult<Vec<SongHit>> {
    let server = server(&state)?;
    tokio::task::spawn_blocking(move || server.search_songs(&query))
        .await
        .map_err(err)?
        .map_err(err)
}

/// 파일 선택 창에서 채보를 골라(여러 개 가능) `song_id` 곡에 연결한다. 취소하면 빈 목록.
/// 서버에는 올리지 않고, 가상 드라이브의 그 곡 폴더에 바로 보인다.
#[tauri::command]
pub async fn add_personal_charts(
    state: State<'_, AppState>,
    window: tauri::WebviewWindow,
    song_id: u32,
) -> CmdResult<Vec<PersonalChart>> {
    let server = server(&state)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title("개인 채보 선택")
        .set_parent(&window)
        .add_filter("BMS 채보", PERSONAL_EXTS)
        .pick_files(move |files| {
            let _ = tx.send(files);
        });
    let Some(files) = rx.await.map_err(err)? else {
        return Ok(Vec::new());
    };
    let files = files
        .into_iter()
        .map(|f| f.into_path().map_err(err))
        .collect::<CmdResult<Vec<_>>>()?;
    server
        .add_personal_charts(song_id, files)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn remove_personal_chart(
    state: State<'_, AppState>,
    song_id: u32,
    name: String,
) -> CmdResult<()> {
    let server = server(&state)?;
    server
        .remove_personal_chart(song_id, name)
        .await
        .map_err(err)?;
    Ok(())
}

/// 지금 동기화하면 받을 용량.
/// 사전 청크는 난이도표 태그와 함께 줘서 창에서 고르는 표에 맞춰 바로 다시 센다.
#[tauri::command]
pub async fn pending_download(state: State<'_, AppState>) -> CmdResult<PendingDownload> {
    let server = server(&state)?;
    server.client.pending_download().await.map_err(err)
}

/// 캐시 한도(GB).
#[tauri::command]
pub fn cache_limit_gb(state: State<'_, AppState>) -> u64 {
    state.hub.config().cache_limit_gb()
}

#[tauri::command]
pub async fn set_cache_limit_gb(state: State<'_, AppState>, gb: u64) -> CmdResult<u64> {
    state.hub.set_cache_limit_gb(gb).await.map_err(err)?;
    Ok(gb)
}

#[derive(Serialize)]
pub struct PlayersView {
    /// 이름으로 알아보는 기본 구동기
    defaults: &'static [&'static str],
    /// 사용자가 고른 실행 파일 (전체 경로)
    custom: Vec<String>,
}

#[tauri::command]
pub fn players(state: State<'_, AppState>) -> PlayersView {
    PlayersView {
        defaults: ebms_core::players::DEFAULT_PLAYERS,
        custom: state.hub.config().custom_players,
    }
}

/// 파일 선택 창에서 구동기 exe를 골라 추가한다. 고른 파일을 돌려주고, 취소하면 None.
/// 마운트한 드라이브에 바로 적용된다.
#[tauri::command]
pub async fn add_custom_player(
    state: State<'_, AppState>,
    window: tauri::WebviewWindow,
) -> CmdResult<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title("구동기 실행 파일 선택")
        .set_parent(&window)
        .add_filter("실행 파일", &["exe"])
        .pick_file(move |file| {
            let _ = tx.send(file);
        });
    let Some(file) = rx.await.map_err(err)? else {
        return Ok(None);
    };
    let path = file.into_path().map_err(err)?;
    let path = path.to_string_lossy().into_owned();
    let mut result = Ok(true);
    state
        .hub
        .update_config(|c| result = c.add_custom_player(&path))
        .map_err(err)?;
    result.map_err(err)?;
    Ok(Some(path))
}

#[tauri::command]
pub fn remove_custom_player(state: State<'_, AppState>, path: String) -> CmdResult<()> {
    state
        .hub
        .update_config(|c| {
            c.remove_custom_player(&path);
        })
        .map_err(err)?;
    Ok(())
}

#[tauri::command]
pub fn drive_info(state: State<'_, AppState>) -> DriveInfo {
    state.mount.info()
}

#[tauri::command]
pub fn set_mount_point(state: State<'_, AppState>, path: String) -> CmdResult<DriveInfo> {
    let path = crate::mount::parse_point(&path)?;
    state.mount.change(&state.hub, path).map_err(err)?;
    Ok(state.mount.info())
}

/// 파일 로그 폴더. 파일 로그를 못 열었으면 None.
#[tauri::command]
pub fn log_info(state: State<'_, AppState>) -> Option<String> {
    state
        .log_dir
        .as_ref()
        .map(|d| d.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn open_log_dir(state: State<'_, AppState>) -> CmdResult<()> {
    let dir = state
        .log_dir
        .as_ref()
        .ok_or("파일 로그를 쓰지 않고 있습니다")?;
    open::that_detached(dir).map_err(err)
}
