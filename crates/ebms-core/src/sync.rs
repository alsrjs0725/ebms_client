//! 서버와 로컬 인덱스 동기화(= 사전 다운로드). 바뀐 청크만 받는다.
//!
//! 차트 청크, 매니페스트, 사전 청크(곡들의 배너·스테이지파일·프리뷰 등)를 받는다.
//! 나머지(키음·BGA)는 플레이할 때 곡 zip 전체로 받는다([`crate::fetch`]).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use futures_util::{StreamExt, stream};
use serde::Serialize;
use tracing::{info, warn};

use crate::api::Api;
use crate::index::{ChartRow, ChunkKind, Index, PreRow};
use crate::paths::{Paths, TmpFile};
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
    /// 받는 도중 받을 범위(난이도표)가 바뀌어 멈췄다. 받은 만큼은 반영돼 있고, 새 범위로 다시 동기화해야 한다.
    pub interrupted: bool,
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
    if version.api != crate::API_VERSION {
        return Err(Error::ApiVersion(version.api));
    }
    let mut report = SyncReport::default();
    // 범위를 읽기 전에 세대를 잡는다. 읽은 뒤 바뀌면 아래에서 멈춘다.
    let plan = Plan {
        index,
        start: index.selection_gen(),
    };
    let wanted = wanted_chunks(api, index).await?;
    let charts = wanted.as_ref().and_then(|w| w.charts.as_ref());
    sync_charts(api, paths, index, charts, &plan, &mut report).await?;
    sync_manifests(api, index, &mut report).await?;
    if !report.interrupted {
        sync_pre(
            api,
            paths,
            index,
            wanted.as_ref().map(|w| &w.pre),
            &plan,
            &mut report,
        )
        .await?;
    }
    if report.interrupted {
        info!(?report, "sync interrupted, download scope changed");
        return Ok(report);
    }
    if report.changed() {
        info!(?report, "sync finished");
    }
    Ok(report)
}

/// 동기화를 시작할 때 정한 받을 범위. 도중에 사용자가 표를 바꾸면 옛 범위로 계속 받지 않고 멈춘다.
struct Plan<'a> {
    index: &'a Index,
    start: u64,
}

impl Plan<'_> {
    fn stale(&self) -> bool {
        self.index.selection_gen() != self.start
    }
}

/// 난이도표를 골랐을 때 받을 청크. 고르지 않았으면(전곡) 없다.
struct WantedChunks {
    /// 고른 표의 태그가 붙은 사전 청크
    pre: BTreeSet<u32>,
    /// 받을 차트 청크. 서버가 차트 청크를 사전 청크와 같은 번호로 묶지 않았으면(이전 서버) 없고, 그때는 전부 받는다.
    charts: Option<BTreeSet<u32>>,
}

/// 고른 난이도표로 받을 청크를 정한다. 차트 청크는 고른 표의 사전 청크와 같은 번호에, 사용자가 직접 고른 표의 곡이 든
/// 번호를 더한다(그 곡도 구동기에 보이므로 차트가 있어야 한다).
async fn wanted_chunks(api: &Api, index: &Index) -> Result<Option<WantedChunks>> {
    let Some(selected) = index.pre_tables()? else {
        return Ok(None);
    };
    let tags = api.pre_tables().await?;
    let pre = tags.chunks_for(&selected);
    let charts = if tags.charts_by_pre {
        let mut charts = pre.clone();
        charts.extend(user_table_chunks(api, index).await?);
        Some(charts)
    } else {
        None
    };
    Ok(Some(WantedChunks { pre, charts }))
}

/// 사용자가 직접 고른 표의 곡이 든 사전 청크(= 차트 청크) 번호.
async fn user_table_chunks(api: &Api, index: &Index) -> Result<BTreeSet<u32>> {
    let songs: BTreeSet<u32> = index
        .user_tables()?
        .into_iter()
        .flat_map(|t| t.songs)
        .collect();
    if songs.is_empty() {
        return Ok(BTreeSet::new());
    }
    let songs: Vec<u32> = songs.into_iter().collect();
    Ok(api.song_chunks(&songs).await?.into_values().collect())
}

