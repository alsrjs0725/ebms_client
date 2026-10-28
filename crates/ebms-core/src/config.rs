//! 연결한 서버와 앱 데이터 폴더.
//!
//! 서버는 하나만 연결한다. 다른 서버(예: stage)로 바꾸려면 지금 서버를 빼고 넣는다.
//! 로컬 데이터는 서버 id별 폴더에 따로 두므로 다시 넣으면 그대로 쓴다.
//!
//! ```text
//! <앱 데이터>/config.toml                      mount_point, extra_players, custom_players, cache_limit_gb, servers = [{id, url, name}]
//! <앱 데이터>/servers/<server_id>/index.sqlite  서버별 로컬 데이터 (paths::Paths)
//! <앱 데이터>/servers/<server_id>/session       세션키 (키체인을 못 쓸 때만)
//! <앱 데이터>/servers/<server_id>/.purge        지우다 실패한 폴더 표시 (다음에 다시 지운다)
//! ```

use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

/// 지우다 실패한 서버 폴더에 남기는 표시. 다음에 열 때 다시 지운다.
const PURGE_MARK: &str = ".purge";

/// 캐시 한도 기본값(GB).
pub const DEFAULT_CACHE_LIMIT_GB: u64 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// 가상 드라이브를 마운트할 곳. 없으면 앱 기본값.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_point: Option<PathBuf>,
    /// 연결한 서버. 파일에는 예전처럼 `[[servers]]` 배열로 쓴다(여러 서버를 담던 버전의 설정은 첫 서버만 쓴다).
    #[serde(
        default,
        rename = "servers",
        serialize_with = "server_as_list",
        deserialize_with = "first_server",
        skip_serializing_if = "Option::is_none"
    )]
    pub server: Option<ServerEntry>,
    /// 기본 구동기([`crate::players::DEFAULT_PLAYERS`]) 외에 곡 전체를 받을 수 있는 프로그램 이름.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_players: Vec<String>,
    /// 설정 창에서 고른 구동기 실행 파일. 이름이 아니라 전체 경로로 맞춘다.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_players: Vec<String>,
    /// 캐시 한도(GB). 없으면 [`DEFAULT_CACHE_LIMIT_GB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_limit_gb: Option<u64>,
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

impl ServerEntry {
    /// 루프백이 아닌 `http://` 서버. 세션키가 평문으로 오간다.
    pub fn is_insecure(&self) -> bool {
        is_insecure_url(&self.url)
    }
}

impl Config {
    /// 캐시 한도(GB).
    pub fn cache_limit_gb(&self) -> u64 {
        self.cache_limit_gb.unwrap_or(DEFAULT_CACHE_LIMIT_GB)
    }

    /// 캐시 한도(바이트).
    pub fn cache_limit(&self) -> u64 {
        self.cache_limit_gb().saturating_mul(1 << 30)
    }

    /// 곡 전체를 받을 수 있는 프로그램 (기본 구동기 + `extra_players` 이름, `custom_players` 경로).
    pub fn players(&self) -> Vec<String> {
        crate::players::DEFAULT_PLAYERS
            .iter()
            .map(|s| s.to_string())
            .chain(self.extra_players.iter().cloned())
            .chain(self.custom_players.iter().cloned())
            .collect()
    }

    /// 구동기 실행 파일을 추가한다. 이미 있으면 `false`.
    pub fn add_custom_player(&mut self, path: &str) -> Result<bool> {
        let path = path.trim();
        if !path.contains(['/', '\\']) || !path.to_lowercase().ends_with(".exe") {
            return Err(Error::Config(format!("not an .exe file path: {path}")));
        }
        let lower = path.to_lowercase();
        if self
            .custom_players
            .iter()
            .any(|p| p.to_lowercase() == lower)
        {
            return Ok(false);
        }
        self.custom_players.push(path.to_string());
        Ok(true)
    }

    /// 구동기 실행 파일을 뺀다. 없었으면 `false`.
    pub fn remove_custom_player(&mut self, path: &str) -> bool {
        let before = self.custom_players.len();
        self.custom_players.retain(|p| p != path);
        self.custom_players.len() != before
    }

    /// 연결할 서버를 정한다. 이미 연결한 서버가 있으면 오류(먼저 뺀다).
    pub fn set_server(&mut self, url: &str, name: Option<&str>) -> Result<&ServerEntry> {
        if let Some(s) = &self.server {
            return Err(Error::Config(format!(
                "a server is already added: {}. remove it first",
                s.url
            )));
        }
        let url = normalize_url(url)?;
        let parsed = Url::parse(&url).map_err(|e| Error::Config(e.to_string()))?;
        let host = parsed.host_str().unwrap_or("server");
        let id = slug(&match parsed.port() {
            Some(port) => format!("{host}-{port}"),
            None => host.to_string(),
        });
        let name = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => host.to_string(),
        };
        Ok(self.server.insert(ServerEntry { id, url, name }))
    }
}

