//! ebms_server API를 흉내 내는 목 서버로 로그인·동기화·가상 FS·다운로드를 검증한다.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ebms_core::api::Api;
use ebms_core::auth::LoginRequest;
use ebms_core::config::AppDir;
use ebms_core::fs::{Caller, Kind, ReadOnlyFs, Trusted};
use ebms_core::hub::Hub;
use ebms_core::manifest::{FileEntry, FileKind, SongManifest};
use ebms_core::{Client, Error, Options};
use serde_json::json;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

const KEY: &str = "test-session-key";

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[derive(Default)]
struct Server {
    /// `/api/version`의 api. 3부터 사전 청크가 있다.
    api: u32,
    chart_chunks: HashMap<u32, Vec<u8>>,
    pre_chunks: HashMap<u32, Vec<u8>>,
    manifests: HashMap<u32, Vec<u8>>,
    songs: HashMap<u32, Vec<u8>>,
    /// 사전 파일 경로
    pre_paths: HashSet<String>,
    /// 유효한 세션키
    sessions: HashSet<String>,
    /// 1회용 코드 → code_challenge
    codes: HashMap<String, String>,
    tickets: u32,
    /// 남은 횟수만큼 청크 다운로드 직전에 그 청크에 차트를 덧붙인다. 곡 추가 중인 서버 흉내.
    appends_on_download: usize,
    /// 켜 두면 청크 본문을 해시 목록과 다르게 망가뜨려 보낸다.
    corrupt_chart: bool,
}

#[derive(Default)]
struct Counters {
    play: AtomicUsize,
    pre_file: AtomicUsize,
    chart_chunk: AtomicUsize,
    pre_chunk: AtomicUsize,
    pre_hash: AtomicUsize,
    manifest: AtomicUsize,
}

/// 켜 두면 사전 파일 응답을 `release`까지 붙잡는다. 멈춘 다운로드 흉내.
#[derive(Default)]
struct Gate {
    hold_pre: AtomicBool,
    release: tokio::sync::Notify,
}

#[derive(Clone)]
struct AppState {
    server: Arc<Mutex<Server>>,
    counters: Arc<Counters>,
    gate: Arc<Gate>,
}

