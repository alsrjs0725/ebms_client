//! 코어를 터미널에서 확인하기 위한 CLI.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ebms_core::fs::Kind;
use ebms_core::{Client, Options};

#[derive(Parser)]
#[command(name = "ebms", about = "EBMS client")]
struct Cli {
    /// 서버 주소
    #[arg(long, env = "EBMS_SERVER", default_value = "http://localhost:8000")]
    server: String,
    /// 로컬 데이터 폴더
    #[arg(long, env = "EBMS_DATA", default_value = "ebms-data")]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 차트 청크와 매니페스트를 동기화
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
    /// 가상 드라이브로 마운트하고 주기적으로 동기화 (Ctrl-C로 종료)
    #[cfg(target_os = "linux")]
    Mount {
        mountpoint: PathBuf,
        /// 동기화 간격(분)
        #[arg(long, default_value_t = 30)]
        sync_minutes: u64,
    },
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
    let client = Client::open(&Options::new(&cli.server, &cli.data))?;

    match cli.cmd {
        Cmd::Sync => {
            let report = rt.block_on(client.sync())?;
            println!("{report:?}");
        }
        Cmd::Ls { path } => {
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
                let buf = fs.read(attr.ino, offset, 1 << 20)?;
                if buf.is_empty() {
                    break;
                }
                offset += buf.len() as u64;
                out.write_all(&buf)?;
            }
        }
        Cmd::Status => {
            let fs = client.fs(rt.handle().clone())?;
            let tree = fs.tree();
            println!("songs: {}", tree.song_count());
            println!("nodes: {}", tree.node_count());
            println!("cache: {} bytes", client.fetcher.cache().total()?);
        }
        #[cfg(target_os = "linux")]
        Cmd::Mount {
            mountpoint,
            sync_minutes,
        } => mount(&rt, client, &mountpoint, sync_minutes)?,
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount(
    rt: &tokio::runtime::Runtime,
    client: Client,
    mountpoint: &std::path::Path,
    sync_minutes: u64,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::time::Duration;

    // 서버에 연결되지 않아도 로컬 인덱스로 마운트한다.
    if let Err(e) = rt.block_on(client.sync()) {
        tracing::warn!(%e, "initial sync failed, using local index");
    }
    let fs = Arc::new(client.fs(rt.handle().clone())?);
    let _session = ebms_vfs_fuse::spawn_mount(fs.clone(), mountpoint)
        .with_context(|| format!("mount {}", mountpoint.display()))?;
    tracing::info!(mountpoint = %mountpoint.display(), "mounted");

    let client = Arc::new(client);
    rt.block_on(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(sync_minutes.max(1) * 60));
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tick.tick() => match client.sync().await {
                    Ok(r) if r.changed() => {
                        if let Err(e) = fs.reload() {
                            tracing::warn!(%e, "reload failed");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(%e, "sync failed"),
                },
            }
        }
    });
    Ok(())
}