async fn sync_charts(
    api: &Api,
    paths: &Paths,
    index: &Index,
    wanted: Option<&BTreeSet<u32>>,
    plan: &Plan<'_>,
    report: &mut SyncReport,
) -> Result<()> {
    let mut remote = api.chart_hash().await?;
    // 난이도표를 골랐으면 그 표의 곡이 든 차트 청크만 받고, 나머지는 로컬에서도 뺀다(사전 청크와 같음).
    if let Some(wanted) = wanted {
        remote.retain(|id, _| wanted.contains(id));
    }
    let local = index.chunk_hashes(ChunkKind::Chart)?;

    // 클로저가 빌린 참조를 받으면 future가 `Send`로 증명되지 않아(앱의 `spawn`) 소유한 값으로 넘긴다.
    let todo: Vec<(u32, String)> = remote
        .iter()
        .filter(|&(id, sha)| !(local.get(id) == Some(sha) && paths.chart_chunk(*id, sha).exists()))
        .map(|(&id, sha)| (id, sha.clone()))
        .collect();
    let mut jobs = stream::iter(todo)
        .map(|(id, sha)| fetch_chunk(api, paths, Remote::Chart, id, sha, scan_chart_chunk))
        .buffer_unordered(CHUNK_CONCURRENCY);
    while let Some((id, fetched)) = jobs.next().await {
        let Some((sha, tmp, charts)) = fetched? else {
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
        // 새 판은 해시가 든 다른 이름으로 둔다. 옛 트리는 옛 파일을, 새 인덱스는 새 파일을 읽고
        // 옛 파일은 트리를 다시 만든 뒤 다음 정리([`tidy_chunks`])에서 지운다.
        tmp.persist(&dest)?;
        index.replace_chart_chunk(id, &sha, &charts)?;
        report.chart_chunks_updated.push(id);
        // 다 받은 청크는 반영하고, 나머지(받는 중인 것 포함)는 버린다. 옛 범위 밖 청크는 다음 동기화가 뺀다.
        if plan.stale() {
            report.interrupted = true;
            break;
        }
    }
    report.chart_chunks_updated.sort_unstable();
    report.chart_chunks_skipped.sort_unstable();
    // 범위가 바뀌었으면 옛 범위로 지우지 않는다. 새 범위로 다시 동기화할 때 정리한다.
    if report.interrupted || plan.stale() {
        report.interrupted = true;
        return Ok(());
    }

    // 파일은 다음 정리에서 지운다. 지금 트리가 아직 읽을 수 있다.
    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_chart_chunk(id)?;
        report.chart_chunks_removed.push(id);
    }
    Ok(())
}

async fn sync_pre(
    api: &Api,
    paths: &Paths,
    index: &Index,
    wanted: Option<&BTreeSet<u32>>,
    plan: &Plan<'_>,
    report: &mut SyncReport,
) -> Result<()> {
    let mut remote = api.pre_hash().await?;
    let local = index.chunk_hashes(ChunkKind::Pre)?;
    // 난이도표를 골랐으면 그 표의 태그가 붙은 청크만 받고, 나머지는 로컬에서도 뺀다.
    // 빠진 청크의 곡은 구동기에서 보이지 않는다([`crate::tree::Tree::open`]).
    if let Some(wanted) = wanted {
        remote.retain(|id, _| wanted.contains(id));
    }

    // 클로저가 빌린 참조를 받으면 future가 `Send`로 증명되지 않아(앱의 `spawn`) 소유한 값으로 넘긴다.
    let todo: Vec<(u32, String)> = remote
        .iter()
        .filter(|&(id, sha)| !(local.get(id) == Some(sha) && paths.pre_chunk(*id, sha).exists()))
        .map(|(&id, sha)| (id, sha.clone()))
        .collect();
    let mut jobs = stream::iter(todo)
        .map(|(id, sha)| fetch_chunk(api, paths, Remote::Pre, id, sha, scan_pre_chunk))
        .buffer_unordered(CHUNK_CONCURRENCY);
    while let Some((id, fetched)) = jobs.next().await {
        let Some((sha, tmp, files)) = fetched? else {
            warn!(chunk_id = id, "pre chunk kept changing on server, skipped");
            report.pre_chunks_skipped.push(id);
            continue;
        };
        let dest = paths.pre_chunk(id, &sha);
        if local.get(&id) == Some(&sha) && dest.exists() {
            continue;
        }
        // 차트 청크와 같이 해시가 든 새 이름으로 둔다.
        tmp.persist(&dest)?;
        index.replace_pre_chunk(id, &sha, &files)?;
        report.pre_chunks_updated.push(id);
        if plan.stale() {
            report.interrupted = true;
            break;
        }
    }
    report.pre_chunks_updated.sort_unstable();
    report.pre_chunks_skipped.sort_unstable();
    if report.interrupted || plan.stale() {
        report.interrupted = true;
        return Ok(());
    }

    for &id in local.keys().filter(|id| !remote.contains_key(id)) {
        index.remove_pre_chunk(id)?;
        report.pre_chunks_removed.push(id);
    }
    Ok(())
}

