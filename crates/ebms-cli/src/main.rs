//! 코어를 터미널에서 확인하기 위한 CLI.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ebms_core::api::Api;
use ebms_core::auth::LoginRequest;
use ebms_core::config::{AppDir, Config, ServerEntry};
use ebms_core::fs::{Kind, ReadOnlyFs, Trusted};
use ebms_core::session::SessionStore;
use ebms_core::{Client, Error, Options};

/// 브라우저 로그인을 기다리는 시간
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(name = "ebms", about = "EBMS client")]
struct Cli {
    /// 대상 서버 (id, 이름 또는 주소). 서버가 하나뿐이면 생략
    #[arg(long, short, env = "EBMS_SERVER", global = true)]
    server: Option<String>,
    /// 앱 데이터 폴더 (기본: OS 앱 데이터 폴더의 ebms)
    #[arg(long, env = "EBMS_DATA", global = true)]
    data: Option<PathBuf>,
    /// 세션키를 OS 키체인 대신 서버 데이터 폴더의 파일에 저장
    #[arg(long, env = "EBMS_NO_KEYRING", global = true)]
    no_keyring: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 서버 목록 관리
    #[command(subcommand)]
    Server(ServerCmd),
    /// 브라우저로 로그인하고 세션키를 저장
    Login {
        /// 브라우저를 열지 않고 주소만 출력
        #[arg(long)]
        no_browser: bool,
        /// 서버의 기기 목록에 보일 이름
        #[arg(long)]
        device_name: Option<String>,
    },
    /// 세션키를 폐기하고 지움
    Logout,
    /// 로그인한 계정과 남은 티켓·사전 다운로드 사용량
    Whoami,
    /// 동기화(= 사전 다운로드): 차트 청크, 매니페스트, 사전 청크(배너·프리뷰 등)
    Sync,
    /// 가상 트리 목록
    Ls {
        #[arg(default_value = "")]
        path: String,
    },
    /// 가상 파일 내용을 stdout으로 (필요하면 다운로드)
    Cat { path: String },
    /// 상태 요약
    Status,
    /// 받은 에셋 캐시를 비움 ("항상 보관" 곡은 남김)
    ClearCache,
    /// 가상 드라이브로 마운트하고 주기적으로 동기화 (Ctrl-C로 종료).
    /// 서버마다 최상위 폴더 하나. --server를 주면 그 서버만
    #[cfg(any(target_os = "linux", windows))]
    Mount {
        mountpoint: PathBuf,
        /// 동기화 간격(분)
        #[arg(long, default_value_t = 30)]
        sync_minutes: u64,
    },
}

#[derive(Subcommand)]
enum ServerCmd {
    /// 서버 추가 (/api/version으로 확인)
    Add {
        url: String,
        /// 표시 이름 (기본: 호스트 이름)
        #[arg(long)]
        name: Option<String>,
    },
    /// 서버 목록과 로그인 상태
    List,
    /// 서버 삭제 (로그아웃하고 목록에서 뺌. 로컬 데이터는 --purge를 줄 때만 지움)
    Remove {
        server: String,
        /// 로컬 데이터(인덱스·차트·캐시)도 지움
        #[arg(long)]
        purge: bool,
    },
}

struct App {
    dir: AppDir,
    config: Config,
    use_keyring: bool,
}

impl App {
    fn server(&self, selector: Option<&str>) -> anyhow::Result<ServerEntry> {
        self.config.select(selector).cloned().map_err(|e| match e {
            Error::Config(msg) => {
                anyhow::anyhow!("{msg} (ebms server add <url>, ebms server list)")
            }
            e => e.into(),
        })
    }

    fn session(&self, server: &ServerEntry) -> SessionStore {
        SessionStore::new(&server.url, &self.dir.server_dir(server), self.use_keyring)
    }

    fn client(&self, server: &ServerEntry) -> anyhow::Result<Client> {
        self.dir.prepare_server_dir(server)?;
        let mut opts = Options::new(&server.url, self.dir.server_dir(server));
        opts.session = self.session(server).load()?;
        Ok(Client::open(&opts)?)
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,fuser=error".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    let root = match cli.data {
        Some(d) => d,
        None => AppDir::default_root().context("no app data folder, use --data")?,
    };
    let dir = AppDir::new(root);
    let mut app = App {
        config: dir.load_config()?,
        dir,
        use_keyring: !cli.no_keyring,
    };
    let sel = cli.server.as_deref();