impl AppState {
    fn server(&self) -> std::sync::MutexGuard<'_, Server> {
        self.server.lock().unwrap()
    }

    /// 서버 `current_user`처럼 Bearer 세션키를 확인한다.
    fn authorized(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let key = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match key {
            Some(k) if self.server().sessions.contains(k) => Ok(()),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

fn hashes(map: &HashMap<u32, Vec<u8>>) -> HashMap<String, String> {
    map.iter().map(|(k, v)| (k.to_string(), sha(v))).collect()
}

fn me_json() -> serde_json::Value {
    json!({
        "id": "0b5f3c1e-0000-4000-8000-000000000001",
        "display_name": "Tester",
        "email": null,
        "role": "user",
        "oauths": [{"oauth": "google", "name": "tester@example.com"}],
        "session": {"kind": "client"},
        "tickets": {"available": 5, "max": 5, "refill_seconds": 60, "next_refill_at": null},
        "pre": {"month": "2026-10", "used_bytes": 0, "limit_bytes": 10737418240u64,
                "throttled_kbps": 500, "throttled": false},
    })
}

fn spawn_server(rt: &tokio::runtime::Runtime, state: AppState) -> String {
    let app = Router::new()
        .route(
            "/api/version",
            get(|State(s): State<AppState>| async move {
                let api = s.server().api;
                axum::Json(json!({"api": api, "server": "test", "auth": ["google", "discord"]}))
            }),
        )
        // ---- 로그인 (웹에는 이미 로그인돼 있다고 가정) ----
        .route(
            "/auth/client/authorize",
            get(|State(s): State<AppState>, Query(q): Query<HashMap<String, String>>| async move {
                let redirect = &q["redirect_uri"];
                assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
                assert_eq!(q["code_challenge_method"], "S256");
                assert!(!q["device_name"].is_empty());
                let code = format!("code-{}", s.server().codes.len());
                s.server().codes.insert(code.clone(), q["code_challenge"].clone());
                Redirect::to(&format!("{redirect}?code={code}&state={}", q["state"]))
            }),
        )
        .route(
            "/api/auth/client/token",
            post(|State(s): State<AppState>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                let code = body["code"].as_str().unwrap();
                let verifier = body["code_verifier"].as_str().unwrap();
                let Some(challenge) = s.server().codes.remove(code) else {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"detail": "invalid or expired code"})))
                        .into_response();
                };
                if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != challenge {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"detail": "invalid code_verifier"})))
                        .into_response();
                }
                s.server().sessions.insert(KEY.to_string());
                let user = me_json();
                axum::Json(json!({
                    "session_key": KEY,
                    "expires_at": 4102444800i64,
                    "user": {"id": user["id"], "display_name": user["display_name"],
                             "email": null, "role": "user"},
                }))
                .into_response()
            }),
        )
        .route(
            "/api/auth/client/logout",
            post(|State(s): State<AppState>, headers: HeaderMap| async move {
                if let Err(status) = s.authorized(&headers) {
                    return status.into_response();
                }
                s.server().sessions.clear();
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/api/me",
            get(|State(s): State<AppState>, headers: HeaderMap| async move {
                s.authorized(&headers).map(|_| axum::Json(me_json()))
            }),
        )
        // ---- 사전 다운로드 ----
        .route(
            "/api/pre/charthash",
            get(|State(s): State<AppState>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                Ok::<_, StatusCode>(axum::Json(hashes(&s.server().chart_chunks)))
            }),
        )
        .route(
            "/api/pre/assethash",
            get(|State(s): State<AppState>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                s.counters.pre_hash.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StatusCode>(axum::Json(hashes(&s.server().pre_chunks)))
            }),
        )
        .route(
            "/api/pre/asset/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                s.counters.pre_chunk.fetch_add(1, Ordering::SeqCst);
                s.server().pre_chunks.get(&id).cloned().ok_or(StatusCode::NOT_FOUND)
            }),
        )
        .route(
            "/api/pre/manifest/hash",
            get(|State(s): State<AppState>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                Ok::<_, StatusCode>(axum::Json(hashes(&s.server().manifests)))
            }),
        )
        .route(
            "/api/pre/manifest/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                s.counters.manifest.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StatusCode>(s.server().manifests[&id].clone())
            }),
        )
        .route(
            "/api/pre/chart/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>, headers: HeaderMap| async move {
                s.authorized(&headers)?;
                let n = s.counters.chart_chunk.fetch_add(1, Ordering::SeqCst);
                let mut server = s.server();
                if server.appends_on_download > 0 {
                    server.appends_on_download -= 1;
                    let chart = format!("#TITLE Added {n}\r\n").into_bytes();
                    let mut entries = zip_entries(&server.chart_chunks[&id]);
                    entries.push((format!("{}.bms", sha(&chart)), chart));
                    let refs: Vec<_> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
                    server.chart_chunks.insert(id, chart_chunk(&refs));
                }
                let mut body = server.chart_chunks[&id].clone();
                if server.corrupt_chart {
                    body.push(0);
                }
                Ok::<_, StatusCode>(body)
            }),
        )
        .route(
            "/api/pre/song/{id}/file",
            get(
                |State(s): State<AppState>,
                 Path(id): Path<u32>,
                 Query(q): Query<HashMap<String, String>>,
                 headers: HeaderMap| async move {
                    s.authorized(&headers)?;
                    s.counters.pre_file.fetch_add(1, Ordering::SeqCst);
                    if s.gate.hold_pre.load(Ordering::SeqCst) {
                        s.gate.release.notified().await;
                    }
                    let path = &q["path"];
                    let server = s.server();
                    if !server.pre_paths.contains(path) {
                        return Err(StatusCode::FORBIDDEN);
                    }
                    let mut zip = zip::ZipArchive::new(Cursor::new(&server.songs[&id])).unwrap();
                    let mut data = Vec::new();
                    zip.by_name(path).unwrap().read_to_end(&mut data).unwrap();
                    Ok(data)
                },
            ),
        )
        // ---- 플레이 다운로드 ----
        .route(
            "/api/play/song/{id}",
            get(|State(s): State<AppState>, Path(id): Path<u32>, headers: HeaderMap| async move {
                if let Err(status) = s.authorized(&headers) {
                    return status.into_response();
                }
                s.counters.play.fetch_add(1, Ordering::SeqCst);
                let mut server = s.server();
                if server.tickets == 0 {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "30")],
                        axum::Json(json!({"detail": "no download ticket"})),
                    )
                        .into_response();
                }
                server.tickets -= 1;
                server.songs[&id].clone().into_response()
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

