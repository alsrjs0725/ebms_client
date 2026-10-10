//! 서버와 로컬 인덱스 동기화(= 사전 다운로드). 바뀐 청크만 받는다.
//!
//! 차트 청크, 매니페스트, 사전 청크(곡들의 배너·스테이지파일·프리뷰 등)를 받는다.
//! 나머지(키음·BGA)는 플레이할 때 곡 zip 전체로 받는다([`crate::fetch`]).

use std::collections::HashSet;
use std::path::Path;

use tracing::{info, warn};

use crate::api::Api;
use crate::index::{ChartRow, ChunkKind, Index, PreRow};
use crate::paths::Paths;
use crate::{Error, Result};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub chart_chunks_updated: Vec<u32>,
    pub chart_chunks_removed: Vec<u32>,
    pub manifest_chunks_updated: Vec<u32>,
    pub manifest_chunks_removed: Vec<u32>,
    /// 받는 동안 서버에서 계속 바뀐 청크(곡 추가 중). 기존 로컬본을 두고 다음 동기화에 다시 받는다.
    pub chart_chunks_skipped: Vec<u32>,
    pub pre_chunks_updated: Vec<u32>,
    pub pre_chunks_removed: Vec<u32>,
    /// 차트 청크와 같은 이유로 건너뛴 사전 청크.
    pub pre_chunks_skipped: Vec<u32>,
}

impl SyncReport {
    pub fn changed(&self) -> bool {
        !(self.chart_chunks_updated.is_empty()
            && self.chart_chunks_removed.is_empty()
            && self.manifest_chunks_updated.is_empty()
            && self.manifest_chunks_removed.is_empty()
            && self.pre_chunks_updated.is_empty()
            && self.pre_chunks_removed.is_empty())
    }
}

pub async fn sync_all(api: &Api, paths: &Paths, index: &Index) -> Result<SyncReport> {
    let version = api.version().await?;
    if !crate::API_VERSIONS.contains(&version.api) {
        return Err(Error::ApiVersion(version.api));
    }
    let mut report = SyncReport::default();
    sync_charts(api, paths, index, &mut report).await?;
    sync_manifests(api, index, &mut report).await?;
    if version.api >= crate::PRE_CHUNK_API {
        sync_pre(api, paths, index, &mut report).await?;
    }
    if report.changed() {
        info!(?report, "sync finished");
    }
    Ok(report)
}

async fn sync_charts(
    api: &Api,
    paths: &Paths,
    index: &Index,
    report: &mut SyncReport,
) -> Result<()> {
    let remote = api.chart_hash().await?;
    let local = index.chunk_hashes(ChunkKind::Chart)?;

    for (&id, sha) in &remote {
        if local.get(&id) == Some(sha) && paths.chart_chunk(id, sha).exists() {
            continue;
        }
        let tmp = paths.tmp_file(&format!("chart_chunk_{id:05}"));
        let Some(sha) =
            download_stable_chunk(api, Remote::Chart, id, sha.clone(), tmp.path()).await?
        else {
            warn!(
                chunk_id = id,
                "chart chunk kept changing on server, skipped"
            );
            report.chart_chunks_skipped.push(id);
            continue;
        };
        let dest = paths.chart_chunk(id, &sha);
        if local.get(&id) == Some(&sha) && dest.exists() {
            continue;
        }
        let charts = {
            let tmp = tmp.path().to_path_buf();
            let sha = sha.clone();
            tokio::task::spawn_blocking(move || scan_chart_chunk(&tmp, id, &sha))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        // 새 판은 해시가 든 다른 이름으로 둔다. 옛 트리는 옛 파일을, 새 인덱스는 새 파일을 읽고
        // 옛 파일은 트리를 다시 만든 뒤 다음 정리([`tidy_chunks`])에서 지운다.
        tmp.persist(&dest)?;
        index.replace_chart_chunk(id, &sha, &charts)?;
        report.chart_chunks_updated.push(id);
    }

    // 파일은 다음 정리에서 지운다. 지금 트리가 아직 읽을 수 있다.
    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_chart_chunk(id)?;
        report.chart_chunks_removed.push(id);
    }
    Ok(())
}

