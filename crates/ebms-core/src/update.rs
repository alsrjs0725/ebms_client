//! 클라이언트 자동 업데이트. GitHub Releases의 최신 정식 릴리스(사전 릴리스 `stage` 제외)를 확인하고,
//! 받은 zip으로 실행 파일 폴더의 파일을 바꾼다.
//!
//! zip에 든 파일만 바꾸고, 실행 파일 옆 `data` 폴더(앱 데이터) 등 나머지는 건드리지 않는다.
//! Windows는 실행 중인 exe를 덮어쓸 수 없지만 이름은 바꿀 수 있으므로, 기존 파일을 `*.old`로 옮긴 뒤
//! 새 파일을 놓는다. `*.old`는 다음 실행 때 [`cleanup`]이 지운다.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// 릴리스를 올리는 저장소
pub const REPO: &str = "alsrjs0725/ebms_client";
/// 릴리스에 올라가는 Windows 패키지 (`release.yml`의 패키징 이름)
pub const ASSET: &str = "ebms-windows-x64.zip";
/// 바꿔 놓은 기존 파일에 붙이는 접미사
const OLD_SUFFIX: &str = ".old";
/// 새 파일을 풀어 두는 폴더. 같은 드라이브여야 이름 바꾸기로 옮길 수 있어 실행 파일 폴더 안에 둔다.
const STAGING: &str = ".update";
/// 앱 데이터 폴더. zip에 있더라도 풀지 않는다.
const DATA_DIR: &str = "data";

/// 설치할 수 있는 새 릴리스.
#[derive(Clone, Debug, Serialize)]
pub struct Release {
    /// `v`를 뺀 버전 (`0.2.0`)
    pub version: String,
    pub notes: String,
    /// 릴리스 페이지
    pub page_url: String,
    #[serde(skip)]
    asset_url: String,
    /// `sha256:<hex>`. GitHub가 주지 않으면 None
    #[serde(skip)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn http(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("ebms-client/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(timeout)
        .build()?)
}

/// 최신 정식 릴리스가 `current`보다 새 버전이고 Windows 패키지가 있으면 돌려준다.
pub async fn check(current: &str) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = http(Duration::from_secs(10))?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    // 정식 릴리스가 아직 하나도 없음
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url,
        });
    }
    Ok(pick(resp.json().await?, current))
}

fn pick(rel: GhRelease, current: &str) -> Option<Release> {
    if rel.draft || rel.prerelease || !is_newer(&rel.tag_name, current) {
        return None;
    }
    let asset = rel.assets.into_iter().find(|a| a.name == ASSET)?;
    Some(Release {
        version: rel.tag_name.trim_start_matches('v').to_string(),
        notes: rel.body.unwrap_or_default(),
        page_url: rel.html_url,
        asset_url: asset.browser_download_url,
        digest: asset.digest,
    })
}

/// `1.2.3`, `v1.2` 같은 버전을 (주, 부, 수) 숫자로. `-` 뒤 꼬리표는 무시한다.
fn parse_version(v: &str) -> Option<[u64; 3]> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split(['-', '+']).next()?;
    let mut out = [0; 3];
    for (i, part) in v.split('.').enumerate() {
        *out.get_mut(i)? = part.parse().ok()?;
    }
    Some(out)
}

/// `tag`가 `current`보다 새 버전인가. 어느 쪽이든 읽지 못하면 false.
pub fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// 릴리스 패키지를 받는다. GitHub가 준 SHA-256과 맞는지 확인한다.
pub async fn download(rel: &Release) -> Result<Vec<u8>> {
    let resp = http(Duration::from_secs(30))?
        .get(&rel.asset_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: rel.asset_url.clone(),
        });
    }
    let data = resp.bytes().await?.to_vec();
    if let Some(want) = rel
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let got = crate::sha256_hex(&data);
        if !got.eq_ignore_ascii_case(want) {
            return Err(Error::Integrity(format!(
                "{ASSET}: sha256 {got}, expected {want}"
            )));
        }
    }
    Ok(data)
}

/// 받은 패키지(`zip`)로 `dir`의 파일을 바꾼다. 패키지에 `required` 파일이 없으면 아무것도 바꾸지 않는다.
///
/// zip의 최상위 폴더(`ebms-windows-x64/`)는 벗겨 낸다. 중간에 실패하면 바꾼 파일을 되돌린다.
pub fn install(zip: &[u8], dir: &Path, required: &str) -> Result<()> {
    let staging = dir.join(STAGING);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let result = extract(zip, &staging).and_then(|files| {
        if !files.iter().any(|f| f == Path::new(required)) {
            return Err(Error::Other(format!("{ASSET} has no {required}")));
        }
        swap(&staging, dir, &files)
    });
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// 파일을 `to`에 풀고, 푼 파일의 상대 경로를 돌려준다.
fn extract(zip: &[u8], to: &Path) -> Result<Vec<PathBuf>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip))?;
    let mut files = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        // enclosed_name은 `..`·절대 경로를 거른다.
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let rel: PathBuf = name
            .components()
            .skip(1)
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        let first = rel.components().next();
        if first.is_none() || first == Some(Component::Normal(DATA_DIR.as_ref())) {
            continue;
        }
        let path = to.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        std::fs::write(&path, data)?;
        files.push(rel);
    }
    Ok(files)
}