const PRE_FILES: &[&str] = &["_7a.bme", "banner.png", "preview.ogg"];

fn noise(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed))
        .collect()
}

fn fixture() -> Fixture {
    let chart_a =
        b"#TITLE Test\r\n#ARTIST Someone\r\n#BANNER banner.png\r\n#WAV01 bgm01.wav\r\n".to_vec();
    let chart_b = b"#TITLE Test [ANOTHER]\r\n".to_vec();
    Fixture {
        files: vec![
            ("_7a.bme", chart_a.clone()),
            ("bgm01.wav", noise(3, 50_000)),
            ("bgm02.wav", noise(5, 40_000)),
            ("banner.png", noise(11, 5_000)),
            ("preview.ogg", noise(17, 8_000)),
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

/// zip의 항목 이름과 내용.
fn zip_entries(zip_bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut z = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            (f.name().to_string(), data)
        })
        .collect()
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
                kind: if PRE_FILES.contains(&f.name()) {
                    FileKind::Pre
                } else {
                    FileKind::Play
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
    url: String,
    _dir: tempfile::TempDir,
}

/// 곡 하나가 있는 서버 상태. 세션키 `KEY`는 이미 유효하다.
fn server_state() -> (AppState, Fixture) {
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

    let mut server = Server {
        api: 1,
        tickets: 5,
        pre_paths: PRE_FILES.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
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
    server.sessions.insert(KEY.to_string());

    let state = AppState {
        server: Arc::new(Mutex::new(server)),
        counters: Arc::default(),
        gate: Arc::default(),
    };
    (state, fixture)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// `logged_in`이면 클라이언트가 유효한 세션키를 갖고 시작한다.
fn setup(logged_in: bool) -> Env {
    let rt = runtime();
    let (state, fixture) = server_state();
    let url = spawn_server(&rt, state.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(&url, dir.path());
    opts.session = logged_in.then(|| KEY.to_string());
    let client = Client::open(&opts).unwrap();
    Env {
        rt,
        state,
        client,
        fixture,
        url,
        _dir: dir,
    }
}

impl Env {
    fn want(&self, name: &str) -> Vec<u8> {
        self.fixture
            .files
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap()
            .1
            .clone()
    }
}

/// 가상 FS 백엔드처럼 런타임 밖 스레드에서 작은 단위로 읽는다. 구동기가 읽는 것으로 본다.
fn read_all(fs: &dyn ReadOnlyFs, path: &str) -> ebms_core::Result<Vec<u8>> {
    read_all_as(fs, path, &Trusted("test"))
}

fn read_all_as(fs: &dyn ReadOnlyFs, path: &str, caller: &dyn Caller) -> ebms_core::Result<Vec<u8>> {
    let attr = fs.resolve(path).unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(attr.kind, Kind::File);
    let mut out = Vec::new();
    while (out.len() as u64) < attr.size {
        let buf = fs.read(attr.ino, out.len() as u64, 7_000, caller)?;
        assert!(!buf.is_empty());
        out.extend(buf);
    }
    Ok(out)
}

const SONG: &str = "00001 Artist - Title";

#[test]
fn browser_login_gets_session_key_and_logout_revokes_it() {
    let env = setup(false);
    env.state.server().sessions.clear();
    let api = Api::new(&env.url).unwrap();
    assert!(api.needs_login());

    let token = env.rt.block_on(async {
        let req = LoginRequest::start(&api, "test device").await.unwrap();
        // 브라우저 대신: 로그인 주소를 열면 서버가 루프백 주소로 리다이렉트한다.
        let browser = tokio::spawn({
            let url = req.url().to_string();
            async move {
                let resp = reqwest::get(url).await.unwrap();
                assert!(resp.url().as_str().starts_with("http://127.0.0.1:"));
                assert_eq!(resp.status(), 200);
                resp.text().await.unwrap()
            }
        });
        let token = req.finish(&api, Duration::from_secs(10)).await.unwrap();
        assert!(browser.await.unwrap().contains("로그인 완료"));
        token
    });
    assert_eq!(token.session_key, KEY);
    assert_eq!(token.user.display_name, "Tester");
    assert!(!api.needs_login());

    let me = env.rt.block_on(api.me()).unwrap();
    assert_eq!(me.tickets.available, 5);
    assert_eq!(me.oauths[0].oauth, "google");

    env.rt.block_on(api.logout()).unwrap();
    assert!(env.state.server().sessions.is_empty());
    assert!(api.needs_login());
}

#[test]
fn reused_code_is_rejected() {
    let env = setup(false);
    let api = Api::new(&env.url).unwrap();
    let err = env
        .rt
        .block_on(api.exchange_code("never-issued", "x".repeat(43).as_str()))
        .unwrap_err();
    assert!(
        matches!(err, Error::Auth(ref d) if d.contains("invalid")),
        "{err}"
    );
}

#[test]
fn requests_without_session_are_unauthorized() {
    let env = setup(false);
    let err = env.rt.block_on(env.client.sync()).unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err}");
    assert!(env.client.api.needs_login());

    // 서버가 세션을 폐기한 경우도 같다.
    let env = setup(true);
    env.state.server().sessions.clear();
    let err = env.rt.block_on(env.client.sync()).unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err}");
    assert!(env.client.api.needs_login());
}

#[test]
fn sync_is_incremental() {
    let env = setup(true);
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
        .server()
        .chart_chunks
        .insert(1, chart_chunk(&[(&format!("{sha_new}.bms"), &new_chart)]));
    let third = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(third.chart_chunks_updated, vec![1]);
    assert_eq!(env.state.counters.chart_chunk.load(Ordering::SeqCst), 2);
    assert_eq!(env.client.index.charts().unwrap().len(), 3);
}

#[test]
fn sync_retries_chart_chunk_changed_during_download() {
    let env = setup(true);
    env.state.server().appends_on_download = 1;
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(report.chart_chunks_updated, vec![0]);
    assert!(report.chart_chunks_skipped.is_empty());
    assert_eq!(env.state.counters.chart_chunk.load(Ordering::SeqCst), 2);
    assert_eq!(env.client.index.charts().unwrap().len(), 3);
    // 받은 해시가 서버 목록과 같으니 다음엔 받지 않는다
    assert!(!env.rt.block_on(env.client.sync()).unwrap().changed());
}

#[test]
fn sync_skips_chart_chunk_that_keeps_changing() {
    let env = setup(true);
    env.state.server().appends_on_download = usize::MAX;
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(report.chart_chunks_skipped, vec![0]);
    assert!(report.chart_chunks_updated.is_empty());
    assert_eq!(report.manifest_chunks_updated, vec![0]);

    // 곡 추가가 끝나면 다음 동기화에서 받는다
    env.state.server().appends_on_download = 0;
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(report.chart_chunks_updated, vec![0]);
}

#[test]
fn sync_rejects_corrupt_chart_chunk() {
    let env = setup(true);
    env.state.server().corrupt_chart = true;
    let err = env.rt.block_on(env.client.sync()).unwrap_err();
    assert!(matches!(err, Error::Integrity(_)), "{err}");
}

#[test]
fn tree_and_chart_reads_need_no_download() {
    let env = setup(true);
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
        read_all(&fs, &format!("{SONG}/_7a.bme")).unwrap(),
        env.fixture.chart_a
    );
    assert_eq!(
        read_all(&fs, &format!("{SONG}/{linked}")).unwrap(),
        env.fixture.chart_b
    );
    assert_eq!(
        fs.resolve(&format!("{SONG}/bgm01.wav")).unwrap().size,
        50_000
    );

    let c = &env.state.counters;
    assert_eq!(
        c.play.load(Ordering::SeqCst) + c.pre_file.load(Ordering::SeqCst),
        0
    );
}

#[test]
fn pre_files_alone_and_play_files_as_whole_song() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    let c = &env.state.counters;

    // 배너·프리뷰: 사전 API로 그 파일만, 티켓 없이
    for name in ["banner.png", "preview.ogg"] {
        assert_eq!(
            read_all(&fs, &format!("{SONG}/{name}")).unwrap(),
            env.want(name)
        );
    }
    assert_eq!(c.pre_file.load(Ordering::SeqCst), 2);
    assert_eq!(c.play.load(Ordering::SeqCst), 0);
    assert_eq!(env.state.server().tickets, 5);

    // 키음을 처음 열면 곡 zip 전체 (대소문자가 다른 이름도 같은 파일)
    assert_eq!(
        read_all(&fs, &format!("{SONG}/BGM01.WAV")).unwrap(),
        env.want("bgm01.wav")
    );
    assert_eq!(c.play.load(Ordering::SeqCst), 1);
    assert_eq!(env.state.server().tickets, 4);

    // 나머지 플레이 파일은 캐시에서
    for name in ["bgm02.wav", "bga/movie.mp4"] {
        assert_eq!(
            read_all(&fs, &format!("{SONG}/{name}")).unwrap(),
            env.want(name)
        );
    }
    assert_eq!(c.play.load(Ordering::SeqCst), 1);
    assert_eq!(c.pre_file.load(Ordering::SeqCst), 2);
}

#[test]
fn async_read_returns_before_download_finishes() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    env.state.gate.hold_pre.store(true, Ordering::SeqCst);

    let read = |path: &str| {
        let attr = fs.resolve(&format!("{SONG}/{path}")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        fs.read_async(
            attr.ino,
            0,
            attr.size as u32,
            Box::new(Trusted("test")),
            Box::new(move |r| tx.send(r).unwrap()),
        );
        rx
    };

    // 사전 파일 응답이 멈춰 있어도 호출은 바로 돌아오고 결과는 아직 없다.
    let preview = read("preview.ogg");
    assert!(
        preview.recv_timeout(Duration::from_millis(300)).is_err(),
        "should still be downloading"
    );
    // 그 사이 같은 스레드에서 다른 읽기는 그대로 끝난다.
    assert_eq!(
        read("_7a.bme").recv().unwrap().unwrap(),
        env.fixture.chart_a
    );

    env.state.gate.release.notify_one();
    assert_eq!(
        preview
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap(),
        env.want("preview.ogg")
    );
    // 받은 뒤에는 캐시에서 바로 끝난다.
    assert_eq!(
        read("preview.ogg").try_recv().unwrap().unwrap(),
        env.want("preview.ogg")
    );
}

/// 백업·인덱서처럼 구동기가 아닌 프로그램.
struct Indexer;

impl Caller for Indexer {
    fn name(&self) -> String {
        "indexer".into()
    }
    fn is_player(&self) -> bool {
        false
    }
}

#[test]
fn non_player_reads_never_download_the_song() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    let c = &env.state.counters;

    // 플레이 파일 16바이트만 읽어도 거절하고 티켓을 쓰지 않는다.
    let attr = fs.resolve(&format!("{SONG}/bga/movie.mp4")).unwrap();
    let err = fs.read(attr.ino, 0, 16, &Indexer).unwrap_err();
    assert!(matches!(err, Error::NotPlayer { .. }), "{err}");
    assert_eq!(c.play.load(Ordering::SeqCst), 0);
    assert_eq!(env.state.server().tickets, 5);

    // 사전 파일과 차트는 그대로 읽힌다.
    assert_eq!(
        read_all_as(&fs, &format!("{SONG}/banner.png"), &Indexer).unwrap(),
        env.want("banner.png")
    );

    // 구동기가 받은 뒤에는 캐시에서 누구나 읽는다.
    read_all(&fs, &format!("{SONG}/bgm01.wav")).unwrap();
    assert_eq!(
        read_all_as(&fs, &format!("{SONG}/bga/movie.mp4"), &Indexer).unwrap(),
        env.want("bga/movie.mp4")
    );
    assert_eq!(c.play.load(Ordering::SeqCst), 1);
    assert_eq!(env.state.server().tickets, 4);
}