    match cli.cmd {
        Cmd::Server(cmd) => server_cmd(&rt, &mut app, cmd)?,
        Cmd::Login {
            no_browser,
            device_name,
        } => {
            let server = app.server(sel)?;
            let api = Api::new(&server.url)?;
            let device_name =
                device_name.unwrap_or_else(|| ebms_core::auth::device_name("ebms-cli"));
            let req = rt.block_on(LoginRequest::start(&api, &device_name))?;
            eprintln!("Log in with your browser:\n{}", req.url());
            if !no_browser && let Err(e) = open::that_detached(req.url()) {
                tracing::warn!(%e, "could not open browser");
            }
            let token = rt.block_on(req.finish(&api, LOGIN_TIMEOUT))?;
            app.session(&server).save(&token.session_key)?;
            println!("{}: logged in as {}", server.name, token.user.display_name);
        }
        Cmd::Logout => {
            let server = app.server(sel)?;
            let store = app.session(&server);
            match store.load()? {
                Some(key) => {
                    let api = Api::new(&server.url)?;
                    api.set_session(Some(key));
                    // 서버에 닿지 않아도 로컬 세션키는 지운다.
                    if let Err(e) = rt.block_on(api.logout()) {
                        tracing::warn!(%e, "server logout failed");
                    }
                    store.clear()?;
                    println!("{}: logged out", server.name);
                }
                None => println!("{}: not logged in", server.name),
            }
        }
        Cmd::Whoami => {
            let server = app.server(sel)?;
            whoami(&rt, &app, &server)?;
        }
        Cmd::Sync => {
            let client = app.client(&app.server(sel)?)?;
            let report = rt.block_on(client.sync()).map_err(login_hint)?;
            println!("{report:?}");
        }
        Cmd::Ls { path } => {
            let client = app.client(&app.server(sel)?)?;
            let fs = client.fs(rt.handle().clone())?;
            let attr = fs
                .resolve(&path)
                .with_context(|| format!("not found: {path}"))?;
            if attr.kind == Kind::File {
                println!("{:>12}  {path}", attr.size);
                return Ok(());
            }
            for e in fs.readdir(attr.ino).unwrap_or_default() {
                let size = fs.getattr(e.ino).map(|a| a.size).unwrap_or(0);
                let slash = if e.kind == Kind::Dir { "/" } else { "" };
                println!("{size:>12}  {}{slash}", e.name);
            }
        }
        Cmd::Cat { path } => {
            let client = app.client(&app.server(sel)?)?;
            let fs = client.fs(rt.handle().clone())?;
            let attr = fs
                .resolve(&path)
                .with_context(|| format!("not found: {path}"))?;
            if attr.kind != Kind::File {
                bail!("not a file: {path}");
            }
            let mut out = std::io::stdout().lock();
            let mut offset = 0;
            while offset < attr.size {
                let buf = fs
                    .read(attr.ino, offset, 1 << 20, &Trusted("ebms cat"))
                    .map_err(login_hint)?;
                if buf.is_empty() {
                    break;
                }
                offset += buf.len() as u64;
                out.write_all(&buf)?;
            }
        }
        Cmd::Status => {
            let server = app.server(sel)?;
            let client = app.client(&server)?;
            let fs = client.fs(rt.handle().clone())?;
            let tree = fs.tree();
            println!("server: {} ({})", server.name, server.url);
            println!("data: {}", client.paths.root().display());
            println!("songs: {}", tree.song_count());
            println!("loaded songs: {}", tree.loaded_songs());
            println!("cache: {} bytes", client.fetcher.cache().total()?);
        }
        Cmd::ClearCache => {
            let server = app.server(sel)?;
            let client = app.client(&server)?;
            let freed = client.fetcher.cache().clear()?;
            println!("{}: cleared {freed} bytes", server.name);
        }
        #[cfg(any(target_os = "linux", windows))]
        Cmd::Mount {
            mountpoint,
            sync_minutes,
        } => {
            let only = match sel {
                Some(_) => Some(app.server(sel)?.id),
                None => None,
            };
            let hub = ebms_core::hub::Hub::open(app.dir, app.use_keyring, rt.handle().clone())?;
            mount(&rt, hub, only, &mountpoint, sync_minutes)?
        }
    }
    Ok(())
}

