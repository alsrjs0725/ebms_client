//! 서버와 로컬 인덱스 동기화. 바뀐 청크만 받는다.

use std::path::Path;

use tracing::{info, warn};

use crate::api::Api;
use crate::index::{ChartRow, ChunkKind, Index};
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
}

impl SyncReport {
    pub fn changed(&self) -> bool {
        !(self.chart_chunks_updated.is_empty()
            && self.chart_chunks_removed.is_empty()
            && self.manifest_chunks_updated.is_empty()
            && self.manifest_chunks_removed.is_empty())
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
        let dest = paths.chart_chunk(id);
        if local.get(&id) == Some(sha) && dest.exists() {
            continue;
        }
        let tmp = paths.tmp_file(&format!("chart_chunk_{id:05}"));
        let Some(sha) = download_stable_chart_chunk(api, id, sha.clone(), &tmp).await? else {
            warn!(
                chunk_id = id,
                "chart chunk kept changing on server, skipped"
            );
            report.chart_chunks_skipped.push(id);
            continue;
        };
        if local.get(&id) == Some(&sha) && dest.exists() {
            let _ = std::fs::remove_file(&tmp);
            continue;
        }
        let charts = {
            let tmp = tmp.clone();
            tokio::task::spawn_blocking(move || scan_chart_chunk(&tmp, id))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        std::fs::rename(&tmp, &dest)?;
        index.replace_chart_chunk(id, &sha, &charts)?;
        report.chart_chunks_updated.push(id);
    }

    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_chart_chunk(id)?;
        let _ = std::fs::remove_file(paths.chart_chunk(id));
        report.chart_chunks_removed.push(id);
    }
    Ok(())
}

/// 서버의 마지막 청크는 곡을 추가할 때마다 다시 써진다. 받는 도중 바뀌면 해시가 어긋나므로
/// 해시 목록을 다시 받아 바뀌었으면 새 해시로 다시 받는다. 끝까지 바뀌면 `None`.
/// 해시가 그대로인데 어긋나면 진짜 손상이므로 에러.
async fn download_stable_chart_chunk(
    api: &Api,
    id: u32,
    mut sha: String,
    tmp: &Path,
) -> Result<Option<String>> {
    for _ in 0..CHART_CHUNK_ATTEMPTS {
        let got = api.download_chart_chunk(id, tmp).await?;
        if got == sha {
            return Ok(Some(sha));
        }
        let _ = std::fs::remove_file(tmp);
        match api.chart_hash().await?.remove(&id) {
            Some(now) if now == sha => {
                return Err(Error::Integrity(format!(
                    "chart chunk {id}: expected {sha}, got {got}"
                )));
            }
            Some(now) => sha = now,
            None => return Ok(None),
        }
    }
    Ok(None)
}

const CHART_CHUNK_ATTEMPTS: usize = 3;

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
pub fn scan_chart_chunk(path: &Path, chunk_id: u32) -> Result<Vec<ChartRow>> {
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
            data_offset,
        });
    }
    Ok(out)
}