#[test]
fn no_ticket_pauses_the_song_until_retry_after() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    env.state.server().tickets = 0;

    let path = format!("{SONG}/bgm01.wav");
    let err = read_all(&fs, &path).unwrap_err();
    assert!(matches!(err, Error::NoTicket { retry_after: 30 }), "{err}");
    assert_eq!(env.state.counters.play.load(Ordering::SeqCst), 1);

    // Retry-After 동안은 서버에 다시 묻지 않는다. 사전 파일은 그대로 받는다.
    env.state.server().tickets = 5;
    let err = read_all(&fs, &format!("{SONG}/bgm02.wav")).unwrap_err();
    assert!(matches!(err, Error::NoTicket { .. }), "{err}");
    assert_eq!(env.state.counters.play.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_all(&fs, &format!("{SONG}/banner.png")).unwrap(),
        env.want("banner.png")
    );
}

#[test]
fn changed_song_on_server_is_an_error_not_wrong_bytes() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();

    // 매니페스트가 갱신되기 전에 서버 zip만 바뀐 상황
    let mut files = env.fixture.files.clone();
    files[1].1 = vec![0u8; 50_000];
    files[3].1 = vec![0u8; 5_000];
    env.state.server().songs.insert(1, song_zip(&files));

    assert!(matches!(
        read_all(&fs, &format!("{SONG}/bgm01.wav")).unwrap_err(),
        Error::Integrity(_)
    ));
    assert!(matches!(
        read_all(&fs, &format!("{SONG}/banner.png")).unwrap_err(),
        Error::Integrity(_)
    ));
}