fn server_as_list<S: Serializer>(
    server: &Option<ServerEntry>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    server.as_slice().serialize(s)
}

fn first_server<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<ServerEntry>, D::Error> {
    Ok(Vec::<ServerEntry>::deserialize(d)?.into_iter().next())
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

/// 루프백(localhost, 127.0.0.0/8, ::1)이 아닌 `http://` 주소인지.
/// 이런 서버는 세션키가 평문으로 오가므로 막지는 않고 경고한다.
pub fn is_insecure_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return true;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    !loopback
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

    /// 기본 위치. 실행 파일 옆 `data` 폴더라 C 드라이브가 아닌 곳에 둔 앱은 데이터도 그 드라이브에 둔다.
    /// 그 폴더를 만들 수 없으면(쓰기 권한이 없는 곳 등) 사용자 데이터 폴더(`%APPDATA%\ebms`)를 쓴다.
    pub fn default_root() -> Option<PathBuf> {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("data")));
        match exe {
            Some(dir) if std::fs::create_dir_all(&dir).is_ok() => Some(dir),
            _ => dirs::data_dir().map(|d| d.join("ebms")),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// 서버의 로컬 데이터 폴더.
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
    fn insecure_url_is_non_loopback_http() {
        for url in [
            "http://ebms.example.com",
            "http://192.168.0.10:8000",
            "http://[2001:db8::1]:8000",
        ] {
            assert!(is_insecure_url(url), "{url}");
        }
        for url in [
            "https://ebms.example.com",
            "http://127.0.0.1:8000",
            "http://localhost:8000",
            "http://LOCALHOST",
            "http://[::1]:8000",
        ] {
            assert!(!is_insecure_url(url), "{url}");
        }
    }

    #[test]
    fn one_server() {
        let mut c = Config::default();
        let a = c
            .set_server("https://ebms.example.com/", None)
            .unwrap()
            .clone();
        assert_eq!(a.url, "https://ebms.example.com");
        assert_eq!(a.id, "ebms-example-com");
        assert_eq!(a.name, "ebms.example.com");
        assert!(
            c.set_server("http://127.0.0.1:8000", Some("local"))
                .is_err()
        );

        c.server = None;
        let b = c
            .set_server("http://127.0.0.1:8000", Some("local"))
            .unwrap();
        assert_eq!(
            (b.id.as_str(), b.name.as_str()),
            ("127-0-0-1-8000", "local")
        );
    }

    #[test]
    fn old_config_with_several_servers_keeps_the_first() {
        let c: Config = toml::from_str(
            "[[servers]]\nid = \"a\"\nurl = \"https://a.example\"\nname = \"A\"\n\n\
             [[servers]]\nid = \"b\"\nurl = \"https://b.example\"\nname = \"B\"\n",
        )
        .unwrap();
        assert_eq!(c.server.unwrap().id, "a");
    }

    #[test]
    fn rejects_bad_urls() {
        for url in ["ftp://x", "not a url", "http://x/?a=1", "http://u:p@x"] {
            assert!(normalize_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn custom_players() {
        let mut c = Config::default();
        assert!(c.add_custom_player(r"D:\Games\Game.exe").unwrap());
        assert!(!c.add_custom_player(r"d:\games\GAME.exe").unwrap());
        assert!(c.add_custom_player("Game.exe").is_err());
        assert!(c.add_custom_player(r"D:\Games\readme.txt").is_err());
        assert!(c.players().contains(&r"D:\Games\Game.exe".to_string()));
        assert!(c.remove_custom_player(r"D:\Games\Game.exe"));
        assert!(!c.remove_custom_player(r"D:\Games\Game.exe"));
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppDir::new(dir.path());
        assert_eq!(app.load_config().unwrap(), Config::default());
        let mut c = Config::default();
        c.set_server("http://127.0.0.1:8000", None).unwrap();
        app.save_config(&c).unwrap();
        assert!(
            std::fs::read_to_string(app.config_path())
                .unwrap()
                .contains("[[servers]]")
        );
        assert_eq!(app.load_config().unwrap(), c);
    }

    #[test]
    fn purge_server_data() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppDir::new(dir.path());
        let entry = |id: &str| ServerEntry {
            id: id.into(),
            url: format!("http://{id}.example"),
            name: id.into(),
        };
        let (a, b) = (entry("a"), entry("b"));
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
