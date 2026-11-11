//! 연결한 서버(하나)를 관리한다. 상주 앱이 쓴다.
//!
//! 서버의 [`Client`]와 가상 FS를 열어 [`Drive`]에 서버 이름 폴더로 넣는다.
//! 서버 추가·삭제, 로그인·로그아웃, 동기화를 한다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::api::{Api, Me, User};
use crate::auth::LoginRequest;
use crate::config::{AppDir, Config, ServerEntry, is_insecure_url};
use crate::drive::Drive;
use crate::fs::EbmsFs;
use crate::openlog::OpenLog;
use crate::personal::PersonalChart;
use crate::players::Players;
use crate::session::SessionStore;
use crate::sync::SyncReport;
use crate::table::UserTable;
use crate::{Client, Error, Options, Result};

/// 서버 하나.
pub struct Server {
    pub entry: ServerEntry,
    pub client: Client,
    pub fs: Arc<EbmsFs>,
    status: Mutex<SyncStatus>,
    /// 같은 서버의 동기화가 겹치지 않게
    sync_lock: tokio::sync::Mutex<()>,
    /// 설정에서 빠짐. 이미 시작한 동기화 묶음이 지운 폴더에 다시 쓰지 않게 한다.
    removed: AtomicBool,
    /// 고른 난이도표의 곡 받기 상태
    table_status: Mutex<TableDownload>,
    /// 곡 받기를 멈춰 달라는 요청
    table_stop: AtomicBool,
}

/// 고른 난이도표의 곡 받기 진행 상태. 설정 창에 보여준다.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TableDownload {
    pub running: bool,
    /// 이번에 받을 곡 수 (이미 받은 곡 제외)
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    /// 아직 동기화하지 않아 목록에 없는 곡 (다음 동기화 뒤 다시 받기)
    pub not_synced: u32,
    /// 티켓을 기다리는 중이면 다시 시도할 시각 (unix 초)
    pub waiting_until: Option<i64>,
    pub last_error: Option<String>,
}

/// 곡 받기가 남겨 두는 티켓 수. 구동기가 새 곡을 바로 열 수 있게 한다.
const TABLE_TICKET_RESERVE: u32 = 1;

/// 이 서버가 쓰는 로컬 저장 공간. 설정 창에 보여준다.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LocalUsage {
    /// 받은 에셋 캐시
    pub cache_bytes: u64,
    /// 캐시 한도
    pub cache_limit: u64,
    /// 캐시 + 인덱스 + 차트. 서버를 지울 때 함께 지울 수 있는 양
    pub total_bytes: u64,
}

/// 곡 검색 결과 하나. 개인 채보를 연결할 곡을 고를 때 쓴다.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SongHit {
    pub song_id: u32,
    /// 가상 드라이브에 보이는 곡 폴더 이름
    pub dir: String,
}

/// 연결한 개인 채보와 그 곡 폴더 이름. 곡이 서버에서 사라졌으면 `dir`이 None.
#[derive(Clone, Debug, Serialize)]
pub struct LinkedChart {
    #[serde(flatten)]
    pub chart: PersonalChart,
    pub dir: Option<String>,
}

/// 곡 검색 결과 최대 개수.
pub const SONG_SEARCH_LIMIT: usize = 50;

/// 마지막 동기화 결과. 설정 창에만 보여준다.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncStatus {
    /// 마지막으로 성공한 시각(unix 초). 앱을 다시 켜도 남게 서버 폴더에 적어 둔다.
    pub last_ok_at: Option<i64>,
    /// 마지막 시도가 실패했으면 그 오류
    pub last_error: Option<String>,
    /// 지금 동기화 중
    pub running: bool,
}

/// 마지막 동기화 성공 시각을 적어 두는 파일 (서버 폴더 안)
const LAST_SYNC_FILE: &str = "last_sync";