/// 다음 동기화에 받을 청크 용량. 동기화를 시작하기 전에 보여 준다.
/// 사전 청크는 난이도표 태그를 함께 둬서 고르는 표를 바꿔 가며 다시 셀 수 있다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PendingDownload {
    /// 아직 없거나 바뀐 차트 청크(난이도표와 무관하게 전부)
    pub chart_bytes: u64,
    pub chart_chunks: u32,
    /// 차트 청크가 사전 청크와 같은 번호로 묶여 있어 고른 표에 따라 받을 차트 청크가 달라지는지.
    /// 그러면 `charts`로 고른 표의 차트 용량을 센다.
    pub charts_by_pre: bool,
    /// 아직 없거나 바뀐 차트 청크별 크기와 태그(`charts_by_pre`일 때만)
    pub charts: Vec<PendingChartChunk>,
    /// 아직 없거나 바뀐 사전 청크(난이도표와 무관하게 전부)
    pub pre: Vec<PendingPreChunk>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingPreChunk {
    pub size: u64,
    /// 이 청크에 곡이 있는 난이도표 id
    pub tables: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingChartChunk {
    pub size: u64,
    /// 같은 번호의 사전 청크에 곡이 있는 난이도표 id
    pub tables: Vec<u32>,
    /// 사용자가 직접 고른 표의 곡이 있어 고른 표와 무관하게 받는 청크
    pub user: bool,
}

impl PendingDownload {
    /// 고른 난이도표(None이면 전곡)로 동기화하면 받을 차트 청크의 (바이트, 개수).
    pub fn chart_for(&self, selected: Option<&[u32]>) -> (u64, u32) {
        let Some(sel) = selected.filter(|_| self.charts_by_pre) else {
            return (self.chart_bytes, self.chart_chunks);
        };
        self.charts
            .iter()
            .filter(|c| c.user || c.tables.iter().any(|t| sel.contains(t)))
            .fold((0, 0), |(bytes, n), c| (bytes + c.size, n + 1))
    }

    /// 고른 난이도표(None이면 전곡)로 동기화하면 받을 사전 청크의 (바이트, 개수).
    pub fn pre_for(&self, selected: Option<&[u32]>) -> (u64, u32) {
        self.pre
            .iter()
            .filter(|c| selected.is_none_or(|sel| c.tables.iter().any(|t| sel.contains(t))))
            .fold((0, 0), |(bytes, n), c| (bytes + c.size, n + 1))
    }
}

/// 지금 동기화하면 받을 청크 용량을 센다. 로컬에 같은 해시의 파일이 있는 청크는 뺀다.
pub async fn pending_download(api: &Api, paths: &Paths, index: &Index) -> Result<PendingDownload> {
    let sizes = api.chunk_sizes().await?;
    let mut out = PendingDownload::default();
    if sizes.asset.is_empty() {
        count_charts(&sizes.chart, paths, index, &mut out, None)?;
        return Ok(out);
    }
    let pre_tables = api.pre_tables().await?;
    let tags = pre_tables.chunks;
    if pre_tables.charts_by_pre {
        out.charts_by_pre = true;
        let user = user_table_chunks(api, index).await?;
        count_charts(&sizes.chart, paths, index, &mut out, Some((&tags, &user)))?;
    } else {
        count_charts(&sizes.chart, paths, index, &mut out, None)?;
    }
    let local = index.chunk_hashes(ChunkKind::Pre)?;
    for (id, c) in &sizes.asset {
        if local.get(id) == Some(&c.sha256) && paths.pre_chunk(*id, &c.sha256).exists() {
            continue;
        }
        out.pre.push(PendingPreChunk {
            size: c.size,
            tables: tags.get(id).cloned().unwrap_or_default(),
        });
    }
    Ok(out)
}

/// (사전 청크 → 난이도표 태그, 사용자가 고른 표의 곡이 든 청크)
type ChunkTags<'a> = (
    &'a std::collections::BTreeMap<u32, Vec<u32>>,
    &'a BTreeSet<u32>,
);

