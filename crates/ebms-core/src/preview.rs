//! Qwilight 미리듣기 동안 플레이 파일을 받지 않기.
//!
//! Qwilight는 곡 고르는 화면에서 미리듣기할 때 preview 음원을 읽은 뒤 키음·BGA를 모두 읽는다(티켓 1개).
//! preview 음원이 있으면 그것만 재생하고 자동 플레이는 멈춰 두므로 키음·BGA 내용은 쓰이지 않는다.
//! 그래서 Qwilight가 preview 음원을 읽은 직후 받지 않은 플레이 파일을 읽으면 받지 않고 읽기 오류로 돌린다.
//! Qwilight는 못 읽은 키음·BGA를 건너뛰고, 플레이를 시작하면 파일을 다시 읽는다.
//!
//! 플레이 시작은 이렇게 알아본다.
//! - 미리듣기 로드는 preview 음원을 읽고 곧바로 파일을 연달아 읽는다. 곡의 플레이 파일 읽기가 [`IDLE`] 동안
//!   없으면 미리듣기 로드가 끝난 것으로 보고, 그 뒤의 읽기는 받는다(플레이 시작은 preview 음원을 읽지 않는다).
//! - 미리듣기 로드 중에 플레이를 시작하면 Qwilight는 로드를 취소하고 같은 파일을 처음부터 다시 읽는다.
//!   거절한 파일을 [`REPLAY`] 이상 지나 처음부터 다시 읽으면 플레이 시작으로 보고 받는다.
//!
//! 미리듣기 중에 옵션을 바꿔 다시 로드하면 preview 음원을 읽지 않으므로 지금처럼 받는다(티켓 1개).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 미리듣기 로드가 끝났다고 보는, 곡의 플레이 파일 읽기가 없는 시간.
pub const IDLE: Duration = Duration::from_secs(2);
/// 거절한 파일을 이만큼 지나 처음부터 다시 읽으면 플레이 시작으로 본다.
pub const REPLAY: Duration = Duration::from_millis(300);
/// 미리듣기 상태를 두는 곡 수. 넘으면 끝난 것부터 정리한다.
const MAX_SONGS: usize = 64;

struct Window {
    /// 마지막으로 preview 음원이나 플레이 파일을 읽은 시각
    last: Instant,
    /// 거절한 파일(곡 안 경로)과 처음 거절한 시각
    refused: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct PreviewGate {
    songs: Mutex<HashMap<u32, Window>>,
}

impl PreviewGate {
    /// Qwilight가 곡의 preview 음원을 처음부터 읽었다.
    pub fn opened(&self, song_id: u32, now: Instant) {
        let mut songs = self.songs.lock().unwrap_or_else(|e| e.into_inner());
        if songs.len() >= MAX_SONGS {
            songs.retain(|_, w| now.saturating_duration_since(w.last) <= IDLE);
        }
        songs.insert(
            song_id,
            Window {
                last: now,
                refused: HashMap::new(),
            },
        );
    }

    /// 곡이 미리듣기 상태일 수 있는지. 프로세스를 알아보기 전에 싸게 거른다.
    pub fn previewing(&self, song_id: u32) -> bool {
        self.songs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&song_id)
    }

    /// Qwilight가 받지 않은 플레이 파일 `path`를 `offset`부터 읽는다. 미리듣기 로드라 받지 않을 것이면 true.
    pub fn refuse(&self, song_id: u32, path: &str, offset: u64, now: Instant) -> bool {
        let mut songs = self.songs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = songs.get_mut(&song_id) else {
            return false;
        };
        if now.saturating_duration_since(w.last) > IDLE {
            songs.remove(&song_id);
            return false;
        }
        match w.refused.get(path) {
            Some(&at) if offset == 0 && now.saturating_duration_since(at) >= REPLAY => {
                songs.remove(&song_id);
                false
            }
            Some(_) => {
                w.last = now;
                true
            }
            None => {
                w.refused.insert(path.to_string(), now);
                w.last = now;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn refuses_only_after_preview() {
        let gate = PreviewGate::default();
        let t = Instant::now();
        assert!(!gate.refuse(1, "a.wav", 0, t));
        gate.opened(1, t);
        assert!(gate.refuse(1, "a.wav", 0, t + ms(100)));
        assert!(gate.refuse(1, "b.wav", 0, t + ms(120)));
        // 다른 곡은 그대로 받는다.
        assert!(!gate.refuse(2, "a.wav", 0, t + ms(120)));
    }

    #[test]
    fn idle_ends_preview() {
        let gate = PreviewGate::default();
        let t = Instant::now();
        gate.opened(1, t);
        assert!(gate.refuse(1, "a.wav", 0, t + ms(500)));
        // 미리듣기 로드가 끝나고 한참 뒤 플레이를 시작하면 받는다.
        assert!(!gate.refuse(1, "c.wav", 0, t + ms(500) + IDLE + ms(1)));
        assert!(!gate.refuse(1, "a.wav", 0, t + ms(500) + IDLE + ms(2)));
    }

    #[test]
    fn replay_starts_play() {
        let gate = PreviewGate::default();
        let t = Instant::now();
        gate.opened(1, t);
        assert!(gate.refuse(1, "a.wav", 0, t));
        // 같은 파일을 곧바로 다시 열거나(BGA 해시 뒤 열기) 이어 읽는 것은 미리듣기 로드
        assert!(gate.refuse(1, "a.wav", 0, t + ms(50)));
        assert!(gate.refuse(1, "a.wav", 4096, t + ms(400)));
        // 거절한 파일을 처음부터 다시 읽으면 플레이 시작. 그 뒤로는 모두 받는다.
        assert!(!gate.refuse(1, "a.wav", 0, t + ms(400)));
        assert!(!gate.refuse(1, "b.wav", 0, t + ms(401)));
    }
}