/// 서버에서 곡 1을 같은 경로·크기, 다른 내용의 곡으로 바꾼다(DB 초기화 후 id 재사용 흉내).
fn replace_song_on_server(env: &Env) -> Vec<(&'static str, Vec<u8>)> {
    let mut files = env.fixture.files.clone();
    for (_, data) in files.iter_mut().skip(1) {
        data.iter_mut().for_each(|b| *b = b.wrapping_add(1));
    }
    let zip = song_zip(&files);
    let mut server = env.state.server();
    let mut manifest: Vec<SongManifest> = serde_json::from_slice(&server.manifests[&0]).unwrap();
    manifest[0].zip_size = zip.len() as u64;
    manifest[0].zip_sha256 = sha(&zip);
    manifest[0].files = entries(&zip);
    server
        .manifests
        .insert(0, serde_json::to_vec(&manifest).unwrap());
    server.songs.insert(1, zip);
    files
}

#[test]
fn reused_song_id_does_not_serve_old_cached_files() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    let path = format!("{SONG}/bgm01.wav");
    assert_eq!(read_all(&fs, &path).unwrap(), env.want("bgm01.wav"));
    assert_eq!(env.state.counters.play.load(Ordering::SeqCst), 1);

    let files = replace_song_on_server(&env);
    env.rt.block_on(env.client.sync()).unwrap();
    // 동기화 때 옛 곡의 캐시를 지운다.
    assert_eq!(env.client.fetcher.cache().total().unwrap(), 0);
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    assert_eq!(read_all(&fs, &path).unwrap(), files[1].1);
    assert_eq!(env.state.counters.play.load(Ordering::SeqCst), 2);
}

