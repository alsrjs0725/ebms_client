//! 서버 목록과 앱 데이터 폴더.
//!
//! 한 사용자가 여러 EBMS 서버에 로그인한다. 계정·세션키·로컬 데이터는 서버마다 따로다.
//!
//! ```text
//! <앱 데이터>/config.toml                      mount_point, extra_players, servers = [{id, url, name}]
//! <앱 데이터>/servers/<server_id>/index.sqlite  서버별 로컬 데이터 (paths::Paths)
//! <앱 데이터>/servers/<server_id>/session       세션키 (키체인을 못 쓸 때만)
//! <앱 데이터>/servers/<server_id>/.purge        지우다 실패한 폴더 표시 (다음에 다시 지운다)
//! ```

use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// 지우다 실패한 서버 폴더에 남기는 표시. 다음에 열 때 다시 지운다.
const PURGE_MARK: &str = ".purge";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// 가상 드라이브를 마운트할 곳. 없으면 앱 기본값.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_point: Option<PathBuf>,
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
    /// 기본 구동기([`crate::players::DEFAULT_PLAYERS`]) 외에 곡 전체를 받을 수 있는 프로그램 이름.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_players: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// 로컬 폴더 이름으로 쓰는 식별자. 추가할 때 호스트 이름으로 만든다.
    pub id: String,
    /// 정규화된 서버 주소 (끝 `/` 없음)
    pub url: String,
    /// 표시 이름 (가상 드라이브의 서버 폴더 이름)
    pub name: String,
}

impl Config {
    /// 곡 전체를 받을 수 있는 프로그램 이름 (기본 구동기 + `extra_players`).
    pub fn players(&self) -> Vec<String> {
        crate::players::DEFAULT_PLAYERS
            .iter()
            .map(|s| s.to_string())
            .chain(self.extra_players.iter().cloned())
            .collect()
    }

