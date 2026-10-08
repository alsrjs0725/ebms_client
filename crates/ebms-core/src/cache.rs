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
    evict_lock: Mutex<()>,
}

impl Cache {
    pub fn new(root: PathBuf, tmp: PathBuf, index: Arc<Index>, limit: u64) -> Self {
        Self {
            root,
            tmp,
            index,
            limit,
            touched: Mutex::new(HashMap::new()),
            evict_lock: Mutex::new(()),
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
        self.evict_to(self.limit)
    }

    /// "항상 보관"이 아닌 항목을 모두 지운다. 읽는 중이라 못 지운 파일은 남긴다.
    pub fn clear(&self) -> Result<u64> {
        let freed = self.evict_to(0)?;
        self.remove_empty_dirs();
        Ok(freed)
    }

    /// 오래된 항목부터 `limit` 이하가 될 때까지 지운다.
    fn evict_to(&self, limit: u64) -> Result<u64> {
        let _guard = self.evict_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut total = self.index.cache_total()?;
        let mut freed = 0;
        while total > limit {
            let batch = self.index.cache_eviction_candidates(256)?;
            let mut progressed = false;
            for row in &batch {
                if total <= limit {
                    break;
                }
                if let Ok(path) = self.path(row.song_id, &row.path) {
                    if let Err(e) = std::fs::remove_file(&path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            warn!(?path, %e, "cache eviction failed");
                        }
                    } else {
                        freed += row.size;
                    }
                }
                self.index.cache_remove(row.song_id, &row.path)?;
                total = total.saturating_sub(row.size);
                progressed = true;
            }
            // 지울 게 없거나 모두 실패하면 같은 후보만 다시 나오므로 멈춘다.
            if !progressed {
                break;
            }
        }
        Ok(freed)
    }

    /// 비워진 곡 폴더를 지운다. 파일이 남은 폴더는 그대로 둔다.
    fn remove_empty_dirs(&self) {
        fn walk(dir: &Path) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(&path);
                    let _ = std::fs::remove_dir(&path);
                }
            }
        }
        walk(&self.root);
    }

    pub fn limit(&self) -> u64 {
        self.limit
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_evict_handles_unremovable_file() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path().join("cache");
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&tmp)?;

        let index_path = dir.path().join("index.db");
        let index = Arc::new(Index::open(&index_path)?);

        let cache = Cache::new(root.clone(), tmp, index.clone(), 0);

        // Put a file in the cache
        cache.put(1, "test.txt", b"hello world")?;
        assert_eq!(cache.total()?, 11);

        // Make the song directory read-only so remove_file fails on Unix systems
        let song_dir = root.join("1");
        let mut permissions = std::fs::metadata(&song_dir)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&song_dir, permissions.clone())?;

        let start = std::time::Instant::now();
        // evict() with limit 0 should finish quickly without looping infinitely
        let freed = cache.evict()?;
        let elapsed = start.elapsed();

        // Restore permissions so cleanup works
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(&song_dir, permissions);

        assert!(elapsed < std::time::Duration::from_secs(2));
        // Item should be removed from index even though file deletion failed
        assert_eq!(cache.total()?, 0);
        assert_eq!(freed, 0);

        Ok(())
    }
}
