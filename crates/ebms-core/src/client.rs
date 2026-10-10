use std::path::PathBuf;
use std::sync::Arc;

use crate::api::Api;
use crate::cache::Cache;
use crate::fetch::Fetcher;
use crate::fs::EbmsFs;
use crate::index::Index;
use crate::paths::Paths;
use crate::sync::{SyncReport, sync_all};
use crate::{Error, Result};

#[derive(Clone)]
pub struct Options {
    pub server: String,
    pub data_dir: PathBuf,
    /// 캐시 한도 (바이트)
    pub cache_limit: u64,
    /// 이 서버의 세션키
    pub session: Option<String>,
}

/// 세션키는 가린다.
impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("server", &self.server)
            .field("data_dir", &self.data_dir)
            .field("cache_limit", &self.cache_limit)
            .field(
                "session",
                &self.session.as_ref().map(|_| crate::api::REDACTED),
            )
            .finish()
    }
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

    /// 동기화한 뒤 서버에서 바뀌거나 사라진 곡의 캐시를 지운다.
    pub async fn sync(&self) -> Result<SyncReport> {
        let report = sync_all(&self.api, &self.paths, &self.index).await?;
        let cache = self.fetcher.cache().clone();
        tokio::task::spawn_blocking(move || cache.drop_stale_songs())
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        Ok(report)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_session_key() {
        let mut opts = Options::new("http://127.0.0.1:8000", "/tmp/x");
        opts.session = Some("secret-key".into());
        let api = Api::new(&opts.server).unwrap();
        api.set_session(opts.session.clone());
        for text in [format!("{opts:?}"), format!("{api:?}")] {
            assert!(!text.contains("secret-key"), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
    }
}
