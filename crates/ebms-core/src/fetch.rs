//! 요청 시 다운로드.
//!
//! - 가벼운 파일(이미지, 프리뷰 등)은 그 파일만 `Range`로 받는다.
//! - 한 곡에서 무거운 파일(키음, 영상)이 `promote_after`개 이상 열리면 곡 zip 전체를 한 번에 받는다.
//! - 같은 파일·곡을 동시에 요청하면 한 번만 받는다.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use crate::api::Api;
use crate::cache::Cache;
use crate::manifest::FileEntry;
use crate::paths::Paths;
use crate::tree::SongInfo;
use crate::{Error, Result};

const LOCAL_HEADER_LEN: u64 = 30;
const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
/// local header의 extra 필드를 위해 미리 더 받는 바이트.
const HEADER_SLACK: u64 = 256;

const HEAVY_EXTS: &[&str] = &[
    "wav", "ogg", "flac", "mp3", "m4a", "opus", "mpg", "mpeg", "mp4", "avi", "wmv", "webm", "m4v",
    "mkv",
];

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
enum Key {
    File(u32, u64), // (song_id, entry offset)
    Song(u32),
}

pub struct Fetcher {
    api: Arc<Api>,
    cache: Arc<Cache>,
    paths: Paths,
    promote_after: usize,
    locks: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    heavy_opened: Mutex<HashMap<u32, HashSet<String>>>,
}

impl Fetcher {
    pub fn new(api: Arc<Api>, cache: Arc<Cache>, paths: Paths, promote_after: usize) -> Self {
        Self {
            api,
            cache,
            paths,
            promote_after,
            locks: Mutex::new(HashMap::new()),
            heavy_opened: Mutex::new(HashMap::new()),
        }
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    /// 파일이 캐시에 있도록 하고 경로를 돌려준다.
    pub async fn ensure(&self, song: &SongInfo, entry: &FileEntry) -> Result<PathBuf> {
        if let Some(p) = self.cache.get(song.song_id, &entry.path)? {
            return Ok(p);
        }

        if is_heavy(&entry.path) && self.note_heavy(song.song_id, &entry.path) {
            match self.ensure_song(song).await {
                Ok(()) => {
                    if let Some(p) = self.cache.get(song.song_id, &entry.path)? {
                        return Ok(p);
                    }
                }
                Err(e) => {
                    warn!(song_id = song.song_id, %e, "full song download failed, falling back to file");
                    // 카운트를 비워 다음 재시도까지 파일 단위로 받는다.
                    self.heavy_opened
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&song.song_id);
                }
            }
        }

        let lock = self.lock(Key::File(song.song_id, entry.offset));
        let _guard = lock.lock().await;
        if let Some(p) = self.cache.get(song.song_id, &entry.path)? {
            return Ok(p);
        }
        let data = self.fetch_entry(song, entry).await?;
        let path = self.cache.put(song.song_id, &entry.path, &data)?;
        self.evict_in_background();
        Ok(path)
    }

    /// 곡 zip 전체를 받아 모든 파일을 캐시에 넣는다.
    pub async fn ensure_song(&self, song: &SongInfo) -> Result<()> {
        let lock = self.lock(Key::Song(song.song_id));
        let _guard = lock.lock().await;
        if song
            .files
            .iter()
            .all(|f| self.cache.contains(song.song_id, &f.path))
        {
            return Ok(());
        }

        info!(
            song_id = song.song_id,
            size = song.zip_size,
            "downloading whole song"
        );
        let tmp = self.paths.tmp_file(&format!("song_{}", song.song_id));
        let result = async {
            let sha = self.api.download_song(song.song_id, &tmp).await?;
            if sha != song.zip_sha256 {
                return Err(Error::Integrity(format!(
                    "song {}: expected {}, got {sha}",
                    song.song_id, song.zip_sha256
                )));
            }
            let cache = self.cache.clone();
            let song = song.clone();
            let tmp = tmp.clone();
            tokio::task::spawn_blocking(move || extract_song(&cache, &song, &tmp))
                .await
                .map_err(|e| Error::Other(e.to_string()))?
        }
        .await;
        let _ = std::fs::remove_file(&tmp);
        result?;
        self.evict_in_background();
        Ok(())
    }

