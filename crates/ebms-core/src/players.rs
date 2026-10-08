//! BMS 구동기 판별. 곡 전체 다운로드(티켓 1개)는 구동기 프로세스만 일으킬 수 있다.
//!
//! 프로세스의 실행 파일과 명령줄 인자를 본다. beatoraja는 `java -jar beatoraja.jar`,
//! LR2는 Wine 아래 `C:\...\LR2body.exe`처럼 실행 파일 자체보다 인자에 이름이 나온다.

/// 기본 구동기 이름 (소문자, 파일 이름 앞부분).
pub const DEFAULT_PLAYERS: &[&str] = &["beatoraja", "lr2oraja", "lr2body", "lunaticrave2"];

/// 실행 파일 `exe`나 명령줄 인자 `args` 중 하나가 `players`의 이름으로 시작하는 파일이면 구동기로 본다.
/// 인자는 `.jar`·`.exe` 파일만 본다 (`grep -r beatoraja ~` 같은 검색어를 구동기로 오인하지 않도록).
pub fn is_player(players: &[String], exe: Option<&str>, args: &[String]) -> bool {
    let matches = |name: &str| {
        let name = name.to_lowercase();
        players
            .iter()
            .any(|p| !p.is_empty() && name.starts_with(&p.to_lowercase()))
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
}
