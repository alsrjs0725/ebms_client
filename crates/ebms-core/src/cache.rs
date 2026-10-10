//! 받은 에셋 캐시. `<cache>/<song_id>/<sha256(path)>`에 원본 그대로 저장한다.
//! 서버가 준 경로를 파일명으로 쓰지 않으므로 OS 파일명 규칙(예약어·ADS·대소문자)에 걸리지 않는다.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

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

    /// 캐시 안 경로. 파일명은 zip 경로의 해시다. 이상한 zip 경로는 오류.
    pub fn path(&self, song_id: u32, rel: &str) -> Result<PathBuf> {
        if Path::new(rel)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::Other(format!(
                "unsafe path in song {song_id}: {rel}"
            )));
        }
        Ok(self.root.join(song_id.to_string()).join(file_key(rel)))
    }

    /// 캐시에 있고 크기가 `size`와 같으면 경로를 돌려주고 접근 시각을 갱신한다.
    /// 크기가 다르면(정전으로 잘린 파일 등) 지우고 없는 것으로 본다.
    pub fn get(&self, song_id: u32, rel: &str, size: u64) -> Result<Option<PathBuf>> {
        let path = self.path(song_id, rel)?;
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(None);
        };
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() != size {
            warn!(
                song_id,
                rel,
                expected = size,
                actual = meta.len(),
                "cached file has wrong size, dropped"
            );
            let _ = std::fs::remove_file(&path);
            self.index.cache_remove(song_id, rel)?;
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

    /// 캐시에 크기 `size`인 파일이 있는지.
    pub fn contains(&self, song_id: u32, rel: &str, size: u64) -> bool {
        self.path(song_id, rel)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .is_some_and(|m| m.is_file() && m.len() == size)
    }

    /// 바이트를 캐시에 넣는다 (임시 파일 → fsync → rename).
    /// `zip_sha256`은 파일이 나온 곡 zip. 캐시에 다른 판이 들어 있으면 먼저 지운다.
    pub fn put(&self, song_id: u32, zip_sha256: &str, rel: &str, data: &[u8]) -> Result<PathBuf> {
        let dest = self.path(song_id, rel)?;
        self.claim_song(song_id, zip_sha256)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self
            .tmp
            .join(format!("cache.{}.{}.part", std::process::id(), unique()));
        // 정전 뒤 잘린 파일이 남지 않게 내용을 디스크에 쓴 다음 rename한다.
        if let Err(e) = write_synced(&tmp, data).and_then(|()| std::fs::rename(&tmp, &dest)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.index
            .cache_put(song_id, rel, data.len() as u64, now())?;
        debug!(song_id, rel, size = data.len(), "cached");
        Ok(dest)
    }

    /// 곡 캐시를 `zip_sha256` 판으로 표시한다. 다른 판이 들어 있으면 지운다.
    fn claim_song(&self, song_id: u32, zip_sha256: &str) -> Result<()> {
        match self.index.cache_song_sha(song_id)? {
            Some(sha) if sha == zip_sha256 => return Ok(()),
            Some(sha) => {
                let _guard = self.evict_lock.lock().unwrap_or_else(|e| e.into_inner());
                info!(
                    song_id,
                    old = sha,
                    new = zip_sha256,
                    "song changed, dropping cached files"
                );
                self.remove_song(song_id)?;
            }
            None => {}
        }
        self.index.cache_set_song_sha(song_id, zip_sha256)
    }

    /// 매니페스트와 캐시를 맞춘다. 곡 zip 해시가 바뀌었거나 서버에서 사라진 곡은
    /// 캐시를 지운다(곡 id가 다른 곡에 다시 쓰여도 옛 파일을 주지 않게). 지운 바이트 수.
    /// 해시 기록이 없는 곡(기록 전에 받은 캐시)은 지금 매니페스트 판으로 본다.
    pub fn drop_stale_songs(&self) -> Result<u64> {
        let _guard = self.evict_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut freed = 0;
        for (song_id, cached, current) in self.index.cache_song_versions()? {
            match (cached, current) {
                (Some(c), Some(m)) if c == m => {}
                (None, Some(m)) => self.index.cache_set_song_sha(song_id, &m)?,
                (cached, current) => {
                    info!(
                        song_id,
                        ?cached,
                        ?current,
                        "song changed or removed on server, dropping cached files"
                    );
                    freed += self.remove_song(song_id)?;
                }
            }
        }
        Ok(freed)
    }

    /// 인덱스에 없는 캐시 파일(넣는 도중 꺼져 rename만 된 파일 등)을 지운다. 지금 넣는 중일 수 있는
    /// 최근 파일([`STALE_AGE`](crate::paths::STALE_AGE) 이내)은 남긴다. 지운 개수.
    pub fn remove_orphans(&self) -> Result<usize> {
        let _guard = self.evict_lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(dirs) = std::fs::read_dir(&self.root) else {
            return Ok(0);
        };
        let mut removed = 0;
        for dir in dirs.flatten() {
            let Some(song_id) = dir.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let keys: std::collections::HashSet<String> = self
                .index
                .cache_song_paths(song_id)?
                .iter()
                .map(|(rel, _)| file_key(rel))
                .collect();
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let known = f.file_name().to_str().is_some_and(|n| keys.contains(n));
                if known || !crate::paths::is_stale(&path) {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            info!(removed, "removed cache files missing from index");
        }
        Ok(removed)
    }

    /// 곡의 캐시 파일과 항목을 모두 지운다. 못 지운 파일은 로그만 남긴다. 지운 바이트 수.
    fn remove_song(&self, song_id: u32) -> Result<u64> {
        let mut freed = 0;
        for (rel, size) in self.index.cache_song_paths(song_id)? {
            let Ok(path) = self.path(song_id, &rel) else {
                continue;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => freed += size,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(?path, %e, "cache removal failed"),
            }
        }
        self.index.cache_remove_song(song_id)?;
        let _ = std::fs::remove_dir(self.root.join(song_id.to_string()));
        Ok(freed)
    }

    /// 한도를 넘으면 오래 안 쓴 곡부터 지운다.
    pub fn evict(&self) -> Result<u64> {
        self.evict_to(self.limit)
    }

    /// "항상 보관"이 아닌 곡을 모두 지운다. 읽는 중이라 못 지운 파일은 남긴다.
    pub fn clear(&self) -> Result<u64> {
        let freed = self.evict_to(0)?;
        self.remove_empty_dirs();
        Ok(freed)
    }

    /// 오래 안 쓴 곡부터 `limit` 이하가 될 때까지 곡 단위로 지운다.
    /// 곡 일부만 남으면 다음 플레이 때 곡 zip 전체를 다시 받아(티켓 1개) 남은 파일도 소용없다.
    fn evict_to(&self, limit: u64) -> Result<u64> {
        let _guard = self.evict_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut total = self.index.cache_total()?;
        let mut freed = 0;
        while total > limit {
            let batch = self.index.cache_eviction_songs(256)?;
            // 지울 곡이 없으면(모두 고정) 멈춘다. 지운 곡은 파일을 못 지워도 항목이 빠지므로 다시 나오지 않는다.
            if batch.is_empty() {
                break;
            }
            for (song_id, size) in batch {
                if total <= limit {
                    break;
                }
                freed += self.remove_song(song_id)?;
                total = total.saturating_sub(size);
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

fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// zip 경로 → 캐시 파일명.
fn file_key(rel: &str) -> String {
    hex::encode(Sha256::digest(rel.as_bytes()))
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
    fn test_remove_orphans_keeps_indexed_and_recent_files() -> Result<()> {
        let dir = tempdir()?;
        let index = Arc::new(Index::open(&dir.path().join("index.db"))?);
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let cache = Cache::new(dir.path().join("cache"), tmp, index, 1 << 30);

        let kept = cache.put(1, "z", "a.wav", b"a")?;
        let orphan = cache.path(1, "b.wav")?;
        let recent = cache.path(1, "c.wav")?;
        std::fs::write(&orphan, b"b")?;
        std::fs::write(&recent, b"c")?;
        let old = SystemTime::now() - crate::paths::STALE_AGE * 2;
        for p in [&kept, &orphan] {
            std::fs::File::options()
                .write(true)
                .open(p)?
                .set_modified(old)?;
        }

        assert_eq!(cache.remove_orphans()?, 1);
        assert!(kept.exists());
        assert!(!orphan.exists());
        assert!(recent.exists());
        Ok(())
    }

    #[test]
    fn test_path_uses_hash() -> Result<()> {
        let dir = tempdir()?;
        let index = Arc::new(Index::open(&dir.path().join("index.db"))?);
        let cache = Cache::new(dir.path().join("cache"), dir.path().join("tmp"), index, 0);

        for rel in ["CON", "a.wav:x", "x.", "sub/A.wav"] {
            let p = cache.path(7, rel)?;
            assert_eq!(
                p.parent(),
                Some(dir.path().join("cache").join("7").as_path())
            );
            assert_eq!(p.file_name().unwrap().len(), 64);
        }
        assert_ne!(cache.path(7, "A.wav")?, cache.path(7, "a.wav")?);
        assert_ne!(cache.path(7, "x.")?, cache.path(7, "x")?);
        assert!(cache.path(7, "../x").is_err());
        assert!(cache.path(7, "/x").is_err());
        Ok(())
    }

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
        let _cached_file = cache.put(1, "z", "test.txt", b"hello world")?;
        assert_eq!(cache.total()?, 11);

        // Make remove_file fail in an OS-appropriate way
        #[cfg(unix)]
        let song_dir = root.join("1");
        #[cfg(unix)]
        let _reset_perms = {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&song_dir)?.permissions();
            permissions.set_mode(0o555);
            std::fs::set_permissions(&song_dir, permissions)?;
            struct Reset(PathBuf);
            impl Drop for Reset {
                fn drop(&mut self) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
                }
            }
            Reset(song_dir)
        };

        #[cfg(windows)]
        let _file_lock = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&_cached_file)?
        };

        let start = std::time::Instant::now();
        // evict() with limit 0 should finish quickly without looping infinitely
        let freed = cache.evict()?;
        let elapsed = start.elapsed();

        assert!(elapsed < std::time::Duration::from_secs(2));
        // Item should be removed from index even though file deletion failed
        assert_eq!(cache.total()?, 0);
        assert_eq!(freed, 0);

        Ok(())
    }

    fn song(song_id: u32, zip_sha256: &str) -> crate::manifest::SongManifest {
        crate::manifest::SongManifest {
            song_id,
            folder: String::new(),
            zip_size: 0,
            zip_sha256: zip_sha256.into(),
            charts: vec![],
            files: vec![],
        }
    }

    #[test]
    fn test_stale_songs_are_dropped() -> Result<()> {
        let dir = tempdir()?;
        let index = Arc::new(Index::open(&dir.path().join("index.db"))?);
        let cache = Cache::new(
            dir.path().join("cache"),
            dir.path().join("tmp"),
            index.clone(),
            u64::MAX,
        );
        std::fs::create_dir_all(dir.path().join("tmp"))?;
        index.replace_manifest_chunk(0, "m", &[song(1, "a"), song(2, "b"), song(3, "c")])?;

        cache.put(1, "a", "x.wav", b"same")?;
        cache.put(2, "old", "x.wav", b"changed")?;
        cache.put(4, "d", "x.wav", b"removed")?;
        // 해시 기록 전에 받은 캐시
        let legacy = cache.path(3, "x.wav")?;
        std::fs::create_dir_all(legacy.parent().unwrap())?;
        std::fs::write(&legacy, b"legacy")?;
        index.cache_put(3, "x.wav", 6, 0)?;

        assert_eq!(cache.drop_stale_songs()?, 7 + 7);
        assert!(cache.get(1, "x.wav", 4)?.is_some());
        assert!(cache.get(2, "x.wav", 7)?.is_none());
        assert!(cache.get(4, "x.wav", 7)?.is_none());
        assert!(cache.get(3, "x.wav", 6)?.is_some());
        assert_eq!(index.cache_song_sha(3)?.as_deref(), Some("c"));
        assert_eq!(index.cache_song_sha(2)?, None);
        assert_eq!(cache.total()?, 4 + 6);

        // 다른 판을 넣으면 그 곡의 옛 파일을 먼저 지운다.
        cache.put(1, "a2", "y.wav", b"new")?;
        assert!(cache.get(1, "x.wav", 4)?.is_none());
        assert!(cache.get(1, "y.wav", 3)?.is_some());
        Ok(())
    }

    #[test]
    fn test_evict_whole_songs_by_last_access() -> Result<()> {
        let dir = tempdir()?;
        let index = Arc::new(Index::open(&dir.path().join("index.db"))?);
        std::fs::create_dir_all(dir.path().join("tmp"))?;
        let cache = Cache::new(
            dir.path().join("cache"),
            dir.path().join("tmp"),
            index.clone(),
            30,
        );
        for song_id in 1..=3 {
            cache.put(song_id, "z", "a.wav", &[0; 10])?;
            cache.put(song_id, "z", "b.wav", &[0; 5])?;
        }
        // 곡 1: 파일 하나만 최근에 읽음 → 곡 전체가 최근으로 친다.
        index.cache_touch(1, "a.wav", 100)?;
        index.cache_touch(1, "b.wav", 300)?;
        index.cache_touch(2, "a.wav", 200)?;
        index.cache_touch(2, "b.wav", 200)?;
        index.cache_touch(3, "a.wav", 250)?;
        index.cache_touch(3, "b.wav", 50)?;
        cache.pin_song(3, true)?;

        // 45 > 30: 고정된 곡 3을 빼고 가장 오래된 곡 2를 통째로 지운다.
        assert_eq!(cache.evict()?, 15);
        assert_eq!(cache.total()?, 30);
        assert!(!cache.contains(2, "a.wav", 10) && !cache.contains(2, "b.wav", 5));
        assert!(cache.contains(1, "a.wav", 10) && cache.contains(1, "b.wav", 5));
        // 그다음은 곡 1 전체. 고정된 곡 3은 남는다.
        assert_eq!(cache.clear()?, 15);
        assert!(!cache.contains(1, "a.wav", 10) && !cache.contains(1, "b.wav", 5));
        assert!(cache.contains(3, "a.wav", 10) && cache.contains(3, "b.wav", 5));
        assert_eq!(cache.total()?, 15);
        Ok(())
    }

    #[test]
    fn test_wrong_size_is_a_miss() -> Result<()> {
        let dir = tempdir()?;
        let index = Arc::new(Index::open(&dir.path().join("index.db"))?);
        let cache = Cache::new(
            dir.path().join("cache"),
            dir.path().join("tmp"),
            index,
            u64::MAX,
        );
        std::fs::create_dir_all(dir.path().join("tmp"))?;
        let path = cache.put(1, "a", "x.wav", b"hello")?;
        assert!(cache.contains(1, "x.wav", 5));
        std::fs::write(&path, b"")?;
        assert!(!cache.contains(1, "x.wav", 5));
        assert!(cache.get(1, "x.wav", 5)?.is_none());
        assert!(!path.exists());
        assert_eq!(cache.total()?, 0);
        Ok(())
    }
}