fn load_last_sync(server_dir: &std::path::Path) -> Option<i64> {
    std::fs::read_to_string(server_dir.join(LAST_SYNC_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn save_last_sync(server_dir: &std::path::Path, at: i64) {
    if let Err(e) = std::fs::write(server_dir.join(LAST_SYNC_FILE), at.to_string()) {
        warn!(%e, "could not save last sync time");
    }
}

impl Server {
    pub fn api(&self) -> &Api {
        &self.client.api
    }

    /// 키체인을 쓰지 못해 세션키가 평문 파일에 있는지. 설정 창에 경고로 보여준다.
    pub fn session_in_file(&self) -> bool {
        crate::session::session_file(&self.client.paths.root).is_file()
    }

    pub fn sync_status(&self) -> SyncStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 로컬 저장 공간. 폴더를 훑으므로 런타임 밖(`spawn_blocking`)에서 부른다.
    pub fn usage(&self) -> Result<LocalUsage> {
        let cache = self.client.fetcher.cache();
        let cache_bytes = cache.total()?;
        Ok(LocalUsage {
            cache_bytes,
            cache_limit: cache.limit(),
            total_bytes: cache_bytes + self.client.paths.size_without_cache(),
        })
    }

    /// 캐시를 비운다 ("항상 보관" 곡은 남김). 지운 바이트 수.
    pub async fn clear_cache(&self) -> Result<u64> {
        let cache = self.client.fetcher.cache().clone();
        tokio::task::spawn_blocking(move || cache.clear())
            .await
            .map_err(|e| Error::Other(e.to_string()))?
    }

    /// 사전 다운로드할 난이도표(서버의 표 id). `None`이면 전곡.
    pub fn pre_tables(&self) -> Result<Option<Vec<u32>>> {
        self.client.index.pre_tables()
    }

    /// 사전 다운로드할 난이도표를 정한다. 다음 동기화부터 고른 표의 곡이 든 사전 청크만 받는다.
    pub fn set_pre_tables(&self, tables: Option<&[u32]>) -> Result<()> {
        self.client.index.set_pre_tables(tables)
    }

    // ---- 고른 난이도표의 곡 받기 ----

    /// 고른 난이도표. 인덱스를 읽으므로 런타임 밖에서 불러도 된다(작음).
    pub fn user_tables(&self) -> Result<Vec<UserTable>> {
        self.client.index.user_tables()
    }

    /// 표 주소를 이 PC가 읽고 해시만 서버에 보내 곡을 맞춘다. 같은 주소가 있으면 바꾼다.
    pub async fn add_user_table(&self, url: &str) -> Result<UserTable> {
        let table = self.check_user_table(url.trim()).await?;
        let mut tables = self.user_tables()?;
        match tables.iter_mut().find(|t| t.url == table.url) {
            Some(t) => *t = table.clone(),
            None => tables.push(table.clone()),
        }
        self.client.index.set_user_tables(&tables)?;
        self.apply_table_pins()?;
        self.reload_if_filtered().await;
        Ok(table)
    }

    /// 고른 표를 모두 다시 읽어 곡을 맞춘다. 읽지 못한 표는 이전 결과를 둔다. 실패한 표의 오류 목록.
    pub async fn refresh_user_tables(&self) -> Result<Vec<String>> {
        let mut tables = self.user_tables()?;
        let mut errors = Vec::new();
        for t in &mut tables {
            match self.check_user_table(&t.url).await {
                Ok(new) => *t = new,
                Err(e) => {
                    warn!(url = %t.url, %e, "difficulty table refresh failed");
                    errors.push(format!("{}: {e}", t.name));
                }
            }
        }
        // 읽는 동안 지운 표는 되살리지 않는다.
        let current: std::collections::HashSet<String> =
            self.user_tables()?.into_iter().map(|t| t.url).collect();
        tables.retain(|t| current.contains(&t.url));
        self.client.index.set_user_tables(&tables)?;
        self.apply_table_pins()?;
        self.reload_if_filtered().await;
        Ok(errors)
    }

    /// 표를 빼고, 다른 표에 없는 곡의 "항상 보관"을 푼다(캐시 한도에 따라 지워질 수 있게).
    pub async fn remove_user_table(&self, url: &str) -> Result<()> {
        let mut tables = self.user_tables()?;
        tables.retain(|t| t.url != url);
        self.client.index.set_user_tables(&tables)?;
        self.apply_table_pins()?;
        self.reload_if_filtered().await;
        Ok(())
    }

    /// 사전 다운로드 표를 골라 곡 목록을 거르고 있으면, 고른 표의 곡이 바뀌었으니 가상 트리를 다시 만든다.
    async fn reload_if_filtered(&self) {
        if matches!(self.client.index.pre_tables(), Ok(Some(_))) {
            self.reload().await;
        }
    }

    async fn check_user_table(&self, url: &str) -> Result<UserTable> {
        let http = crate::table::http_client()?;
        let hashes = crate::table::fetch(&http, url).await?;
        let matched = self.api().match_table(&hashes.md5, &hashes.sha256).await?;
        info!(
            url,
            name = hashes.name,
            charts = hashes.charts,
            songs = matched.songs.len(),
            "difficulty table matched"
        );
        Ok(UserTable {
            url: url.to_string(),
            name: hashes.name,
            symbol: hashes.symbol,
            charts: hashes.charts as u32,
            matched_charts: (matched.matched_md5.len() + matched.matched_sha256.len())
                .min(hashes.charts) as u32,
            bytes: matched.songs.iter().map(|s| s.size).sum(),
            songs: matched.songs.into_iter().map(|s| s.id).collect(),
            checked_at: unix_now(),
        })
    }

    /// 고른 표들의 곡 (겹치는 곡은 한 번).
    fn table_songs(&self) -> Result<std::collections::BTreeSet<u32>> {
        Ok(self
            .user_tables()?
            .into_iter()
            .flat_map(|t| t.songs)
            .collect())
    }

    /// 고른 표의 곡만 캐시에서 "항상 보관"으로 둔다.
    fn apply_table_pins(&self) -> Result<()> {
        self.client.index.cache_pin_only(&self.table_songs()?)
    }

    pub fn table_download(&self) -> TableDownload {
        self.table_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 곡 받기를 멈춘다. 지금 받는 곡은 끝까지 받는다.
    pub fn stop_table_download(&self) {
        self.table_stop.store(true, Ordering::SeqCst);
    }

    fn set_table_status(&self, f: impl FnOnce(&mut TableDownload)) {
        f(&mut self.table_status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// 고른 표의 곡 중 캐시에 없는 곡을 하나씩 받는다(곡마다 티켓 1개). 티켓이 모자라면 찰 때까지 기다리고,
    /// 구동기가 바로 쓸 수 있게 [`TABLE_TICKET_RESERVE`]개는 남긴다. 받은 곡은 "항상 보관"으로 둔다.
    /// 이미 돌고 있으면 아무것도 하지 않는다.
    pub async fn download_tables(&self) -> Result<()> {
        {
            let mut st = self.table_status.lock().unwrap_or_else(|e| e.into_inner());
            if st.running {
                return Ok(());
            }
            *st = TableDownload {
                running: true,
                ..Default::default()
            };
        }
        self.table_stop.store(false, Ordering::SeqCst);
        let result = self.download_tables_inner().await;
        self.set_table_status(|st| {
            st.running = false;
            st.waiting_until = None;
            if let Err(e) = &result {
                st.last_error = Some(e.to_string());
            }
        });
        if let Err(e) = &result {
            warn!(server = %self.entry.name, %e, "table download stopped");
        }
        result
    }

    async fn download_tables_inner(&self) -> Result<()> {
        self.apply_table_pins()?;
        let tree = self.fs.tree();
        let cache = self.client.fetcher.cache().clone();
        let mut todo = Vec::new();
        let mut not_synced = 0;
        for id in self.table_songs()? {
            match tree.song(id) {
                Some(song)
                    if !song
                        .files
                        .iter()
                        .all(|f| cache.contains(id, &f.path, f.size)) =>
                {
                    todo.push(song)
                }
                Some(_) => {}
                None => not_synced += 1,
            }
        }
        self.set_table_status(|st| {
            st.total = todo.len() as u32;
            st.not_synced = not_synced;
        });
        info!(songs = todo.len(), not_synced, "table download started");
        for song in todo {
            loop {
                if self.table_stop.load(Ordering::SeqCst) || self.removed.load(Ordering::SeqCst) {
                    info!("table download stopped by user");
                    return Ok(());
                }
                // 남길 티켓만 있으면 다음 티켓이 찰 때까지 기다린다.
                let me = self.api().me().await?;
                let wait = if me.tickets.available <= TABLE_TICKET_RESERVE {
                    Some(
                        me.tickets
                            .next_refill_in
                            .map_or(me.tickets.refill_seconds, |s| s.max(1) as u64),
                    )
                } else {
                    match self
                        .client
                        .fetcher
                        .ensure_song(&song, "ebms (table download)", "")
                        .await
                    {
                        Ok(()) => {
                            cache.pin_song(song.song_id, true)?;
                            self.set_table_status(|st| st.done += 1);
                            break;
                        }
                        Err(Error::NoTicket { retry_after }) => Some(retry_after),
                        Err(e @ Error::Unauthorized) => return Err(e),
                        Err(e) => {
                            warn!(song_id = song.song_id, %e, "table download: song failed");
                            self.set_table_status(|st| {
                                st.failed += 1;
                                st.last_error = Some(format!("곡 {}: {e}", song.song_id));
                            });
                            break;
                        }
                    }
                };
                if let Some(secs) = wait {
                    self.wait_ticket(secs).await;
                }
            }
        }
        Ok(())
    }

    /// `secs`초(1초 이상) 기다린다. 멈추라는 요청이 오면 바로 돌아온다.
    async fn wait_ticket(&self, secs: u64) {
        let until = unix_now() + secs.max(1) as i64;
        self.set_table_status(|st| st.waiting_until = Some(until));
        while unix_now() < until && !self.table_stop.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.set_table_status(|st| st.waiting_until = None);
    }

    /// 곡 폴더 이름(`00123 Artist - Title`)에 `query`가 든 곡. 대소문자를 무시하고, 빈칸으로 나눈 낱말이 모두
    /// 들어 있어야 한다. 최대 [`SONG_SEARCH_LIMIT`]곡. 인덱스를 읽으므로 런타임 밖에서 부른다.
    pub fn search_songs(&self, query: &str) -> Result<Vec<SongHit>> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self
            .client
            .index
            .song_folders()?
            .into_iter()
            .map(|(song_id, folder)| SongHit {
                song_id,
                dir: crate::tree::song_dir_name(song_id, &folder),
            })
            .filter(|h| {
                let dir = h.dir.to_lowercase();
                words.iter().all(|w| dir.contains(w.as_str()))
            })
            .take(SONG_SEARCH_LIMIT)
            .collect();
        Ok(hits)
    }

    /// 연결한 개인 채보 전체. 인덱스를 읽으므로 런타임 밖에서 부른다.
    pub fn personal_charts(&self) -> Result<Vec<LinkedChart>> {
        let index = &self.client.index;
        let dirs: std::collections::HashMap<u32, String> =
            index.song_folders()?.into_iter().collect();
        Ok(index
            .personal_list()?
            .into_iter()
            .map(|chart| LinkedChart {
                dir: dirs
                    .get(&chart.song_id)
                    .map(|f| crate::tree::song_dir_name(chart.song_id, f)),
                chart,
            })
            .collect())
    }

    /// 채보 파일을 복사해 곡에 연결하고 가상 트리를 다시 만든다. 서버에는 올리지 않는다.
    pub async fn add_personal_charts(
        &self,
        song_id: u32,
        files: Vec<std::path::PathBuf>,
    ) -> Result<Vec<PersonalChart>> {
        let (paths, index) = (self.client.paths.clone(), self.client.index.clone());
        let added = tokio::task::spawn_blocking(move || {
            files
                .iter()
                .map(|f| crate::personal::add(&paths, &index, song_id, f))
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()));
        // 일부만 연결됐어도 보이도록 실패와 상관없이 다시 만든다.
        self.reload().await;
        added?
    }

    /// 개인 채보 연결을 끊고 가상 트리를 다시 만든다.
    pub async fn remove_personal_chart(&self, song_id: u32, name: String) -> Result<bool> {
        let (paths, index) = (self.client.paths.clone(), self.client.index.clone());
        let removed = tokio::task::spawn_blocking(move || {
            crate::personal::remove(&paths, &index, song_id, &name)
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))
        .and_then(|r| r)?;
        self.reload().await;
        Ok(removed)
    }

    async fn reload(&self) {
        let fs = self.fs.clone();
        let reloaded = tokio::task::spawn_blocking(move || fs.reload())
            .await
            .map_err(|e| Error::Other(e.to_string()))
            .and_then(|r| r);
        if let Err(e) = reloaded {
            warn!(server = %self.entry.name, %e, "reload failed");
        }
    }

    /// 동기화하고 바뀐 게 있으면 가상 트리를 다시 만든다.
    pub async fn sync(&self) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().await;
        if self.removed.load(Ordering::SeqCst) {
            return Err(Error::Other(format!(
                "server {} was removed",
                self.entry.name
            )));
        }
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running = true;
        let result = self.client.sync().await;
        // 인덱스를 읽으므로 런타임 밖에서, 설정 창이 기다리지 않게 상태 잠금 밖에서.
        // 실패해도 일부 청크는 바뀌었을 수 있다. 다음 동기화가 옛 청크 판을 지우기 전에 다시 만든다.
        if !matches!(&result, Ok(report) if !report.changed()) {
            self.reload().await;
        }
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.running = false;
        let saved = match &result {
            Ok(_) => {
                let now = unix_now();
                status.last_ok_at = Some(now);
                status.last_error = None;
                Some(now)
            }
            Err(e) => {
                warn!(server = %self.entry.name, %e, "sync failed");
                status.last_error = Some(e.to_string());
                None
            }
        };
        drop(status);
        // 지운 서버의 폴더를 다시 만들지 않게
        if let Some(at) = saved
            && !self.removed.load(Ordering::SeqCst)
        {
            save_last_sync(&self.client.paths.root, at);
        }
        result
    }
}

struct State {
    config: Config,
    server: Option<Arc<Server>>,
}

pub struct Hub {
    dir: AppDir,
    use_keyring: bool,
    rt: Handle,
    drive: Arc<Drive>,
    /// 마운트한 드라이브가 보는 구동기 목록. 설정을 바꾸면 같이 바꾼다.
    players: Players,
    /// 개발자 도구에 보여줄 파일 열기 기록
    open_log: OpenLog,
    state: Mutex<State>,
}

impl Hub {
    /// 설정을 읽고 연결한 서버를 연다. 열지 못하면 로그만 남기고 서버 없이 연다.
    /// 키체인을 읽으므로 tokio 런타임 밖에서 부른다. `rt`는 다운로드를 돌릴 런타임.
    pub fn open(dir: AppDir, use_keyring: bool, rt: Handle) -> Result<Self> {
        let config = dir.load_config()?;
        dir.purge_pending();
        let hub = Self {
            dir,
            use_keyring,
            rt,
            drive: Arc::new(Drive::new()),
            players: Players::new(config.players()),
            open_log: OpenLog::default(),
            state: Mutex::new(State {
                config: config.clone(),
                server: None,
            }),
        };
        if let Some(entry) = &config.server {
            match open_server(&hub.dir, use_keyring, &hub.rt, entry, config.cache_limit()) {
                Ok(server) => hub.insert(server),
                Err(e) => warn!(server = %entry.name, %e, "could not open server"),
            }
        }
        Ok(hub)
    }

    fn insert(&self, server: Arc<Server>) {
        self.drive
            .set(&server.entry.id, &server.entry.name, server.fs.clone());
        self.lock().server = Some(server);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn app_dir(&self) -> &AppDir {
        &self.dir
    }

    /// 다운로드를 돌리는 런타임.
    pub fn runtime(&self) -> Handle {
        self.rt.clone()
    }

    /// 서버 이름 폴더를 둔 가상 드라이브.
    pub fn drive(&self) -> Arc<Drive> {
        self.drive.clone()
    }

    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    /// 곡 전체를 받을 수 있는 프로그램. 마운트에 넘기면 설정을 바꿀 때 바로 따라간다.
    pub fn players(&self) -> Players {
        self.players.clone()
    }

    /// 가상 드라이브의 파일 열기 기록. 개발자 도구 창이 떠 있을 때만 쌓인다.
    pub fn open_log(&self) -> OpenLog {
        self.open_log.clone()
    }

    /// 설정을 바꿔 저장한다.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut state = self.lock();
        let mut config = state.config.clone();
        f(&mut config);
        self.dir.save_config(&config)?;
        self.players.set(config.players());
        state.config = config.clone();
        Ok(config)
    }

    /// 캐시 한도(GB)를 바꿔 저장하고, 줄었으면 넘는 만큼 오래 안 쓴 곡부터 지운다.
    pub async fn set_cache_limit_gb(&self, gb: u64) -> Result<()> {
        if gb == 0 {
            return Err(Error::Config("cache limit must be at least 1 GB".into()));
        }
        let limit = self
            .update_config(|c| c.cache_limit_gb = Some(gb))?
            .cache_limit();
        if let Some(server) = self.server() {
            let cache = server.client.fetcher.cache().clone();
            cache.set_limit(limit);
            let evicted = tokio::task::spawn_blocking(move || cache.evict())
                .await
                .map_err(|e| Error::Other(e.to_string()))
                .and_then(|r| r);
            if let Err(e) = evicted {
                warn!(server = %server.entry.name, %e, "cache eviction failed");
            }
        }
        Ok(())
    }

    /// 연결한 서버. 없으면 None.
    pub fn server(&self) -> Option<Arc<Server>> {
        self.lock().server.clone()
    }

    fn require(&self) -> Result<Arc<Server>> {
        self.server()
            .ok_or_else(|| Error::Config("no server added yet".into()))
    }

    pub fn session(&self, entry: &ServerEntry) -> SessionStore {
        SessionStore::new(&entry.url, &self.dir.server_dir(entry), self.use_keyring)
    }

    /// `/api/version`으로 EBMS 서버인지 확인하고 연결한다. 이미 연결한 서버가 있으면 오류(먼저 뺀다).
    pub async fn add(&self, url: &str, name: Option<&str>) -> Result<Arc<Server>> {
        let mut config = self.config();
        let entry = config.set_server(url, name)?.clone();
        let url = entry.url.clone();
        if is_insecure_url(&url) {
            warn!(%url, "adding an unencrypted http server, session key will be sent in plain text");
        }
        let version = Api::new(&url)?
            .version()
            .await
            .map_err(|e| Error::Config(format!("{url} is not a reachable EBMS server ({e})")))?;
        if version.api != crate::API_VERSION {
            return Err(Error::ApiVersion(version.api));
        }
        let server = {
            let (dir, rt, entry) = (self.dir.clone(), self.rt.clone(), entry.clone());
            let use_keyring = self.use_keyring;
            let cache_limit = config.cache_limit();
            tokio::task::spawn_blocking(move || {
                open_server(&dir, use_keyring, &rt, &entry, cache_limit)
            })
            .await
            .map_err(|e| Error::Other(e.to_string()))??
        };
        self.update_config(|c| c.server = Some(entry))?;
        self.insert(server.clone());
        info!(server = %server.entry.name, %url, "server added");
        Ok(server)
    }

    /// 로그아웃하고 서버를 뺀다. `purge`면 로컬 데이터(인덱스·차트·캐시)도 지운다.
    /// 쓰는 중이라 다 못 지운 파일은 다음에 앱을 열 때 지운다.
    pub async fn remove(&self, purge: bool) -> Result<ServerEntry> {
        let server = self.require()?;
        let entry = server.entry.clone();
        self.logout().await?;
        self.update_config(|c| c.server = None)?;
        self.drive.clear();
        self.lock().server = None;
        // 진행 중인 동기화가 끝나길 기다리고 새 동기화는 막는다. 그래야 인덱스 DB 등 파일이 닫혀
        // 지울 수 있고(Windows는 열린 파일을 못 지움), 지운 폴더에 동기화가 다시 쓰지 않는다.
        server.removed.store(true, Ordering::SeqCst);
        drop(server.sync_lock.lock().await);
        drop(server);
        if purge {
            let (dir, e) = (self.dir.clone(), entry.clone());
            tokio::task::spawn_blocking(move || dir.purge_server_dir(&e))
                .await
                .map_err(|e| Error::Other(e.to_string()))??;
        }
        info!(server = %entry.name, purge, "server removed");
        Ok(entry)
    }

    /// 브라우저 로그인. `open_browser`에 로그인 주소를 넘긴다. 받은 세션키는 저장하고 바로 쓴다.
    pub async fn login(&self, timeout: Duration, open_browser: impl FnOnce(&str)) -> Result<User> {
        let server = self.require()?;
        // 진행 중인 요청이 새 세션키를 덮어쓰지 않도록 따로 받는다.
        let api = Api::new(&server.entry.url)?;
        let req = LoginRequest::start(&api).await?;
        open_browser(req.url());
        let token = req.finish(&api, timeout).await?;
        let store = self.session(&server.entry);
        let key = token.session_key.clone();
        tokio::task::spawn_blocking(move || store.save(&key))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        server.api().set_session(Some(token.session_key));
        info!(server = %server.entry.name, "logged in");
        Ok(token.user)
    }

    /// 서버에서 세션을 폐기하고 저장된 세션키를 지운다. 서버에 닿지 않아도 로컬 세션키는 지운다.
    pub async fn logout(&self) -> Result<()> {
        let server = self.require()?;
        if server.api().has_session() {
            if let Err(e) = server.api().logout().await {
                warn!(server = %server.entry.name, %e, "server logout failed");
            }
            info!(server = %server.entry.name, "logged out");
        }
        server.api().set_session(None);
        let store = self.session(&server.entry);
        tokio::task::spawn_blocking(move || store.clear())
            .await
            .map_err(|e| Error::Other(e.to_string()))?
    }

    /// 계정 정보. 로그인하지 않았으면 None.
    pub async fn me(&self) -> Result<Option<Me>> {
        let server = self.require()?;
        if !server.api().has_session() {
            return Ok(None);
        }
        server.api().me().await.map(Some)
    }

    /// 로그인돼 있으면 동기화한다. 실패는 서버 상태와 로그에 남는다([`Server::sync`]).
    pub async fn sync(&self) {
        let Some(server) = self.server().filter(|s| !s.api().needs_login()) else {
            return;
        };
        let _ = server.sync().await;
    }
}

fn open_server(
    dir: &AppDir,
    use_keyring: bool,
    rt: &Handle,
    entry: &ServerEntry,
    cache_limit: u64,
) -> Result<Arc<Server>> {
    dir.prepare_server_dir(entry)?;
    if entry.is_insecure() {
        warn!(server = %entry.name, url = %entry.url, "unencrypted http server, session key is sent in plain text");
    }
    let mut opts = Options::new(&entry.url, dir.server_dir(entry));
    opts.cache_limit = cache_limit;
    opts.session = SessionStore::new(&entry.url, &dir.server_dir(entry), use_keyring).load()?;
    let client = Client::open(&opts)?;
    let fs = Arc::new(client.fs(rt.clone())?);
    Ok(Arc::new(Server {
        entry: entry.clone(),
        client,
        fs,
        status: Mutex::new(SyncStatus {
            last_ok_at: load_last_sync(&dir.server_dir(entry)),
            ..SyncStatus::default()
        }),
        sync_lock: tokio::sync::Mutex::new(()),
        removed: AtomicBool::new(false),
        table_status: Mutex::new(TableDownload::default()),
        table_stop: AtomicBool::new(false),
    }))
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 설정 창 명령은 다른 스레드에서 돌리므로 future가 `Send`여야 한다.
#[allow(dead_code)]
fn _table_futures_are_send(server: std::sync::Arc<Server>) {
    fn send<T: Send>(_: T) {}
    send(server.download_tables());
    send(server.add_user_table(""));
    send(server.refresh_user_tables());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_sync_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_last_sync(dir.path()), None);
        save_last_sync(dir.path(), 1_760_500_000);
        assert_eq!(load_last_sync(dir.path()), Some(1_760_500_000));
    }
}
