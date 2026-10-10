//! 요청 시 다운로드.
//!
//! - 사전 파일(배너·프리뷰 등)은 보통 동기화 때 사전 청크로 받아 두므로 여기 오지 않는다.
//!   사전 청크에 아직 없는 곡(서버가 만드는 중, 이전 서버)만 사전 API로 그 파일을 받는다.
//! - 플레이 파일(키음·BGA 등)을 처음 열면 플레이 API로 곡 zip 전체를 받는다(티켓 1개).
//!   구동기가 아닌 프로그램의 읽기는 받지 않고 거절한다.
//! - 티켓이 없어 `429`를 받으면 `Retry-After` 동안 이 서버의 어느 곡도 플레이 다운로드를 다시
//!   요청하지 않는다. 티켓은 계정 단위라 다른 곡을 요청해도 같은 `429`가 온다.
//!   대기 시각은 데이터 폴더에 적어 두어 재시작해도 이어진다.
//! - 같은 파일·곡을 동시에 요청하면 한 번만 받는다.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::api::{Api, MAX_RETRY_AFTER};
use crate::cache::Cache;
use crate::fs::Caller;
use crate::manifest::{FileEntry, FileKind};
use crate::paths::Paths;
use crate::tree::SongInfo;
use crate::{Error, Result};

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
enum Key {
    File(u32, u64), // (song_id, entry offset)
    Song(u32),
}

pub struct Fetcher {
    api: Arc<Api>,
    cache: Arc<Cache>,
    paths: Paths,
    locks: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    /// 티켓이 없어 플레이 다운로드를 멈춘 경우 다시 시도할 시각(unix 초). 서버(계정) 단위.
    retry_at: Mutex<Option<u64>>,
}

impl Fetcher {
    pub fn new(api: Arc<Api>, cache: Arc<Cache>, paths: Paths) -> Self {
        // 지난 실행에서 받은 429 대기를 이어간다. 읽지 못하면 대기 없음으로 본다.
        let retry_at = std::fs::read_to_string(paths.no_ticket())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&at| at > unix_now());
        Self {
            api,
            cache,
            paths,
            locks: Mutex::new(HashMap::new()),
            retry_at: Mutex::new(retry_at),
        }
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    /// 파일이 캐시에 있도록 하고 경로를 돌려준다.
    /// 플레이 파일이 캐시에 없으면 `caller`가 구동기일 때만 곡 전체를 받는다.
    pub async fn ensure(
        &self,
        song: &SongInfo,
        entry: &FileEntry,
        caller: &dyn Caller,
    ) -> Result<PathBuf> {
        if let Some(p) = self.cache.get(song.song_id, &entry.path, entry.size)? {
            return Ok(p);
        }

        if entry.kind == FileKind::Play {
            if !caller.is_player() {
                let program = caller.name();
                info!(
                    song_id = song.song_id,
                    program,
                    path = entry.path,
                    "play file read by a non-player, refused"
                );
                return Err(Error::NotPlayer { program });
            }
            self.ensure_song(song, &caller.name()).await?;
            return self
                .cache
                .get(song.song_id, &entry.path, entry.size)?
                .ok_or_else(|| {
                    Error::Integrity(format!(
                        "song {}: {} not in song zip",
                        song.song_id, entry.path
                    ))
                });
        }

        let lock = self.lock(Key::File(song.song_id, entry.offset));
        let _guard = lock.lock().await;
        if let Some(p) = self.cache.get(song.song_id, &entry.path, entry.size)? {
            return Ok(p);
        }
        let data = self.api.pre_file(song.song_id, &entry.path).await?;
        verify_entry(entry, &data)?;
        let path = self
            .cache
            .put(song.song_id, &song.zip_sha256, &entry.path, &data)?;
        self.evict_in_background();
        Ok(path)
    }

    /// 곡 zip 전체를 받아 모든 파일을 캐시에 넣는다. `program`은 로그용 요청 프로그램.
    pub async fn ensure_song(&self, song: &SongInfo, program: &str) -> Result<()> {
        self.check_backoff()?;
        let lock = self.lock(Key::Song(song.song_id));
        let _guard = lock.lock().await;
        if song
            .files
            .iter()
            .all(|f| self.cache.contains(song.song_id, &f.path, f.size))
        {
            return Ok(());
        }
        // 잠금을 기다리는 동안 앞선 요청이 429를 받았을 수 있다.
        self.check_backoff()?;

        info!(
            song_id = song.song_id,
            size = song.zip_size,
            program,
            "downloading whole song (uses a ticket)"
        );
        let tmp = self.paths.tmp_file(&format!("song_{}", song.song_id));
        let result = async {
            let sha = self.api.play_song(song.song_id, &tmp).await?;
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
        if let Err(Error::NoTicket { retry_after }) = &result {
            // 알림 없이 로그에만 남긴다.
            warn!(
                song_id = song.song_id,
                retry_after, "no download ticket, pausing play downloads from this server"
            );
            self.pause(*retry_after);
        }
        result?;
        self.evict_in_background();
        Ok(())
    }

    /// 티켓을 기다리는 중이면 서버에 묻지 않고 바로 `NoTicket`을 돌려준다.
    fn check_backoff(&self) -> Result<()> {
        let mut retry_at = self.retry_at.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut at) = *retry_at else {
            return Ok(());
        };
        let now = unix_now();
        if at <= now {
            *retry_at = None;
            let _ = std::fs::remove_file(self.paths.no_ticket());
            return Ok(());
        }
        // 시계가 뒤로 가거나 파일이 바뀌어도 상한보다 오래 기다리지 않는다.
        if at - now > MAX_RETRY_AFTER {
            at = now + MAX_RETRY_AFTER;
            *retry_at = Some(at);
        }
        Err(Error::NoTicket {
            retry_after: at - now,
        })
    }

    /// `retry_after`초 동안 이 서버의 플레이 다운로드를 멈추고 데이터 폴더에 적어 둔다.
    fn pause(&self, retry_after: u64) {
        let at = unix_now().saturating_add(retry_after.clamp(1, MAX_RETRY_AFTER));
        *self.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
        if let Err(e) = std::fs::write(self.paths.no_ticket(), at.to_string()) {
            warn!(%e, "failed to save no-ticket backoff");
        }
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// 사전 API로 받은 파일이 매니페스트와 같은지 확인한다.
fn verify_entry(entry: &FileEntry, data: &[u8]) -> Result<()> {
    if data.len() as u64 != entry.size {
        return Err(Error::Integrity(format!(
            "{}: size {} != {}",
            entry.path,
            data.len(),
            entry.size
        )));
    }
    if Some(crc32fast::hash(data)) != entry.crc32_value() {
        return Err(Error::Integrity(format!("{}: crc32 mismatch", entry.path)));
    }
    Ok(())
}

fn extract_song(cache: &Cache, song: &SongInfo, zip_path: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    let sizes: HashMap<&str, u64> = song
        .files
        .iter()
        .map(|e| (e.path.as_str(), e.size))
        .collect();
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
        // 매니페스트에 있고 크기가 같은 파일만 이미 받은 것으로 본다.
        if sizes
            .get(name.as_str())
            .is_some_and(|&size| cache.contains(song.song_id, &name, size))
        {
            continue;
        }
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data)?;
        cache.put(song.song_id, &song.zip_sha256, &name, &data)?;
    }
    Ok(())
}