#[test]
fn removed_song_cache_is_dropped_on_sync() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    read_all(&fs, &format!("{SONG}/bgm01.wav")).unwrap();
    let cache = env.client.fetcher.cache();
    let cached = cache.path(1, "bgm01.wav").unwrap();
    assert!(cached.exists());

    env.state.server().manifests.clear();
    env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(cache.total().unwrap(), 0);
    assert!(!cached.exists());
}

#[test]
fn truncated_cache_file_is_downloaded_again() {
    let env = setup(true);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    let path = format!("{SONG}/bgm01.wav");
    read_all(&fs, &path).unwrap();

    // 정전으로 잘린 파일 흉내
    let cached = env.client.fetcher.cache().path(1, "bgm01.wav").unwrap();
    std::fs::write(&cached, b"").unwrap();
    assert_eq!(read_all(&fs, &path).unwrap(), env.want("bgm01.wav"));
    assert_eq!(env.state.counters.play.load(Ordering::SeqCst), 2);
}

/// 브라우저 대신 로그인 주소를 열어 루프백 리다이렉트까지 따라간다.
fn fake_browser(url: &str) {
    let url = url.to_string();
    tokio::spawn(async move {
        let resp = reqwest::get(url).await.unwrap();
        assert_eq!(resp.status(), 200);
    });
}

