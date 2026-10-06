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
    if version.api != crate::API_VERSION {
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
        let got = api.download_chart_chunk(id, &tmp).await?;
        if &got != sha {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Integrity(format!(
                "chart chunk {id}: expected {sha}, got {got}"
            )));
        }
        let charts = {
            let tmp = tmp.clone();
            tokio::task::spawn_blocking(move || scan_chart_chunk(&tmp, id))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        std::fs::rename(&tmp, &dest)?;
        index.replace_chart_chunk(id, sha, &charts)?;
        report.chart_chunks_updated.push(id);
    }

    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_chart_chunk(id)?;
        let _ = std::fs::remove_file(paths.chart_chunk(id));
        report.chart_chunks_removed.push(id);
    }
    Ok(())
}

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