/// 아직 없거나 바뀐 차트 청크를 센다. `tags`가 있으면(사전 청크와 같은 번호) 청크별로도 적는다.
fn count_charts(
    sizes: &std::collections::BTreeMap<u32, crate::api::ChunkSize>,
    paths: &Paths,
    index: &Index,
    out: &mut PendingDownload,
    tags: Option<ChunkTags<'_>>,
) -> Result<()> {
    let local = index.chunk_hashes(ChunkKind::Chart)?;
    for (id, c) in sizes {
        if local.get(id) == Some(&c.sha256) && paths.chart_chunk(*id, &c.sha256).exists() {
            continue;
        }
        out.chart_bytes += c.size;
        out.chart_chunks += 1;
        if let Some((tags, user)) = tags {
            out.charts.push(PendingChartChunk {
                size: c.size,
                tables: tags.get(id).cloned().unwrap_or_default(),
                user: user.contains(id),
            });
        }
    }
    Ok(())
}

/// 청크 폴더에서 인덱스가 가리키지 않는 파일(교체·삭제된 옛 판)을 지운다.
/// 옛 판을 읽는 트리가 없을 때(시작할 때, 다음 동기화 전) 부른다.
pub fn tidy_chunks(paths: &Paths, index: &Index) -> Result<()> {
    let kinds = [
        (
            ChunkKind::Chart,
            paths.charts(),
            Paths::chart_chunk as fn(&Paths, u32, &str) -> std::path::PathBuf,
        ),
        (ChunkKind::Pre, paths.pre(), Paths::pre_chunk),
    ];
    for (kind, dir, current) in kinds {
        let keep: HashSet<_> = index
            .chunk_hashes(kind)?
            .iter()
            .map(|(&id, sha)| current(paths, id, sha))
            .collect();
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

/// 청크를 동시에 받는 개수. 청크가 작아져(사전 8곡·차트 100개) 하나씩 받으면 왕복 지연이 쌓인다.
const CHUNK_CONCURRENCY: usize = 4;

/// 청크 하나를 임시 파일로 받아 스캔한다. 여러 개를 동시에 돌리고, 결과의 반영(옮기기·인덱스)은
/// 부르는 쪽이 하나씩 한다. 서버에서 계속 바뀌어 건너뛰면 `None`.
async fn fetch_chunk<T: Send + 'static>(
    api: &Api,
    paths: &Paths,
    remote: Remote,
    id: u32,
    sha: String,
    scan: fn(&Path, u32, &str) -> Result<T>,
) -> (u32, Result<Option<(String, TmpFile, T)>>) {
    let name = match remote {
        Remote::Chart => format!("chart_chunk_{id:05}"),
        Remote::Pre => format!("pre_chunk_{id:05}"),
    };
    let tmp = paths.tmp_file(&name);
    let result = async {
        let Some(sha) = download_stable_chunk(api, remote, id, sha, tmp.path()).await? else {
            return Ok(None);
        };
        let rows = {
            let tmp = tmp.path().to_path_buf();
            let sha = sha.clone();
            tokio::task::spawn_blocking(move || scan(&tmp, id, &sha))
                .await
                .map_err(|e| Error::Other(e.to_string()))??
        };
        Ok(Some((sha, rows)))
    }
    .await;
    (
        id,
        result.map(|got| got.map(|(sha, rows)| (sha, tmp, rows))),
    )
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

/// 앱은 동기화를 `spawn`으로 돌리므로 future가 `Send`여야 한다.
#[allow(dead_code)]
fn sync_all_is_send(api: &Api, paths: &Paths, index: &Index) {
    fn is_send<T: Send>(_: T) {}
    is_send(sync_all(api, paths, index));
}