#[test]
fn several_servers_show_as_top_level_folders() {
    let rt = runtime();
    let (state_a, fixture) = server_state();
    let (state_b, _) = server_state();
    state_a.server().sessions.clear();
    state_b.server().sessions.clear();
    let url_a = spawn_server(&rt, state_a.clone());
    let url_b = spawn_server(&rt, state_b.clone());
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(AppDir::new(dir.path()), false, rt.handle().clone()).unwrap();
    let banner = fixture
        .files
        .iter()
        .find(|(n, _)| *n == "banner.png")
        .unwrap();

    let (a, b) = rt.block_on(async {
        let a = hub.add(&url_a, Some("Server A")).await.unwrap();
        let b = hub.add(&url_b, Some("Server B")).await.unwrap();
        assert!(hub.add(&url_a, None).await.is_err(), "same url twice");
        assert!(hub.me(&a.entry.id).await.unwrap().is_none());
        for s in [&a, &b] {
            let user = hub
                .login(&s.entry.id, "test", Duration::from_secs(10), fake_browser)
                .await
                .unwrap();
            assert_eq!(user.display_name, "Tester");
        }
        hub.sync_all().await;
        (a, b)
    });
    assert!(a.sync_status().last_ok_at.is_some());
    // 키체인 대신 서버별 폴더의 session 파일에 저장됨
    assert!(hub.app_dir().server_dir(&a.entry).join("session").exists());

    let drive = hub.drive();
    let names: Vec<String> = drive
        .readdir(ebms_core::tree::ROOT)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["Server A", "Server B"]);
    for server in ["Server A", "Server B"] {
        let path = format!("{server}/{SONG}/banner.png");
        assert_eq!(read_all(drive.as_ref(), &path).unwrap(), banner.1);
    }

    // 서버 A의 세션이 만료돼도 B는 그대로 동작한다.
    state_a.server().sessions.clear();
    rt.block_on(hub.sync_all());
    assert!(a.sync_status().last_error.is_some());
    assert!(a.api().needs_login());
    assert!(b.sync_status().last_error.is_none());
    let path = format!("Server B/{SONG}/bgm01.wav");
    assert_eq!(read_all(drive.as_ref(), &path).unwrap(), fixture.files[1].1);
    // 받아 둔 파일은 로그인 없이도 읽힌다.
    let path = format!("Server A/{SONG}/banner.png");
    assert_eq!(read_all(drive.as_ref(), &path).unwrap(), banner.1);

    // 캐시 사용량을 보여주고 비울 수 있다.
    let usage = a.usage().unwrap();
    assert!(usage.cache_bytes > 0);
    assert_eq!(usage.cache_limit, 20 << 30);
    assert!(
        usage.total_bytes > usage.cache_bytes,
        "index and charts count too"
    );
    let freed = rt.block_on(a.clear_cache()).unwrap();
    assert_eq!(freed, usage.cache_bytes);
    assert_eq!(a.usage().unwrap().cache_bytes, 0);
    let cache_dir = hub.app_dir().server_dir(&a.entry).join("cache");
    assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 0);

    // 삭제하면 로그아웃하고 드라이브에서 빠진다. 설정 파일에도 남지 않는다.
    // 로컬 데이터도 지우기를 고르면 서버 폴더가 사라진다.
    // (Windows는 열린 파일을 못 지우므로 서버 B를 놓아 준다.)
    let b_entry = b.entry.clone();
    drop(b);
    let b_dir = hub.app_dir().server_dir(&b_entry);
    assert!(b_dir.join("index.sqlite").exists());
    rt.block_on(hub.remove(&b_entry.id, true)).unwrap();
    assert!(!b_dir.exists());
    assert!(state_b.server().sessions.is_empty());
    assert!(drive.resolve("Server B").is_none());
    assert!(drive.resolve("Server A").is_some());
    let config = AppDir::new(dir.path()).load_config().unwrap();
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].name, "Server A");

    // 지우지 않으면 로컬 데이터는 남는다.
    let a_dir = hub.app_dir().server_dir(&a.entry);
    rt.block_on(hub.remove(&a.entry.id, false)).unwrap();
    assert!(a_dir.join("index.sqlite").exists());
    rt.block_on(hub.add(&url_a, Some("Server A"))).unwrap();

    // 다시 열면 남은 서버만
    drop(hub);
    let hub = Hub::open(AppDir::new(dir.path()), false, rt.handle().clone()).unwrap();
    assert_eq!(hub.servers().len(), 1);
    assert!(hub.drive().resolve(&format!("Server A/{SONG}")).is_some());
}

