//! 등록된 서버 전체를 관리한다. 상주 앱과 CLI `mount`가 쓴다.
//!
//! 서버마다 [`Client`]와 가상 FS를 열어 [`Drive`]에 서버별 최상위 폴더로 넣는다.
//! 서버 추가·삭제, 로그인·로그아웃, 동기화를 서버별로 따로 한다. 한 서버의 오류는 다른 서버에 영향을 주지 않는다.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::runtime::Handle;
use tracing::warn;

use crate::api::{Api, Me, User};
use crate::auth::LoginRequest;
use crate::config::{AppDir, Config, ServerEntry, normalize_url};
use crate::drive::Drive;
use crate::fs::EbmsFs;
use crate::session::SessionStore;
use crate::sync::SyncReport;
use crate::{Client, Error, Options, Result};

/// 서버 하나.
pub struct Server {
    pub entry: ServerEntry,
    pub client: Client,
    pub fs: Arc<EbmsFs>,
    status: Mutex<SyncStatus>,
    /// 같은 서버의 동기화가 겹치지 않게
    sync_lock: tokio::sync::Mutex<()>,
}

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

/// 마지막 동기화 결과. 설정 창에만 보여준다.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncStatus {
    /// 마지막으로 성공한 시각(unix 초)
    pub last_ok_at: Option<i64>,
    /// 마지막 시도가 실패했으면 그 오류
    pub last_error: Option<String>,
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

    /// 동기화하고 바뀐 게 있으면 가상 트리를 다시 만든다.
    pub async fn sync(&self) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().await;
        let result = self.client.sync().await;
        // 인덱스를 읽으므로 런타임 밖에서, 설정 창이 기다리지 않게 상태 잠금 밖에서.
        if matches!(&result, Ok(report) if report.changed()) {
            let fs = self.fs.clone();
            let reloaded = tokio::task::spawn_blocking(move || fs.reload())
                .await
                .map_err(|e| Error::Other(e.to_string()))
                .and_then(|r| r);
            if let Err(e) = reloaded {
                warn!(server = %self.entry.name, %e, "reload failed");
            }
        }
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match &result {
            Ok(_) => {
                status.last_ok_at = Some(unix_now());
                status.last_error = None;
            }
            Err(e) => status.last_error = Some(e.to_string()),
        }
        drop(status);
        result
    }
}

struct State {
    config: Config,
    servers: BTreeMap<String, Arc<Server>>,
}

pub struct Hub {
    dir: AppDir,
    use_keyring: bool,
    rt: Handle,
    drive: Arc<Drive>,
    state: Mutex<State>,
}

impl Hub {
    /// 설정을 읽고 등록된 서버를 모두 연다. 열지 못한 서버는 로그만 남기고 건너뛴다.
    /// 키체인을 읽으므로 tokio 런타임 밖에서 부른다. `rt`는 다운로드를 돌릴 런타임.
    pub fn open(dir: AppDir, use_keyring: bool, rt: Handle) -> Result<Self> {
        let config = dir.load_config()?;
        dir.purge_pending();
        let hub = Self {
            dir,
            use_keyring,
            rt,
            drive: Arc::new(Drive::new()),
            state: Mutex::new(State {
                config: config.clone(),
                servers: BTreeMap::new(),
            }),
        };
        for entry in &config.servers {
            match hub.open_server(entry) {
                Ok(server) => hub.insert(server),
                Err(e) => warn!(server = %entry.name, %e, "could not open server"),
            }
        }
        Ok(hub)
    }

    fn open_server(&self, entry: &ServerEntry) -> Result<Arc<Server>> {
        open_server(&self.dir, self.use_keyring, &self.rt, entry)
    }

    fn insert(&self, server: Arc<Server>) {
        self.drive
            .insert(&server.entry.id, &server.entry.name, server.fs.clone());
        self.lock().servers.insert(server.entry.id.clone(), server);
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

    /// 서버별 최상위 폴더로 합친 가상 드라이브.
    pub fn drive(&self) -> Arc<Drive> {
        self.drive.clone()
    }

    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    /// 설정을 바꿔 저장한다.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut state = self.lock();
        let mut config = state.config.clone();
        f(&mut config);
        self.dir.save_config(&config)?;
        state.config = config.clone();
        Ok(config)
    }

    /// 설정 순서대로.
    pub fn servers(&self) -> Vec<Arc<Server>> {
        let state = self.lock();
        state
            .config
            .servers
            .iter()
            .filter_map(|e| state.servers.get(&e.id).cloned())
            .collect()
    }

    pub fn server(&self, id: &str) -> Result<Arc<Server>> {
        self.lock()
            .servers
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown server: {id}")))
    }

    pub fn session(&self, entry: &ServerEntry) -> SessionStore {
        SessionStore::new(&entry.url, &self.dir.server_dir(entry), self.use_keyring)
    }

