//! 받은 에셋 캐시. `<cache>/<song_id>/<path>`에 원본 그대로 저장한다.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use crate::index::Index;
use crate::{Error, Result};

/// 마지막 접근 시각을 DB에 다시 쓰기까지의 최소 간격(초).
const TOUCH_INTERVAL: i64 = 60;

pub struct Cache {
    root: PathBuf,
    tmp: PathBuf,
    index: Arc<Index>,
    limit: u64,
    touched: Mutex<HashMap<(u32, String), i64>>,
}

impl Cache {
    pub fn new(root: PathBuf, tmp: PathBuf, index: Arc<Index>, limit: u64) -> Self {
        Self {
            root,
            tmp,
            index,
            limit,
            touched: Mutex::new(HashMap::new()),
        }
    }

    /// 캐시 안 경로. zip 경로가 캐시 밖을 가리키면 오류.
    pub fn path(&self, song_id: u32, rel: &str) -> Result<PathBuf> {
        let rel = Path::new(rel);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(Error::Other(format!(
                "unsafe path in song {song_id}: {}",
                rel.display()
            )));
        }
        Ok(self.root.join(song_id.to_string()).join(rel))
    }

    /// 캐시에 있으면 경로를 돌려주고 접근 시각을 갱신한다.
    pub fn get(&self, song_id: u32, rel: &str) -> Result<Option<PathBuf>> {
        let path = self.path(song_id, rel)?;
        if !path.is_file() {
            return Ok(None);
        }
        let now = now();
        let key = (song_id, rel.to_string());
        let mut touched = self.touched.lock().unwrap_or_else(|e| e.into_inner());
        if touched.get(&key).is_none_or(|&t| now - t >= TOUCH_INTERVAL) {
            touched.insert(key, now);
            drop(touched);
            self.index.cache_touch(song_id, rel, now)?;
        }
        Ok(Some(path))
    }

    pub fn contains(&self, song_id: u32, rel: &str) -> bool {
        self.path(song_id, rel).is_ok_and(|p| p.is_file())
    }

    /// 바이트를 캐시에 넣는다 (임시 파일 → rename).
    pub fn put(&self, song_id: u32, rel: &str, data: &[u8]) -> Result<PathBuf> {
        let dest = self.path(song_id, rel)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self
            .tmp
            .join(format!("cache.{}.{}.part", std::process::id(), unique()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &dest)?;
        self.index
            .cache_put(song_id, rel, data.len() as u64, now())?;
        debug!(song_id, rel, size = data.len(), "cached");
        Ok(dest)
    }

    /// 한도를 넘으면 오래된 항목부터 지운다.
    pub fn evict(&self) -> Result<u64> {
        let mut total = self.index.cache_total()?;
        let mut freed = 0;
        while total > self.limit {
            let batch = self.index.cache_eviction_candidates(256)?;
            if batch.is_empty() {
                break;
            }
            for row in batch {
                if total <= self.limit {
                    break;
                }
                if let Ok(path) = self.path(row.song_id, &row.path)
                    && let Err(e) = std::fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(?path, %e, "cache eviction failed");
                    continue;
                }
                self.index.cache_remove(row.song_id, &row.path)?;
                total = total.saturating_sub(row.size);
                freed += row.size;
            }
        }
        Ok(freed)
    }

    pub fn total(&self) -> Result<u64> {
        self.index.cache_total()
    }

    pub fn pin_song(&self, song_id: u32, pinned: bool) -> Result<()> {
        self.index.cache_set_pinned(song_id, pinned)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}