/// 서버처럼 `{song_id}/{경로}` 이름으로 사전 파일을 무압축으로 묶는다.
fn pre_chunk(song_id: u32, files: &[(&str, &[u8])]) -> Vec<u8> {
    let named: Vec<(String, &[u8])> = files
        .iter()
        .map(|(n, d)| (format!("{song_id}/{n}"), *d))
        .collect();
    let refs: Vec<(&str, &[u8])> = named.iter().map(|(n, d)| (n.as_str(), *d)).collect();
    make_zip(&refs, zip::CompressionMethod::Stored)
}

/// 사전 청크가 있는 서버(API 3).
fn setup_with_pre_chunk() -> Env {
    let env = setup(true);
    let chunk = pre_chunk(
        1,
        &[
            ("banner.png", &env.want("banner.png")),
            ("preview.ogg", &env.want("preview.ogg")),
        ],
    );
    let mut server = env.state.server();
    server.api = 3;
    server.pre_chunks.insert(0, chunk);
    drop(server);
    env
}

#[test]
fn sync_downloads_pre_chunk_so_pre_files_need_no_download() {
    let env = setup_with_pre_chunk();
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(report.pre_chunks_updated, vec![0]);
    let c = &env.state.counters;
    assert_eq!(c.pre_chunk.load(Ordering::SeqCst), 1);

    let fs = env.client.fs(env.rt.handle().clone()).unwrap();
    for name in ["banner.png", "preview.ogg"] {
        assert_eq!(
            read_all_as(&fs, &format!("{SONG}/{name}"), &Indexer).unwrap(),
            env.want(name)
        );
    }
    assert_eq!(c.pre_file.load(Ordering::SeqCst), 0);
    assert_eq!(c.play.load(Ordering::SeqCst), 0);
    assert_eq!(env.client.fetcher.cache().total().unwrap(), 0);

    // 바뀌지 않으면 다시 받지 않는다
    assert!(!env.rt.block_on(env.client.sync()).unwrap().changed());
    assert_eq!(c.pre_chunk.load(Ordering::SeqCst), 1);

    // 서버에서 없어진 청크는 지운다
    env.state.server().pre_chunks.clear();
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert_eq!(report.pre_chunks_removed, vec![0]);
    assert!(env.client.index.pre_files(1).unwrap().is_empty());
}

#[test]
fn pre_file_missing_from_chunk_falls_back_to_pre_api() {
    let env = setup_with_pre_chunk();
    // 청크의 배너가 매니페스트와 다르면(서버가 다시 만드는 중 등) 청크를 믿지 않는다.
    let chunk = pre_chunk(
        1,
        &[
            ("banner.png", b"stale"),
            ("preview.ogg", &env.want("preview.ogg")),
        ],
    );
    env.state.server().pre_chunks.insert(0, chunk);
    env.rt.block_on(env.client.sync()).unwrap();
    let fs = env.client.fs(env.rt.handle().clone()).unwrap();

    assert_eq!(
        read_all(&fs, &format!("{SONG}/banner.png")).unwrap(),
        env.want("banner.png")
    );
    assert_eq!(
        read_all(&fs, &format!("{SONG}/preview.ogg")).unwrap(),
        env.want("preview.ogg")
    );
    assert_eq!(env.state.counters.pre_file.load(Ordering::SeqCst), 1);
}

#[test]
fn old_server_has_no_pre_chunks() {
    let env = setup(true);
    let report = env.rt.block_on(env.client.sync()).unwrap();
    assert!(report.pre_chunks_updated.is_empty());
    assert_eq!(env.state.counters.pre_hash.load(Ordering::SeqCst), 0);
}