async fn sync_pre(api: &Api, paths: &Paths, index: &Index, report: &mut SyncReport) -> Result<()> {
    let remote = api.pre_hash().await?;
    let local = index.chunk_hashes(ChunkKind::Pre)?;

    for (&id, sha) in &remote {
        if local.get(&id) == Some(sha) && paths.pre_chunk(id, sha).exists() {
            continue;
        }
        let tmp = paths.tmp_file(&format!("pre_chunk_{id:05}"));
        let Some(sha) =
            download_stable_chunk(api, Remote::Pre, id, sha.clone(), tmp.path()).await?
        else {
            warn!(chunk_id = id, "pre chunk kept changing on server, skipped");
            report.pre_chunks_skipped.push(id);
            continue;
        };
        let dest = paths.pre_chunk(id, &sha);
        if local.get(&id) == Some(&sha) && dest.exists() {
            continue;
        }
        let files = {
            let tmp = tmp.path().to_path_buf();
            let sha = sha.clone();
            tokio::task::spawn_blocking(move || scan_pre_chunk(&tmp, id, &sha))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        // 차트 청크와 같이 해시가 든 새 이름으로 둔다.
        tmp.persist(&dest)?;
        index.replace_pre_chunk(id, &sha, &files)?;
        report.pre_chunks_updated.push(id);
    }

    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_pre_chunk(id)?;
        report.pre_chunks_removed.push(id);
    }
    Ok(())
}

/// 청크 폴더를 인덱스에 맞춘다. 해시 없는 이전 이름은 인덱스 판 이름으로 옮기고,
/// 인덱스가 가리키지 않는 파일(교체·삭제된 옛 판)은 지운다.
/// 옛 판을 읽는 트리가 없을 때(시작할 때, 다음 동기화 전) 부른다.
pub fn tidy_chunks(paths: &Paths, index: &Index) -> Result<()> {
    let kinds = [
        (
            ChunkKind::Chart,
            paths.charts(),
            Paths::chart_chunk as fn(&Paths, u32, &str) -> std::path::PathBuf,
            Paths::legacy_chart_chunk as fn(&Paths, u32) -> std::path::PathBuf,
        ),
        (
            ChunkKind::Pre,
            paths.pre(),
            Paths::pre_chunk,
            Paths::legacy_pre_chunk,
        ),
    ];
    for (kind, dir, current, legacy) in kinds {
        let mut keep = HashSet::new();
        for (id, sha) in index.chunk_hashes(kind)? {
            let dest = current(paths, id, &sha);
            let old = legacy(paths, id);
            if !dest.exists() && old.exists() {
                std::fs::rename(&old, &dest)?;
            }
            keep.insert(dest);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if keep.contains(&path) || !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => info!(?path, "removed old chunk file"),
                // 읽는 중이라 못 지우면(Windows) 다음에 다시 한다.
                Err(e) => warn!(?path, %e, "old chunk file removal failed"),
            }
        }
    }
    Ok(())
}

/// 해시 목록이 있는 zip 청크 종류.
#[derive(Clone, Copy, Debug)]
enum Remote {
    Chart,
    Pre,
}

impl Remote {
    async fn hashes(self, api: &Api) -> Result<std::collections::BTreeMap<u32, String>> {
        match self {
            Remote::Chart => api.chart_hash().await,
            Remote::Pre => api.pre_hash().await,
        }
    }

    async fn download(self, api: &Api, id: u32, dest: &Path) -> Result<String> {
        match self {
            Remote::Chart => api.download_chart_chunk(id, dest).await,
            Remote::Pre => api.download_pre_chunk(id, dest).await,
        }
    }
}