    /// 곡 zip에서 파일 하나만 받는다.
    async fn fetch_entry(&self, song: &SongInfo, entry: &FileEntry) -> Result<Vec<u8>> {
        let start = entry.offset;
        let name_len = entry.path.len() as u64;
        let guess_end = (start + LOCAL_HEADER_LEN + name_len + entry.comp_size + HEADER_SLACK)
            .min(song.zip_size)
            .saturating_sub(1);
        let mut buf = self
            .api
            .song_range(song.song_id, &song.zip_sha256, start, guess_end)
            .await?
            .to_vec();

        if buf.len() < LOCAL_HEADER_LEN as usize
            || u32::from_le_bytes(buf[0..4].try_into().unwrap()) != LOCAL_HEADER_SIG
        {
            return Err(Error::Integrity(format!(
                "song {}: bad local header for {}",
                song.song_id, entry.path
            )));
        }
        let n = u16::from_le_bytes([buf[26], buf[27]]) as u64;
        let m = u16::from_le_bytes([buf[28], buf[29]]) as u64;
        let data_start = LOCAL_HEADER_LEN + n + m;
        let need = data_start + entry.comp_size;
        if (buf.len() as u64) < need {
            let more = self
                .api
                .song_range(
                    song.song_id,
                    &song.zip_sha256,
                    start + buf.len() as u64,
                    start + need - 1,
                )
                .await?;
            buf.extend_from_slice(&more);
        }
        let raw = &buf[data_start as usize..need as usize];
        decode_entry(entry, raw)
    }

    fn note_heavy(&self, song_id: u32, path: &str) -> bool {
        let mut map = self.heavy_opened.lock().unwrap_or_else(|e| e.into_inner());
        let set = map.entry(song_id).or_default();
        set.insert(path.to_string());
        set.len() >= self.promote_after
    }

    fn lock(&self, key: Key) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        // 아무도 쓰지 않는 잠금은 정리한다.
        if locks.len() > 1024 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(key).or_default().clone()
    }

    fn evict_in_background(&self) {
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = cache.evict() {
                warn!(%e, "cache eviction failed");
            }
        });
    }
}

fn is_heavy(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name.starts_with("preview") {
        return false; // 선곡 화면 프리뷰
    }
    crate::extension(path).is_some_and(|e| HEAVY_EXTS.contains(&e.as_str()))
}

fn decode_entry(entry: &FileEntry, raw: &[u8]) -> Result<Vec<u8>> {
    let data = match entry.method {
        0 => raw.to_vec(),
        8 => {
            let mut out = Vec::with_capacity(entry.size as usize);
            flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out)?;
            out
        }
        m => {
            return Err(Error::Other(format!(
                "unsupported zip method {m} for {}",
                entry.path
            )));
        }
    };
    if data.len() as u64 != entry.size {
        return Err(Error::Integrity(format!(
            "{}: size {} != {}",
            entry.path,
            data.len(),
            entry.size
        )));
    }
    if Some(crc32fast::hash(&data)) != entry.crc32_value() {
        return Err(Error::Integrity(format!("{}: crc32 mismatch", entry.path)));
    }
    Ok(data)
}

fn extract_song(cache: &Cache, song: &SongInfo, zip_path: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().to_string();
        if cache.path(song.song_id, &name).is_err() {
            warn!(
                song_id = song.song_id,
                name, "unsafe path in song zip, skipped"
            );
            continue;
        }
        if cache.contains(song.song_id, &name) {
            continue;
        }
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data)?;
        cache.put(song.song_id, &name, &data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_classification() {
        assert!(is_heavy("bgm01.wav"));
        assert!(is_heavy("sub/KICK.OGG"));
        assert!(!is_heavy("preview.ogg"));
        assert!(!is_heavy("banner.png"));
        assert!(!is_heavy("_7a.bme"));
    }
}
