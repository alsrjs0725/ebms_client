//! ebms_server API를 흉내 내는 목 서버로 동기화·가상 FS·다운로드를 검증한다.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ebms_core::fs::{EbmsFs, Kind};
use ebms_core::manifest::{FileEntry, SongManifest};
use ebms_core::{Client, Options};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[derive(Default)]
struct Server {
    chart_chunks: HashMap<u32, Vec<u8>>,
    manifests: HashMap<u32, Vec<u8>>,
    songs: HashMap<u32, Vec<u8>>,
}

#[derive(Default)]
struct Counters {
    song_full: AtomicUsize,
    song_range: AtomicUsize,
    chart_chunk: AtomicUsize,
    manifest: AtomicUsize,
}

#[derive(Clone)]
struct AppState {
    server: Arc<Mutex<Server>>,
    counters: Arc<Counters>,
}

fn hashes(map: &HashMap<u32, Vec<u8>>) -> axum::Json<HashMap<String, String>> {
    axum::Json(map.iter().map(|(k, v)| (k.to_string(), sha(v))).collect())
}

/// 서버 `blob_response`의 Range / If-Range 동작.
fn blob(headers: &HeaderMap, data: &[u8], counters: &Counters) -> Response {
    let etag = format!("\"{}\"", sha(data));
    let if_range_ok = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v.to_str().unwrap() == etag);
    if let (true, Some(range)) = (if_range_ok, headers.get(header::RANGE)) {
        let r = range.to_str().unwrap().strip_prefix("bytes=").unwrap();
        let (a, b) = r.split_once('-').unwrap();
        let start: usize = a.parse().unwrap();
        let end: usize = b.parse::<usize>().unwrap().min(data.len() - 1);
        counters.song_range.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::PARTIAL_CONTENT,
            [
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", data.len()),
                ),
                (header::ETAG, etag),
            ],
            data[start..=end].to_vec(),
        )
            .into_response();
    }
    counters.song_full.fetch_add(1, Ordering::SeqCst);
    ([(header::ETAG, etag)], data.to_vec()).into_response()
}

fn spawn_server(rt: &tokio::runtime::Runtime, state: AppState) -> String {
    let app = Router::new()
        .route("/api/version", get(|| async { axum::Json(serde_json::json!({"api": 1, "server": "test"})) }))
        .route(
            "/api/charthash",
            get(|State(s): State<AppState>| async move { hashes(&s.server.lock().unwrap().chart_chunks) }),
        )
        .route(
            "/api/manifest/hash",
            get(|State(s): State<AppState>| async move { hashes(&s.server.lock().unwrap().manifests) }),
        )
        .route(
            "/api/manifest/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>| async move {
                s.counters.manifest.fetch_add(1, Ordering::SeqCst);
                s.server.lock().unwrap().manifests[&id].clone()
            }),
        )
        .route(
            "/api/files/chart/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>| async move {
                s.counters.chart_chunk.fetch_add(1, Ordering::SeqCst);
                s.server.lock().unwrap().chart_chunks[&id].clone()
            }),
        )
        .route(
            "/api/files/song/id/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>, headers: HeaderMap| async move {
                let data = s.server.lock().unwrap().songs[&id].clone();
                blob(&headers, &data, &s.counters)
            }),
        )
        .with_state(state);
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Fixture {
    chart_a: Vec<u8>,
    chart_b: Vec<u8>,
    files: Vec<(&'static str, Vec<u8>)>,
}

fn noise(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed))
        .collect()
}

fn fixture() -> Fixture {
    let chart_a = b"#TITLE Test\r\n#ARTIST Someone\r\n#WAV01 bgm01.wav\r\n".to_vec();
    let chart_b = b"#TITLE Test [ANOTHER]\r\n".to_vec();
    Fixture {
        files: vec![
            ("_7a.bme", chart_a.clone()),
            ("bgm01.wav", noise(3, 50_000)),
            ("bgm02.wav", noise(5, 40_000)),
            ("bgm03.wav", noise(7, 30_000)),
            ("banner.png", noise(11, 5_000)),
            ("bga/movie.mp4", noise(13, 60_000)),
        ],
        chart_a,
        chart_b,
    }
}

fn make_zip(files: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(method);
    for (name, data) in files {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn song_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    make_zip(&files, zip::CompressionMethod::Deflated)
}

fn chart_chunk(charts: &[(&str, &[u8])]) -> Vec<u8> {
    make_zip(charts, zip::CompressionMethod::Stored)
}

/// 서버의 `zip_entries`와 같은 형식.
fn entries(zip_bytes: &[u8]) -> Vec<FileEntry> {
    let mut z = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let f = z.by_index_raw(i).unwrap();
            FileEntry {
                path: f.name().to_string(),
                size: f.size(),
                offset: f.header_start(),
                comp_size: f.compressed_size(),
                crc32: format!("{:08x}", f.crc32()),
                method: if f.compression() == zip::CompressionMethod::Stored {
                    0
                } else {
                    8
                },
            }
        })
        .collect()
}

struct Env {
    rt: tokio::runtime::Runtime,
    state: AppState,
    client: Client,
    fixture: Fixture,
    _dir: tempfile::TempDir,
}