/// 서버의 마지막 청크는 곡을 추가할 때마다 다시 써진다. 받는 도중 바뀌면 해시가 어긋나므로
/// 해시 목록을 다시 받아 바뀌었으면 새 해시로 다시 받는다. 끝까지 바뀌면 `None`.
/// 해시가 그대로인데 어긋나면 진짜 손상이므로 에러.
async fn download_stable_chunk(
    api: &Api,
    remote: Remote,
    id: u32,
    mut sha: String,
    tmp: &Path,
) -> Result<Option<String>> {
    for _ in 0..CHUNK_ATTEMPTS {
        let got = remote.download(api, id, tmp).await?;
        if got == sha {
            return Ok(Some(sha));
        }
        match remote.hashes(api).await?.remove(&id) {
            Some(now) if now == sha => {
                return Err(Error::Integrity(format!(
                    "{remote:?} chunk {id}: expected {sha}, got {got}"
                )));
            }
            Some(now) => sha = now,
            None => return Ok(None),
        }
    }
    Ok(None)
}

const CHUNK_ATTEMPTS: usize = 3;

async fn sync_manifests(api: &Api, index: &Index, report: &mut SyncReport) -> Result<()> {
    let remote = api.manifest_hash().await?;
    let local = index.chunk_hashes(ChunkKind::Manifest)?;

    for (&id, sha) in &remote {
        if local.get(&id) == Some(sha) {
            continue;
        }
        let (got, songs) = api.manifest(id).await?;
        if &got != sha {
            return Err(Error::Integrity(format!(
                "manifest {id}: expected {sha}, got {got}"
            )));
        }
        index.replace_manifest_chunk(id, sha, &songs)?;
        report.manifest_chunks_updated.push(id);
    }

    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_manifest_chunk(id)?;
        report.manifest_chunks_removed.push(id);
    }
    Ok(())
}

/// 차트 청크 zip의 항목(`{sha256}{ext}`, 무압축)을 읽어 인덱스 행으로 만든다.
pub fn scan_chart_chunk(path: &Path, chunk_id: u32, chunk_sha256: &str) -> Result<Vec<ChartRow>> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    let mut out = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i)?;
        let name = entry.name().to_string();
        let Some((sha, ext)) = name.rsplit_once('.') else {
            warn!(chunk_id, name, "unexpected entry name in chart chunk");
            continue;
        };
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            warn!(chunk_id, name, "unexpected entry name in chart chunk");
            continue;
        }
        if entry.compression() != zip::CompressionMethod::Stored {
            warn!(chunk_id, name, "compressed entry in chart chunk, skipped");
            continue;
        }
        let Some(data_offset) = entry.data_start() else {
            warn!(chunk_id, name, "no data offset for chart entry");
            continue;
        };
        out.push(ChartRow {
            sha256: sha.to_ascii_lowercase(),
            ext: ext.to_ascii_lowercase(),
            size: entry.size(),
            crc32: entry.crc32(),
            chunk_id,
            chunk_sha256: chunk_sha256.to_string(),
            data_offset,
        });
    }
    Ok(out)
}

/// 사전 청크 zip의 항목(`{song_id}/{곡 zip 안 경로}`, 무압축)을 읽어 인덱스 행으로 만든다.
pub fn scan_pre_chunk(path: &Path, chunk_id: u32, chunk_sha256: &str) -> Result<Vec<PreRow>> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    let mut out = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i)?;
        let name = entry.name().to_string();
        let Some((song_id, rel)) = name
            .split_once('/')
            .and_then(|(id, rel)| Some((id.parse::<u32>().ok()?, rel)))
            .filter(|(_, rel)| !rel.is_empty())
        else {
            warn!(chunk_id, name, "unexpected entry name in pre chunk");
            continue;
        };
        if entry.compression() != zip::CompressionMethod::Stored {
            warn!(chunk_id, name, "compressed entry in pre chunk, skipped");
            continue;
        }
        let Some(data_offset) = entry.data_start() else {
            warn!(chunk_id, name, "no data offset for pre entry");
            continue;
        };
        out.push(PreRow {
            song_id,
            path: rel.to_string(),
            size: entry.size(),
            crc32: entry.crc32(),
            chunk_id,
            chunk_sha256: chunk_sha256.to_string(),
            data_offset,
        });
    }
    Ok(out)
}
