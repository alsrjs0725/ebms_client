//! 로그. 콘솔(stdout)과 앱 데이터 폴더의 `logs/`에 함께 남긴다.
//!
//! Windows 릴리스는 콘솔이 없고 데스크톱에서 실행하면 stdout이 어디에도 붙지 않으므로,
//! "오류는 조용히, 로그에만 남긴다"가 의미 있으려면 파일 로그가 있어야 한다.
//! 하루 단위로 새 파일을 쓰고 최근 [`KEEP_FILES`]개만 남긴다.

use std::path::{Path, PathBuf};

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// 남길 로그 파일 수(일).
const KEEP_FILES: usize = 7;

/// 앱 데이터 폴더 안 로그 폴더.
pub fn log_dir(root: &Path) -> PathBuf {
    root.join("logs")
}

/// `dir`에 하루 단위로 회전하는 로그 파일 `ebms.<날짜>.log`.
fn appender(dir: &Path) -> Result<RollingFileAppender, tracing_appender::rolling::InitError> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("ebms")
        .filename_suffix("log")
        .max_log_files(KEEP_FILES)
        .build(dir)
}

/// 로그를 켠다. 파일 로그를 열지 못하면 콘솔에만 쓰고 이유를 로그에 남긴다.
/// 파일 로그를 쓰면 그 폴더를 돌려준다.
pub fn init(root: Option<&Path>) -> Option<PathBuf> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,fuser=error".into());
    let file = root.map(|root| {
        let dir = log_dir(root);
        appender(&dir).map(|a| (dir, a))
    });
    let (dir, appender, failed) = match file {
        Some(Ok((dir, appender))) => (Some(dir), Some(appender), None),
        Some(Err(e)) => (None, None, Some(e)),
        None => (None, None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(appender.map(|a| fmt::layer().with_writer(a).with_ansi(false)))
        .init();
    if let Some(e) = failed {
        tracing::warn!(%e, "could not open log file, logging to console only");
    }
    // 패닉도 파일 로그에 남긴다. 콘솔 출력은 기본 훅이 그대로 한다.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn writes_dated_file_under_logs() {
        let root = std::env::temp_dir().join(format!("ebms-log-test-{}", std::process::id()));
        let dir = log_dir(&root);
        let mut a = appender(&dir).unwrap();
        a.write_all(b"hello\n").unwrap();
        a.flush().unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            names[0].starts_with("ebms.") && names[0].ends_with(".log"),
            "{names:?}"
        );
    }
}