fn old_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(OLD_SUFFIX);
    PathBuf::from(s)
}

/// `staging`의 `files`를 `dir`로 옮긴다. 기존 파일은 `*.old`로 비켜 둔다.
fn swap(staging: &Path, dir: &Path, files: &[PathBuf]) -> Result<()> {
    // 비켜 둔 파일, 새로 놓은 파일
    let mut moved: Vec<PathBuf> = Vec::new();
    let mut placed: Vec<PathBuf> = Vec::new();
    let mut step = |rel: &PathBuf| -> std::io::Result<()> {
        let target = dir.join(rel);
        if target.exists() {
            let old = old_path(&target);
            if old.exists() {
                std::fs::remove_file(&old)?;
            }
            std::fs::rename(&target, &old)?;
            moved.push(target.clone());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(staging.join(rel), &target)?;
        placed.push(target);
        Ok(())
    };
    let mut failed = None;
    for rel in files {
        if let Err(e) = step(rel) {
            failed = Some(e);
            break;
        }
    }
    let Some(e) = failed else {
        return Ok(());
    };
    for target in &placed {
        let _ = std::fs::remove_file(target);
    }
    for target in &moved {
        if let Err(e) = std::fs::rename(old_path(target), target) {
            tracing::warn!(?target, %e, "could not restore file after failed update");
        }
    }
    Err(e.into())
}

/// 지난 업데이트가 남긴 `*.old` 파일과 풀다 만 폴더를 지운다. `data` 폴더는 보지 않는다.
/// 지우지 못한 파일(아직 실행 중인 이전 프로세스가 쥔 파일 등)은 다음에 다시 지운다.
pub fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.join(STAGING));
    fn walk(dir: &Path, top: bool) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !(top && entry.file_name() == DATA_DIR) {
                    walk(&path, false);
                }
            } else if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(OLD_SUFFIX))
                && let Err(e) = std::fs::remove_file(&path)
            {
                tracing::debug!(?path, %e, "old file not removed yet");
            }
        }
    }
    walk(dir, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("stage", "0.1.0"));
        assert!(!is_newer("v1.2.3.4", "0.1.0"));
    }

    #[test]
    fn pick_skips_prerelease_and_missing_asset() {
        let rel = |tag: &str, pre: bool, asset: &str| GhRelease {
            tag_name: tag.into(),
            body: Some("notes".into()),
            html_url: "https://example.com".into(),
            draft: false,
            prerelease: pre,
            assets: vec![GhAsset {
                name: asset.into(),
                browser_download_url: "https://example.com/a.zip".into(),
                digest: None,
            }],
        };
        let got = pick(rel("v0.2.0", false, ASSET), "0.1.0").unwrap();
        assert_eq!(got.version, "0.2.0");
        assert!(pick(rel("v0.2.0", true, ASSET), "0.1.0").is_none());
        assert!(pick(rel("v0.2.0", false, "other.zip"), "0.1.0").is_none());
        assert!(pick(rel("v0.1.0", false, ASSET), "0.1.0").is_none());
    }

    #[test]
    fn install_replaces_files_and_keeps_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("ebms-app.exe"), b"old app").unwrap();
        std::fs::write(dir.join("README.md"), b"old readme").unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/config.toml"), b"keep").unwrap();

        let zip = make_zip(&[
            ("ebms-windows-x64/ebms-app.exe", b"new app"),
            ("ebms-windows-x64/ebms.exe", b"new cli"),
            ("ebms-windows-x64/data/config.toml", b"overwrite"),
            ("ebms-windows-x64/../evil", b"x"),
        ]);
        install(&zip, dir, "ebms-app.exe").unwrap();

        assert_eq!(std::fs::read(dir.join("ebms-app.exe")).unwrap(), b"new app");
        assert_eq!(std::fs::read(dir.join("ebms.exe")).unwrap(), b"new cli");
        assert_eq!(std::fs::read(dir.join("README.md")).unwrap(), b"old readme");
        assert_eq!(
            std::fs::read(dir.join("data/config.toml")).unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read(dir.join("ebms-app.exe.old")).unwrap(),
            b"old app"
        );
        assert!(!dir.join(STAGING).exists());
        assert!(!tmp.path().parent().unwrap().join("evil").exists());

        std::fs::write(dir.join("data/x.old"), b"user file").unwrap();
        cleanup(dir);
        assert!(!dir.join("ebms-app.exe.old").exists());
        assert!(dir.join("data/x.old").exists());
    }

    #[test]
    fn install_without_required_file_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("ebms.exe"), b"old cli").unwrap();
        let zip = make_zip(&[("ebms-windows-x64/ebms.exe", b"new cli")]);
        assert!(install(&zip, dir, "ebms-app.exe").is_err());
        assert_eq!(std::fs::read(dir.join("ebms.exe")).unwrap(), b"old cli");
        assert!(!dir.join(STAGING).exists());
    }
}