    /// 서버를 추가한다. 같은 주소가 이미 있으면 오류.
    pub fn add(&mut self, url: &str, name: Option<&str>) -> Result<&ServerEntry> {
        let url = normalize_url(url)?;
        if self.servers.iter().any(|s| s.url == url) {
            return Err(Error::Config(format!("server already added: {url}")));
        }
        let parsed = Url::parse(&url).map_err(|e| Error::Config(e.to_string()))?;
        let host = parsed.host_str().unwrap_or("server");
        let base = match parsed.port() {
            Some(port) => format!("{host}-{port}"),
            None => host.to_string(),
        };
        let id = self.unique_id(&slug(&base));
        let name = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => host.to_string(),
        };
        if self.servers.iter().any(|s| s.name == name) {
            return Err(Error::Config(format!("server name already used: {name}")));
        }
        self.servers.push(ServerEntry { id, url, name });
        Ok(self.servers.last().unwrap())
    }

    /// id, 이름, 주소 중 하나로 서버를 찾아 목록에서 뺀다.
    pub fn remove(&mut self, selector: &str) -> Option<ServerEntry> {
        let i = self.position(selector)?;
        Some(self.servers.remove(i))
    }

    /// id, 이름, 주소 중 하나로 서버를 찾는다.
    pub fn find(&self, selector: &str) -> Option<&ServerEntry> {
        self.position(selector).map(|i| &self.servers[i])
    }

    /// 명령의 대상 서버. 지정하지 않으면 서버가 하나뿐일 때 그 서버.
    pub fn select(&self, selector: Option<&str>) -> Result<&ServerEntry> {
        match selector {
            Some(sel) => self
                .find(sel)
                .ok_or_else(|| Error::Config(format!("unknown server: {sel}"))),
            None => match self.servers.as_slice() {
                [only] => Ok(only),
                [] => Err(Error::Config("no server added yet".into())),
                _ => Err(Error::Config(
                    "several servers added, choose one with --server".into(),
                )),
            },
        }
    }

    fn position(&self, selector: &str) -> Option<usize> {
        let url = normalize_url(selector).ok();
        self.servers
            .iter()
            .position(|s| s.id == selector || s.name == selector || Some(&s.url) == url.as_ref())
    }

    fn unique_id(&self, base: &str) -> String {
        let mut id = base.to_string();
        let mut n = 2;
        while self.servers.iter().any(|s| s.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

/// `http(s)://host[:port][/path]` 형태로 맞춘다. 끝 `/`, 쿼리, 프래그먼트는 허용하지 않는다.
pub fn normalize_url(url: &str) -> Result<String> {
    let parsed =
        Url::parse(url.trim()).map_err(|e| Error::Config(format!("bad url {url}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::Config(format!("bad url {url}: http(s) only")));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() || !parsed.username().is_empty() {
        return Err(Error::Config(format!("bad url {url}")));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn slug(s: &str) -> String {
    let out: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "server".into() } else { out }
}

/// 앱 데이터 폴더.
#[derive(Clone, Debug)]
pub struct AppDir {
    root: PathBuf,
}

impl AppDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// OS 기본 위치 (Windows `%APPDATA%\ebms`, Linux `~/.local/share/ebms`).
    pub fn default_root() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("ebms"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// 서버별 로컬 데이터 폴더.
    pub fn server_dir(&self, server: &ServerEntry) -> PathBuf {
        self.root.join("servers").join(&server.id)
    }

    /// 서버의 로컬 데이터(인덱스·차트·캐시·세션 파일)를 지운다.
    /// 다른 프로그램이 파일을 열고 있어 다 지우지 못하면 표시를 남기고 `false`를 돌려준다.
    /// 남은 것은 [`Self::purge_pending`]이나 [`Self::prepare_server_dir`]가 다시 지운다.
    pub fn purge_server_dir(&self, server: &ServerEntry) -> Result<bool> {
        purge_dir(&self.server_dir(server))
    }

    /// 지우다 만 서버 폴더를 다시 지워 본다. 앱을 시작할 때 부른다.
    pub fn purge_pending(&self) {
        let Ok(entries) = std::fs::read_dir(self.root.join("servers")) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.join(PURGE_MARK).exists()
                && let Err(e) = purge_dir(&dir)
            {
                tracing::warn!(?dir, %e, "could not purge server data");
            }
        }
    }

    /// 서버를 열기 전에 부른다. 같은 id로 지우다 만 데이터가 있으면 먼저 지운다.
    pub fn prepare_server_dir(&self, server: &ServerEntry) -> Result<()> {
        let dir = self.server_dir(server);
        if dir.join(PURGE_MARK).exists() && !purge_dir(&dir)? {
            return Err(Error::Config(format!(
                "local data of a removed server is still in use: {}. close programs using it and try again",
                dir.display()
            )));
        }
        Ok(())
    }

    pub fn load_config(&self) -> Result<Config> {
        match std::fs::read_to_string(self.config_path()) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", self.config_path().display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let text = toml::to_string(config).map_err(|e| Error::Config(e.to_string()))?;
        let tmp = self.root.join("config.toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, self.config_path())?;
        Ok(())
    }
}

/// 폴더를 지운다. 다 못 지우면 표시를 남기고 `false`.
fn purge_dir(dir: &Path) -> Result<bool> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => {
            tracing::warn!(?dir, %e, "server data not fully removed, will retry later");
            std::fs::write(dir.join(PURGE_MARK), b"")?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_find_remove() {
        let mut c = Config::default();
        let a = c.add("https://ebms.example.com/", None).unwrap().clone();
        assert_eq!(a.url, "https://ebms.example.com");
        assert_eq!(a.id, "ebms-example-com");
        assert_eq!(a.name, "ebms.example.com");
        let b = c
            .add("http://127.0.0.1:8000", Some("local"))
            .unwrap()
            .clone();
        assert_eq!(b.id, "127-0-0-1-8000");

        assert!(c.add("https://ebms.example.com", None).is_err());
        assert_eq!(c.find("local").unwrap().id, b.id);
        assert_eq!(c.find("https://ebms.example.com/").unwrap().id, a.id);
        assert!(c.select(None).is_err());
        assert_eq!(c.select(Some(&a.id)).unwrap().url, a.url);

        assert_eq!(c.remove("local").unwrap(), b);
        assert_eq!(c.select(None).unwrap().id, a.id);
    }

    #[test]
    fn same_host_gets_unique_id() {
        let mut c = Config::default();
        c.add("https://h.example/a", Some("a")).unwrap();
        let b = c.add("https://h.example/b", Some("b")).unwrap();
        assert_eq!(b.id, "h-example-2");
    }

    #[test]
    fn rejects_bad_urls() {
        for url in ["ftp://x", "not a url", "http://x/?a=1", "http://u:p@x"] {
            assert!(normalize_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppDir::new(dir.path());
        assert_eq!(app.load_config().unwrap(), Config::default());
        let mut c = Config::default();
        c.add("http://127.0.0.1:8000", None).unwrap();
        app.save_config(&c).unwrap();
        assert_eq!(app.load_config().unwrap(), c);
    }

    #[test]
    fn purge_server_data() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppDir::new(dir.path());
        let mut c = Config::default();
        let a = c.add("http://a.example", None).unwrap().clone();
        let b = c.add("http://b.example", None).unwrap().clone();
        for s in [&a, &b] {
            std::fs::create_dir_all(app.server_dir(s).join("cache/1")).unwrap();
            std::fs::write(app.server_dir(s).join("cache/1/x.ogg"), b"x").unwrap();
        }
        assert!(app.purge_server_dir(&a).unwrap());
        assert!(!app.server_dir(&a).exists());
        assert!(app.server_dir(&b).exists());
        // 없는 폴더도 성공
        assert!(app.purge_server_dir(&a).unwrap());

        // 지우다 만 폴더는 다음에 지운다.
        std::fs::write(app.server_dir(&b).join(PURGE_MARK), b"").unwrap();
        app.prepare_server_dir(&b).unwrap();
        assert!(!app.server_dir(&b).exists());
        std::fs::create_dir_all(app.server_dir(&b)).unwrap();
        std::fs::write(app.server_dir(&b).join(PURGE_MARK), b"").unwrap();
        app.purge_pending();
        assert!(!app.server_dir(&b).exists());
    }
}
