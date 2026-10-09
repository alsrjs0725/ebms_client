//! 서버 공지. 상주 앱이 켜질 때 서버마다 `GET /api/notices`를 확인해 아직 확인하지 않은 공지를 띄운다.
//!
//! 확인한 공지는 앱 데이터 폴더의 `notices_seen.json`에 서버별 `id:updated_at`으로 남긴다.
//! 서버에서 공지를 고치면 `updated_at`이 바뀌어 다시 띄운다. 게시가 끝난 공지의 기록은 다음 확인 때 지운다.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::Result;
use crate::api::Notice;
use crate::config::AppDir;
use crate::hub::Server;

/// 서버 하나의 아직 확인하지 않은 공지.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerNotices {
    pub server_id: String,
    pub server_name: String,
    pub notices: Vec<Notice>,
}

type Seen = BTreeMap<String, BTreeSet<String>>;

fn key(n: &Notice) -> String {
    format!("{}:{}", n.id, n.updated_at)
}

/// 확인한 공지 기록.
pub struct NoticeStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl NoticeStore {
    pub fn new(dir: &AppDir) -> Self {
        Self {
            path: dir.root().join("notices_seen.json"),
            lock: Mutex::new(()),
        }
    }

    fn load(&self) -> Seen {
        match std::fs::read(&self.path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                warn!(path = ?self.path, %e, "ignoring broken notice record");
                Seen::new()
            }),
            Err(_) => Seen::new(),
        }
    }

    fn save(&self, seen: &Seen) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(seen)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 게시 중인 공지(`active`) 중 확인하지 않은 것. 게시가 끝난 공지의 기록은 지운다.
    pub fn unseen(&self, server_id: &str, active: &[Notice]) -> Vec<Notice> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = self.load();
        let keys: BTreeSet<String> = active.iter().map(key).collect();
        let before = seen.get(server_id).cloned().unwrap_or_default();
        let kept: BTreeSet<String> = before.intersection(&keys).cloned().collect();
        let unseen = active
            .iter()
            .filter(|n| !kept.contains(&key(n)))
            .cloned()
            .collect();
        if kept != before {
            if kept.is_empty() {
                seen.remove(server_id);
            } else {
                seen.insert(server_id.to_string(), kept);
            }
            if let Err(e) = self.save(&seen) {
                warn!(%e, "could not save notice record");
            }
        }
        unseen
    }

    /// 사용자가 확인한 공지를 남긴다.
    pub fn mark_seen(&self, notices: &[ServerNotices]) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = self.load();
        for s in notices {
            seen.entry(s.server_id.clone())
                .or_default()
                .extend(s.notices.iter().map(key));
        }
        self.save(&seen)
    }
}

/// 서버마다 공지를 확인한다. (확인하지 않은 공지가 있는 서버, 닿지 않은 서버)
pub async fn check(
    store: &NoticeStore,
    servers: Vec<Arc<Server>>,
    timeout: Duration,
) -> (Vec<ServerNotices>, Vec<Arc<Server>>) {
    let jobs = servers.into_iter().map(|server| async move {
        let result = tokio::time::timeout(timeout, server.api().notices()).await;
        (server, result)
    });
    let mut found = Vec::new();
    let mut failed = Vec::new();
    for (server, result) in futures_util::future::join_all(jobs).await {
        match result {
            Ok(Ok(active)) => {
                let notices = store.unseen(&server.entry.id, &active);
                if !notices.is_empty() {
                    found.push(ServerNotices {
                        server_id: server.entry.id.clone(),
                        server_name: server.entry.name.clone(),
                        notices,
                    });
                }
            }
            Ok(Err(e)) => {
                warn!(server = %server.entry.name, %e, "could not check notices");
                failed.push(server);
            }
            Err(_) => {
                warn!(server = %server.entry.name, "notice check timed out");
                failed.push(server);
            }
        }
    }
    (found, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(id: u64, updated_at: i64) -> Notice {
        Notice {
            id,
            title: format!("n{id}"),
            body: String::new(),
            level: "info".into(),
            updated_at,
        }
    }

    #[test]
    fn unseen_until_marked_and_again_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let store = NoticeStore::new(&AppDir::new(dir.path()));
        let active = vec![notice(1, 10), notice(2, 20)];
        assert_eq!(store.unseen("a", &active), active);

        store
            .mark_seen(&[ServerNotices {
                server_id: "a".into(),
                server_name: "A".into(),
                notices: active.clone(),
            }])
            .unwrap();
        assert!(store.unseen("a", &active).is_empty());
        // 다른 서버와는 따로
        assert_eq!(store.unseen("b", &active), active);

        // 고친 공지는 다시 띄운다. 끝난 공지(2)의 기록은 지운다.
        let edited = vec![notice(1, 11)];
        assert_eq!(store.unseen("a", &edited), edited);
        assert!(!store.load().contains_key("a"));
        assert_eq!(store.unseen("a", &[notice(2, 20)]), vec![notice(2, 20)]);
    }
}