fn setup() -> Env {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let fixture = fixture();
    let zip = song_zip(&fixture.files);
    let sha_a = sha(&fixture.chart_a);
    let sha_b = sha(&fixture.chart_b);
    let manifest = vec![SongManifest {
        song_id: 1,
        folder: "Artist - Title".into(),
        zip_size: zip.len() as u64,
        zip_sha256: sha(&zip),
        // chart_b는 곡 zip에 없는, 나중에 연결된 차트
        charts: vec![sha_a.clone(), sha_b.clone()],
        files: entries(&zip),
    }];

    let mut server = Server::default();
    server.chart_chunks.insert(
        0,
        chart_chunk(&[
            (&format!("{sha_a}.bme"), &fixture.chart_a),
            (&format!("{sha_b}.bme"), &fixture.chart_b),
        ]),
    );
    server
        .manifests
        .insert(0, serde_json::to_vec(&manifest).unwrap());
    server.songs.insert(1, zip);

    let state = AppState {
        server: Arc::new(Mutex::new(server)),
        counters: Arc::default(),
    };
    let url = spawn_server(&rt, state.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(url, dir.path());
    opts.promote_after = 3;
    let client = Client::open(&opts).unwrap();
    Env {
        rt,
        state,
        client,
        fixture,
        _dir: dir,
    }
}

/// 가상 FS 백엔드처럼 런타임 밖 스레드에서 작은 단위로 읽는다.
fn read_all(fs: &EbmsFs, path: &str) -> Vec<u8> {
    let attr = fs.resolve(path).unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(attr.kind, Kind::File);
    let mut out = Vec::new();
    while (out.len() as u64) < attr.size {
        let buf = fs.read(attr.ino, out.len() as u64, 7_000).unwrap();
        assert!(!buf.is_empty());
        out.extend(buf);
    }
    out
}

const SONG: &str = "00001 Artist - Title";

#[test]
fn sync_is_incremental() {
    let env = setup();
    let first = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(first.chart_chunks_updated, vec![0]);
    assert_eq!(first.manifest_chunks_updated, vec![0]);

    let second = env.rt.block_on(env.client.sync()).unwrap();
    assert!(!second.changed());
    assert_eq!(env.state.counters.chart_chunk.load(Ordering::SeqCst), 1);
    assert_eq!(env.state.counters.manifest.load(Ordering::SeqCst), 1);

    // 서버에 청크가 추가되면 그 청크만 받는다
    let new_chart = b"#TITLE New\r\n".to_vec();
    let sha_new = sha(&new_chart);
    env.state
        .server
        .lock()
        .unwrap()
        .chart_chunks
        .insert(1, chart_chunk(&[(&format!("{sha_new}.bms"), &new_chart)]));
    let third = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(third.chart_chunks_updated, vec![1]);
    assert_eq!(env.state.counters.chart_chunk.load(Ordering::SeqCst), 2);
    assert_eq!(env.client.index.charts().unwrap().len(), 3);
}

#[test]
fn tree_and_chart_reads_need_no_download() {
    let env = setup();
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();

    let names: Vec<_> = fs
        .readdir(fs.resolve(SONG).unwrap().ino)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    let linked = format!("{}.bme", sha(&env.fixture.chart_b));
    for want in ["_7a.bme", "bgm01.wav", "banner.png", "bga", linked.as_str()] {
        assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
    }

    assert_eq!(
        read_all(&fs, &format!("{SONG}/_7a.bme")),
        env.fixture.chart_a
    );
    assert_eq!(
        read_all(&fs, &format!("{SONG}/{linked}")),
        env.fixture.chart_b
    );
    assert_eq!(
        fs.resolve(&format!("{SONG}/bgm01.wav")).unwrap().size,
        50_000
    );

    let c = &env.state.counters;
    assert_eq!(
        c.song_full.load(Ordering::SeqCst) + c.song_range.load(Ordering::SeqCst),
        0
    );
}

#[test]
fn light_files_use_range_and_heavy_files_promote_to_full_song() {
    let env = setup();
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    let c = &env.state.counters;
    let want = |name: &str| {
        env.fixture
            .files
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap()
            .1
            .clone()
    };

    // 배너: 파일 단위
    assert_eq!(
        read_all(&fs, &format!("{SONG}/banner.png")),
        want("banner.png")
    );
    assert_eq!(c.song_full.load(Ordering::SeqCst), 0);
    assert!(c.song_range.load(Ordering::SeqCst) >= 1);

    // 대소문자가 다른 이름으로 요청해도 같은 파일
    assert_eq!(
        read_all(&fs, &format!("{SONG}/BGM01.WAV")),
        want("bgm01.wav")
    );
    assert_eq!(
        read_all(&fs, &format!("{SONG}/bgm02.wav")),
        want("bgm02.wav")
    );
    assert_eq!(c.song_full.load(Ordering::SeqCst), 0);

    // 세 번째 무거운 파일에서 곡 전체 다운로드
    assert_eq!(
        read_all(&fs, &format!("{SONG}/bgm03.wav")),
        want("bgm03.wav")
    );
    assert_eq!(c.song_full.load(Ordering::SeqCst), 1);

    // 이후는 캐시에서
    let before = c.song_range.load(Ordering::SeqCst);
    assert_eq!(
        read_all(&fs, &format!("{SONG}/bga/movie.mp4")),
        want("bga/movie.mp4")
    );
    assert_eq!(c.song_range.load(Ordering::SeqCst), before);
    assert_eq!(c.song_full.load(Ordering::SeqCst), 1);
}

#[test]
fn changed_song_on_server_is_an_error_not_wrong_bytes() {
    let env = setup();
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();

    // 매니페스트가 갱신되기 전에 서버 zip만 바뀐 상황
    let mut files = env.fixture.files.clone();
    files[4].1 = vec![0u8; 5_000];
    env.state
        .server
        .lock()
        .unwrap()
        .songs
        .insert(1, song_zip(&files));

    let attr = fs.resolve(&format!("{SONG}/banner.png")).unwrap();
    assert!(fs.read(attr.ino, 0, 100).is_err());
    assert_eq!(env.state.counters.song_full.load(Ordering::SeqCst), 1); // 200 응답을 받았지만 본문은 쓰지 않음
}
