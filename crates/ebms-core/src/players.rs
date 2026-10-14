//! BMS 구동기 판별. 곡 전체 다운로드(티켓 1개)는 구동기 프로세스만 일으킬 수 있다.
//!
//! 프로세스의 실행 파일과 명령줄 인자를 본다. beatoraja는 `java -jar beatoraja.jar`,
//! LR2는 Wine 아래 `C:\...\LR2body.exe`처럼 실행 파일 자체보다 인자에 이름이 나온다.
//! 설정 창에서 고른 실행 파일은 이름이 아니라 전체 경로로 맞춘다.

use std::sync::{Arc, RwLock};

/// 기본 구동기 이름 (소문자, 파일 이름 앞부분).
pub const DEFAULT_PLAYERS: &[&str] = &["beatoraja", "lr2oraja", "lr2body", "lunaticrave2"];

/// 실행 파일 `exe`나 명령줄 인자 `args` 중 하나가 `players`의 이름으로 시작하는 파일이면 구동기로 본다.
/// 인자는 `.jar`·`.exe` 파일만 본다 (`grep -r beatoraja ~` 같은 검색어를 구동기로 오인하지 않도록).
pub fn is_player(players: &[String], exe: Option<&str>, args: &[String]) -> bool {
    // 경로 구분자가 있는 항목은 사용자가 고른 실행 파일. 이름이 아니라 전체 경로가 같아야 한다.
    let (paths, names): (Vec<&String>, Vec<&String>) = players
        .iter()
        .filter(|p| !p.is_empty())
        .partition(|p| is_path(p));
    let same_path = |path: &str| {
        let path = normalize_path(path);
        paths.iter().any(|p| normalize_path(p) == path)
    };
    if exe.is_some_and(same_path) || args.iter().any(|a| same_path(a)) {
        return true;
    }
    let matches = |name: &str| {
        let name = name.to_lowercase();
        names.iter().any(|p| name.starts_with(&p.to_lowercase()))
    };
    if exe.map(file_name).is_some_and(matches) {
        return true;
    }
    args.iter()
        // 클래스패스(`-cp a.jar:b.jar`)도 나눠 본다.
        .flat_map(|a| a.split([':', ';']))
        .map(file_name)
        .filter(|n| {
            let n = n.to_lowercase();
            n.ends_with(".jar") || n.ends_with(".exe")
        })
        .any(matches)
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn is_path(player: &str) -> bool {
    player.contains(['/', '\\'])
}

/// Windows 경로는 대소문자와 `/`·`\`를 가리지 않는다.
fn normalize_path(path: &str) -> String {
    path.trim().replace('/', "\\").to_lowercase()
}

/// 마운트한 드라이브와 설정 창이 같이 쓰는 구동기 목록. 바꾸면 다시 마운트하지 않아도 바로 적용된다.
#[derive(Clone, Default)]
pub struct Players(Arc<RwLock<Arc<[String]>>>);

impl Players {
    pub fn new(players: Vec<String>) -> Self {
        Self(Arc::new(RwLock::new(players.into())))
    }

    pub fn set(&self, players: Vec<String>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = players.into();
    }

    pub fn get(&self) -> Arc<[String]> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// [`is_player`]를 지금 목록으로 부른다.
    pub fn is_player(&self, exe: Option<&str>, args: &[String]) -> bool {
        is_player(&self.get(), exe, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        DEFAULT_PLAYERS.iter().map(|s| s.to_string()).collect()
    }

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn players() {
        let p = defaults();
        assert!(is_player(
            &p,
            Some("/usr/lib/jvm/bin/java"),
            &args(&["java", "-jar", "beatoraja.jar"])
        ));
        assert!(is_player(
            &p,
            Some("/usr/bin/java"),
            &args(&[
                "java",
                "-cp",
                "lib/*:beatoraja-0.8.7.jar",
                "bms.player.MainLoader"
            ])
        ));
        assert!(is_player(
            &p,
            Some("/opt/wine/bin/wine64-preloader"),
            &args(&["C:\\LR2\\LR2body.exe"])
        ));
        assert!(is_player(&p, Some("/opt/LR2oraja/lr2oraja"), &[]));
    }

    #[test]
    fn others() {
        let p = defaults();
        for (exe, a) in [
            (
                "/usr/bin/rsync",
                args(&["rsync", "-a", "/home/me/", "/backup"]),
            ),
            (
                "/usr/bin/grep",
                args(&["grep", "-r", "beatoraja", "/home/me"]),
            ),
            (
                "/usr/bin/baloo_file_extractor",
                args(&["baloo_file_extractor"]),
            ),
            ("/usr/bin/java", args(&["java", "-jar", "other.jar"])),
        ] {
            assert!(!is_player(&p, Some(exe), &a), "{exe}");
        }
        assert!(!is_player(&p, None, &[]));
    }

    #[test]
    fn custom_exe_matches_full_path_only() {
        let mut p = defaults();
        p.push(r"D:\Games\MyPlayer\Game.exe".into());
        assert!(is_player(&p, Some(r"d:/games/myplayer/GAME.EXE"), &[]));
        // Wine처럼 인자로 나와도 같은 경로면 구동기
        assert!(is_player(
            &p,
            Some("/opt/wine/bin/wine64-preloader"),
            &args(&[r"D:\Games\MyPlayer\Game.exe"])
        ));
        // 이름만 같은 다른 파일은 아니다
        assert!(!is_player(&p, Some(r"C:\Other\Game.exe"), &[]));
        assert!(!is_player(&p, Some(r"D:\Games\MyPlayer\Game.exe.bak"), &[]));
    }

    #[test]
    fn shared_players_update() {
        let players = Players::new(defaults());
        let exe = Some(r"D:\Games\Game.exe");
        assert!(!players.is_player(exe, &[]));
        let mut p = defaults();
        p.push(r"D:\Games\Game.exe".into());
        players.clone().set(p);
        assert!(players.is_player(exe, &[]));
    }
}
