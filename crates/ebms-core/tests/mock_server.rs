//! ebms_server API를 흉내 내는 목 서버로 로그인·동기화·가상 FS·다운로드를 검증한다.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
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
use ebms_core::fs::{Kind, ReadOnlyFs};
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
    chart_chunks: HashMap<u32, Vec<u8>>,
    manifests: HashMap<u32, Vec<u8>>,
    songs: HashMap<u32, Vec<u8>>,
    /// 사전 파일 경로
    pre_paths: HashSet<String>,
    /// 유효한 세션키
    sessions: HashSet<String>,
    /// 1회용 코드 → code_challenge
    codes: HashMap<String, String>,
    tickets: u32,
}

#[derive(Default)]
struct Counters {
    play: AtomicUsize,
    pre_file: AtomicUsize,
    chart_chunk: AtomicUsize,
    manifest: AtomicUsize,
}

#[derive(Clone)]
struct AppState {
    server: Arc<Mutex<Server>>,
    counters: Arc<Counters>,
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
            get(|| async { axum::Json(json!({"api": 1, "server": "test", "auth": ["google", "discord"]})) }),
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
                s.counters.chart_chunk.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StatusCode>(s.server().chart_chunks[&id].clone())
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

/// 가상 FS 백엔드처럼 런타임 밖 스레드에서 작은 단위로 읽는다.
fn read_all(fs: &dyn ReadOnlyFs, path: &str) -> ebms_core::Result<Vec<u8>> {
    let attr = fs.resolve(path).unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(attr.kind, Kind::File);
    let mut out = Vec::new();
    while (out.len() as u64) < attr.size {
        let buf = fs.read(attr.ino, out.len() as u64, 7_000)?;
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

    // 삭제하면 로그아웃하고 드라이브에서 빠진다. 설정 파일에도 남지 않는다.
    rt.block_on(hub.remove(&b.entry.id)).unwrap();
    assert!(state_b.server().sessions.is_empty());
    assert!(drive.resolve("Server B").is_none());
    assert!(drive.resolve("Server A").is_some());
    let config = AppDir::new(dir.path()).load_config().unwrap();
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].name, "Server A");

    // 다시 열면 남은 서버만
    drop(hub);
    let hub = Hub::open(AppDir::new(dir.path()), false, rt.handle().clone()).unwrap();
    assert_eq!(hub.servers().len(), 1);
    assert!(hub.drive().resolve(&format!("Server A/{SONG}")).is_some());
}
