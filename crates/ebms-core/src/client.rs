use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;
use crate::api::Api;
use crate::cache::Cache;
use crate::fetch::Fetcher;
use crate::fs::EbmsFs;
use crate::index::Index;
use crate::paths::Paths;
use crate::sync::{SyncReport, sync_all};

#[derive(Clone, Debug)]
pub struct Options {
    pub server: String,
    pub data_dir: PathBuf,
    /// 캐시 한도 (바이트)
    pub cache_limit: u64,
    /// 이 서버의 세션키
    pub session: Option<String>,
}

impl Options {
    pub fn new(server: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            server: server.into(),
            data_dir: data_dir.into(),
            cache_limit: 20 << 30,
            session: None,
        }
    }
}

/// 코어 구성요소를 묶는다.
pub struct Client {
    pub api: Arc<Api>,
    pub paths: Paths,
    pub index: Arc<Index>,
    pub fetcher: Arc<Fetcher>,
}

impl Client {
    pub fn open(opts: &Options) -> Result<Self> {
        let paths = Paths::new(&opts.data_dir);
        paths.create_dirs()?;
        let api = Arc::new(Api::new(&opts.server)?);
        api.set_session(opts.session.clone());
        let index = Arc::new(Index::open(&paths.index())?);
        let cache = Arc::new(Cache::new(
            paths.cache(),
            paths.tmp(),
            index.clone(),
            opts.cache_limit,
        ));
        let fetcher = Arc::new(Fetcher::new(api.clone(), cache, paths.clone()));
        Ok(Self {
            api,
            paths,
            index,
            fetcher,
        })
    }

    pub async fn sync(&self) -> Result<SyncReport> {
        sync_all(&self.api, &self.paths, &self.index).await
    }

    /// 가상 FS. `rt`는 다운로드를 돌릴 tokio 런타임.
    pub fn fs(&self, rt: tokio::runtime::Handle) -> Result<EbmsFs> {
        EbmsFs::new(
            self.index.clone(),
            self.fetcher.clone(),
            self.paths.clone(),
            rt,
        )
    }
}