fn server_cmd(rt: &tokio::runtime::Runtime, app: &mut App, cmd: ServerCmd) -> anyhow::Result<()> {
    match cmd {
        ServerCmd::Add { url, name } => {
            let url = ebms_core::config::normalize_url(&url)?;
            let version = rt
                .block_on(Api::new(&url)?.version())
                .with_context(|| format!("{url} is not an EBMS server"))?;
            if !ebms_core::API_VERSIONS.contains(&version.api) {
                bail!("unsupported server api version {}", version.api);
            }
            let entry = app.config.add(&url, name.as_deref())?.clone();
            app.dir.save_config(&app.config)?;
            println!("added {} ({}) as {}", entry.name, entry.url, entry.id);
            if !version.auth.is_empty() {
                println!("login: ebms login --server {}", entry.id);
            }
        }
        ServerCmd::List => {
            if app.config.servers.is_empty() {
                println!("no server. ebms server add <url>");
            }
            for s in &app.config.servers {
                let login = match app.session(s).load() {
                    Ok(Some(_)) => "logged in",
                    Ok(None) => "logged out",
                    Err(_) => "?",
                };
                println!("{}\t{}\t{}\t{login}", s.id, s.name, s.url);
            }
        }
        ServerCmd::Remove { server, purge } => {
            let entry = app.server(Some(&server))?;
            let store = app.session(&entry);
            if let Some(key) = store.load()? {
                let api = Api::new(&entry.url)?;
                api.set_session(Some(key));
                if let Err(e) = rt.block_on(api.logout()) {
                    tracing::warn!(%e, "server logout failed");
                }
            }
            store.clear()?;
            app.config.remove(&entry.id);
            app.dir.save_config(&app.config)?;
            let data = app.dir.server_dir(&entry);
            if !purge {
                let size = ebms_core::paths::dir_size(&data);
                println!(
                    "removed {}. local data ({size} bytes) kept in {} (delete with --purge)",
                    entry.name,
                    data.display()
                );
            } else if app.dir.purge_server_dir(&entry)? {
                println!("removed {} and its local data", entry.name);
            } else {
                println!(
                    "removed {}. some local data is in use and will be deleted later: {}",
                    entry.name,
                    data.display()
                );
            }
        }
    }
    Ok(())
}

fn whoami(rt: &tokio::runtime::Runtime, app: &App, server: &ServerEntry) -> anyhow::Result<()> {
    println!("server: {} ({})", server.name, server.url);
    let Some(key) = app.session(server).load()? else {
        println!("not logged in (ebms login)");
        return Ok(());
    };
    let api = Api::new(&server.url)?;
    api.set_session(Some(key));
    let me = match rt.block_on(api.me()) {
        Err(Error::Unauthorized) => {
            println!("login required (ebms login)");
            return Ok(());
        }
        r => r?,
    };
    println!("user: {} ({})", me.user.display_name, me.user.id);
    if me.user.role != "user" {
        println!("role: {}", me.user.role);
    }
    let oauths: Vec<String> = me
        .oauths
        .iter()
        .map(|o| format!("{} {}", o.oauth, o.name).trim().to_string())
        .collect();
    println!("oauth: {}", oauths.join(", "));
    let t = &me.tickets;
    let refill = match t.next_refill_at {
        Some(at) => format!(", next in {}s", (at - unix_now()).max(0)),
        None => String::new(),
    };
    println!("tickets: {}/{}{refill}", t.available, t.max);
    let p = &me.pre;
    println!(
        "pre download ({}): {} / {}{}",
        p.month,
        human_bytes(p.used_bytes),
        human_bytes(p.limit_bytes),
        if p.throttled {
            format!(", throttled to {} Kbps", p.throttled_kbps)
        } else {
            String::new()
        }
    );
    Ok(())
}

/// 401이면 다시 로그인하라고 알려 준다.
fn login_hint(e: Error) -> anyhow::Error {
    match e {
        Error::Unauthorized => anyhow::anyhow!("login required: ebms login"),
        e => e.into(),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(any(target_os = "linux", windows))]
fn mount(
    rt: &tokio::runtime::Runtime,
    hub: ebms_core::hub::Hub,
    only: Option<String>,
    mountpoint: &std::path::Path,
    sync_minutes: u64,
) -> anyhow::Result<()> {
    use std::time::Duration;

    let drive = hub.drive();
    if let Some(only) = &only {
        for id in drive.ids().into_iter().filter(|id| id != only) {
            drive.remove(&id);
        }
    }
    if drive.ids().is_empty() {
        bail!("no server added yet (ebms server add <url>)");
    }
    // 서버에 연결되지 않아도 로컬 인덱스로 마운트한다.
    rt.block_on(hub.sync_all());
    #[cfg(target_os = "linux")]
    let session = ebms_vfs_fuse::spawn_mount(drive, mountpoint, hub.config().players());
    #[cfg(windows)]
    let session =
        ebms_vfs_winfsp::spawn_mount(drive, mountpoint, hub.config().players(), hub.runtime());
    let _session = session.with_context(|| format!("mount {}", mountpoint.display()))?;
    tracing::info!(mountpoint = %mountpoint.display(), "mounted");

    rt.block_on(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(sync_minutes.max(1) * 60));
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tick.tick() => hub.sync_all().await,
            }
        }
    });
    Ok(())
}
