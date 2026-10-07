//! 서버별 세션키 저장.
//!
//! OS 키체인(Windows 자격 증명 관리자, macOS 키체인, Linux Secret Service)에 서버 주소별로 둔다.
//! 키체인을 쓸 수 없으면 서버 데이터 폴더의 `session` 파일(권한 600)에 둔다.
//!
//! 키체인 호출은 블로킹이므로 tokio 런타임 밖(또는 `spawn_blocking`)에서 부른다.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::Result;

const KEYRING_SERVICE: &str = "ebms";
const SESSION_FILE: &str = "session";

pub struct SessionStore {
    url: String,
    file: PathBuf,
    use_keyring: bool,
}

impl SessionStore {
    /// `url`은 키체인 항목 이름, `server_dir`은 키체인을 못 쓸 때 파일을 둘 폴더.
    pub fn new(url: &str, server_dir: &Path, use_keyring: bool) -> Self {
        Self {
            url: url.to_string(),
            file: server_dir.join(SESSION_FILE),
            use_keyring,
        }
    }

    fn entry(&self) -> Option<keyring::Entry> {
        if !self.use_keyring {
            return None;
        }
        match keyring::Entry::new(KEYRING_SERVICE, &self.url) {
            Ok(e) => Some(e),
            Err(e) => {
                debug!(%e, "keyring unavailable, using session file");
                None
            }
        }
    }

    pub fn load(&self) -> Result<Option<String>> {
        if let Some(entry) = self.entry() {
            match entry.get_password() {
                Ok(key) => return Ok(Some(key)),
                Err(keyring::Error::NoEntry) => {}
                Err(e) => debug!(%e, "keyring read failed, trying session file"),
            }
        }
        match std::fs::read_to_string(&self.file) {
            Ok(key) => Ok(Some(key.trim().to_string()).filter(|k| !k.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, key: &str) -> Result<()> {
        if let Some(entry) = self.entry() {
            match entry.set_password(key) {
                Ok(()) => {
                    remove_file(&self.file)?;
                    return Ok(());
                }
                Err(e) => debug!(%e, "keyring write failed, using session file"),
            }
        }
        write_private(&self.file, key)
    }

    pub fn clear(&self) -> Result<()> {
        if let Some(entry) = self.entry() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => debug!(%e, "keyring delete failed"),
            }
        }
        remove_file(&self.file)
    }
}

fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// 본인만 읽을 수 있는 파일로 쓴다.
fn write_private(path: &Path, data: &str) -> Result<()> {
    use std::io::Write;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new("http://127.0.0.1:8000", dir.path(), false);
        assert_eq!(store.load().unwrap(), None);
        store.save("secret").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(SESSION_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        store.clear().unwrap();
    }
}
