//! 개발자 도구의 파일 열기 기록. 가상 드라이브에서 열린 파일을 최근 [`CAPACITY`]개까지 메모리에만 둔다.
//!
//! 개발자 도구 창이 떠 있는 동안([`OpenLog::set_enabled`])만 기록한다. 꺼져 있으면 마운트 쪽은
//! [`OpenLog::enabled`]만 보고 프로세스 정보도 읽지 않는다.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// 메모리에 두는 최대 개수. 넘으면 오래된 것부터 버린다.
pub const CAPACITY: usize = 1000;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpenEntry {
    /// 1부터 늘어나는 번호. 창은 마지막으로 받은 번호 뒤만 가져간다.
    pub seq: u64,
    /// 연 시각 (유닉스 밀리초)
    pub at_ms: u64,
    /// 드라이브 안 경로 (`\서버\곡\파일`)
    pub path: String,
    pub size: u64,
    pub pid: u32,
    /// 연 프로세스의 실행 파일. 못 읽으면 None
    pub process: Option<String>,
}

#[derive(Default)]
struct Inner {
    enabled: AtomicBool,
    entries: Mutex<Entries>,
}

#[derive(Default)]
struct Entries {
    next_seq: u64,
    list: VecDeque<OpenEntry>,
}

/// 마운트한 드라이브와 개발자 도구 창이 같이 쓰는 기록.
#[derive(Clone, Default)]
pub struct OpenLog(Arc<Inner>);

impl OpenLog {
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Ordering::Relaxed);
    }

    /// 기록한다. 꺼져 있으면 버린다. `process`는 켜져 있을 때만 부른다.
    pub fn record(
        &self,
        path: String,
        size: u64,
        pid: u32,
        process: impl FnOnce() -> Option<String>,
    ) {
        if !self.enabled() {
            return;
        }
        let process = process();
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let mut e = self.lock();
        e.next_seq += 1;
        let seq = e.next_seq;
        if e.list.len() >= CAPACITY {
            e.list.pop_front();
        }
        e.list.push_back(OpenEntry {
            seq,
            at_ms,
            path,
            size,
            pid,
            process,
        });
    }

    /// `after`번 뒤의 기록. 0이면 남아 있는 전부.
    pub fn since(&self, after: u64) -> Vec<OpenEntry> {
        let e = self.lock();
        let skip = e.list.partition_point(|x| x.seq <= after);
        e.list.iter().skip(skip).cloned().collect()
    }

    pub fn clear(&self) {
        self.lock().list.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.0.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(log: &OpenLog, path: &str) {
        log.record(path.into(), 1, 7, || Some("x.exe".into()));
    }

    #[test]
    fn records_only_when_enabled() {
        let log = OpenLog::default();
        log.record("a".into(), 1, 7, || panic!("process read while disabled"));
        assert!(log.since(0).is_empty());
        log.set_enabled(true);
        add(&log, "b");
        log.set_enabled(false);
        add(&log, "c");
        let got = log.since(0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "b");
        assert_eq!(got[0].process.as_deref(), Some("x.exe"));
    }

    #[test]
    fn keeps_latest_capacity() {
        let log = OpenLog::default();
        log.set_enabled(true);
        for i in 0..CAPACITY + 5 {
            add(&log, &i.to_string());
        }
        let all = log.since(0);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all[0].path, "5");
        assert_eq!(all[0].seq, 6);
        let tail = log.since(CAPACITY as u64 + 3);
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), [1004, 1005]);
        log.clear();
        assert!(log.since(0).is_empty());
        add(&log, "next");
        assert_eq!(log.since(0)[0].seq, CAPACITY as u64 + 6);
    }
}