    /// `/api/version`으로 EBMS 서버인지 확인하고 추가한다.
    pub async fn add(&self, url: &str, name: Option<&str>) -> Result<Arc<Server>> {
        let url = normalize_url(url)?;
        let version = Api::new(&url)?
            .version()
            .await
            .map_err(|e| Error::Config(format!("{url} is not a reachable EBMS server ({e})")))?;
        if !crate::API_VERSIONS.contains(&version.api) {
            return Err(Error::ApiVersion(version.api));
        }
        let mut config = self.config();
        let entry = config.add(&url, name)?.clone();
        let server = {
            let (dir, rt, entry) = (self.dir.clone(), self.rt.clone(), entry.clone());
            let use_keyring = self.use_keyring;
            tokio::task::spawn_blocking(move || open_server(&dir, use_keyring, &rt, &entry))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        self.update_config(|c| c.servers.push(entry.clone()))?;
        self.insert(server.clone());
        Ok(server)
    }

    /// 로그아웃하고 목록에서 뺀다. `purge`면 로컬 데이터(인덱스·차트·캐시)도 지운다.
    /// 쓰는 중이라 다 못 지운 파일은 다음에 앱을 열 때 지운다.
    pub async fn remove(&self, id: &str, purge: bool) -> Result<ServerEntry> {
        let entry = self.server(id)?.entry.clone();
        self.logout(id).await?;
        self.update_config(|c| {
            c.remove(id);
        })?;
        self.drive.remove(id);
        self.lock().servers.remove(id);
        if purge {
            let (dir, e) = (self.dir.clone(), entry.clone());
            tokio::task::spawn_blocking(move || dir.purge_server_dir(&e))
                .await
                .map_err(|e| Error::Other(e.to_string()))??;
        }
        Ok(entry)
    }

    /// 브라우저 로그인. `open_browser`에 로그인 주소를 넘긴다. 받은 세션키는 저장하고 바로 쓴다.
    pub async fn login(
        &self,
        id: &str,
        device_name: &str,
        timeout: Duration,
        open_browser: impl FnOnce(&str),
    ) -> Result<User> {
        let server = self.server(id)?;
        // 진행 중인 요청이 새 세션키를 덮어쓰지 않도록 따로 받는다.
        let api = Api::new(&server.entry.url)?;
        let req = LoginRequest::start(&api, device_name).await?;
        open_browser(req.url());
        let token = req.finish(&api, timeout).await?;
        let store = self.session(&server.entry);
        let key = token.session_key.clone();
        tokio::task::spawn_blocking(move || store.save(&key))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        server.api().set_session(Some(token.session_key));
        Ok(token.user)
    }

    /// 서버에서 세션을 폐기하고 저장된 세션키를 지운다. 서버에 닿지 않아도 로컬 세션키는 지운다.
    pub async fn logout(&self, id: &str) -> Result<()> {
        let server = self.server(id)?;
        if server.api().has_session()
            && let Err(e) = server.api().logout().await
        {
            warn!(server = %server.entry.name, %e, "server logout failed");
        }
        server.api().set_session(None);
        let store = self.session(&server.entry);
        tokio::task::spawn_blocking(move || store.clear())
            .await
            .map_err(|e| Error::Other(e.to_string()))?
    }

    /// 계정 정보. 로그인하지 않았으면 None.
    pub async fn me(&self, id: &str) -> Result<Option<Me>> {
        let server = self.server(id)?;
        if !server.api().has_session() {
            return Ok(None);
        }
        server.api().me().await.map(Some)
    }

    /// 로그인된 서버를 모두 동기화한다. 실패는 서버별 상태에만 남긴다.
    pub async fn sync_all(&self) {
        let servers = self.servers();
        let jobs = servers
            .iter()
            .filter(|s| !s.api().needs_login())
            .map(|s| async move {
                if let Err(e) = s.sync().await {
                    warn!(server = %s.entry.name, %e, "sync failed");
                }
            });
        futures_util::future::join_all(jobs).await;
    }
}

fn open_server(
    dir: &AppDir,
    use_keyring: bool,
    rt: &Handle,
    entry: &ServerEntry,
) -> Result<Arc<Server>> {
    dir.prepare_server_dir(entry)?;
    let mut opts = Options::new(&entry.url, dir.server_dir(entry));
    opts.session = SessionStore::new(&entry.url, &dir.server_dir(entry), use_keyring).load()?;
    let client = Client::open(&opts)?;
    let fs = Arc::new(client.fs(rt.clone())?);
    Ok(Arc::new(Server {
        entry: entry.clone(),
        client,
        fs,
        status: Mutex::new(SyncStatus::default()),
        sync_lock: tokio::sync::Mutex::new(()),
    }))
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
