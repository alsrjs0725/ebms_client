//! 파일을 연 프로세스. 곡 다운로드가 필요할 때만 실행 파일과 명령줄을 읽는다.

use std::sync::Arc;

use ebms_core::fs::Caller;
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, HANDLE, UNICODE_STRING};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

/// `NtQueryInformationProcess`의 ProcessCommandLineInformation (Windows 8.1+)
const PROCESS_COMMAND_LINE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(60);

/// 다운로드가 끝난 뒤 런타임 스레드에서도 쓰므로 데이터를 소유한다.
pub(crate) struct ProcessCaller {
    pid: u32,
    players: Arc<[String]>,
}

impl ProcessCaller {
    pub(crate) fn new(pid: u32, players: Arc<[String]>) -> Self {
        Self { pid, players }
    }

    fn info(&self) -> (Option<String>, Vec<String>) {
        let Some(process) = Process::open(self.pid) else {
            return (None, Vec::new());
        };
        let args = process.command_line().map(|c| split_args(&c));
        (process.exe(), args.unwrap_or_default())
    }
}

impl Caller for ProcessCaller {
    fn name(&self) -> String {
        let (exe, args) = self.info();
        let exe = exe.as_deref().map(file_name).unwrap_or("?");
        // javaw처럼 실행 파일만으로는 알 수 없는 경우를 위해 명령줄도 남긴다.
        let args: String = args.join(" ").chars().take(200).collect();
        format!("{exe}[{}] {args}", self.pid)
    }

    fn is_player(&self) -> bool {
        let (exe, args) = self.info();
        ebms_core::players::is_player(&self.players, exe.as_deref(), &args)
    }
}

struct Process(HANDLE);

impl Process {
    fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        // SAFETY: 실패하면 Err. 성공한 핸들은 Drop에서 닫는다.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .ok()
            .map(Self)
    }

    fn exe(&self) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: buf와 len은 호출 동안 유효하다.
        unsafe {
            QueryFullProcessImageNameW(
                self.0,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    fn command_line(&self) -> Option<String> {
        // 결과는 UNICODE_STRING 뒤에 문자열이 이어지는 형태. 정렬을 맞추려고 u64로 잡는다.
        let mut buf = vec![0u64; 4096];
        for _ in 0..2 {
            let size = (buf.len() * 8) as u32;
            let mut needed = 0u32;
            // SAFETY: buf는 size 바이트를 쓸 수 있다.
            let status = unsafe {
                NtQueryInformationProcess(
                    self.0,
                    PROCESS_COMMAND_LINE_INFORMATION,
                    buf.as_mut_ptr().cast(),
                    size,
                    &mut needed,
                )
            };
            if status.is_ok() {
                // SAFETY: 성공하면 buf 앞부분은 UNICODE_STRING이고, Buffer는 buf 안을 가리킨다.
                let s = unsafe { &*buf.as_ptr().cast::<UNICODE_STRING>() };
                if s.Buffer.is_null() {
                    return None;
                }
                // SAFETY: Length 바이트만큼 유효하다.
                let wide =
                    unsafe { std::slice::from_raw_parts(s.Buffer.0, usize::from(s.Length) / 2) };
                return Some(String::from_utf16_lossy(wide));
            }
            if needed as usize <= buf.len() * 8 {
                return None;
            }
            buf = vec![0u64; (needed as usize).div_ceil(8)];
        }
        None
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: open에서 연 핸들이다.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Windows 명령줄을 인자로 나눈다. 따옴표 안의 공백은 나누지 않고 따옴표는 뺀다.
/// 구동기 판별에 쓰는 정도라 `\"` 같은 이스케이프는 따지지 않는다.
fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has_arg = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has_arg = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has_arg {
                    args.push(std::mem::take(&mut cur));
                    has_arg = false;
                }
            }
            c => {
                cur.push(c);
                has_arg = true;
            }
        }
    }
    if has_arg {
        args.push(cur);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_command_line() {
        assert_eq!(
            split_args(
                r#""C:\Program Files\Java\bin\javaw.exe" -Xmx4g -jar "D:\BMS\beatoraja.jar""#
            ),
            [
                r"C:\Program Files\Java\bin\javaw.exe",
                "-Xmx4g",
                "-jar",
                r"D:\BMS\beatoraja.jar"
            ]
        );
        assert_eq!(split_args("  a  b "), ["a", "b"]);
        assert_eq!(split_args(r#"x "" y"#), ["x", "", "y"]);
    }

    #[test]
    fn detects_players() {
        let players: Vec<String> = ebms_core::players::DEFAULT_PLAYERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = split_args(r#"javaw.exe -cp "beatoraja.jar;ir\*" bms.player.MainLoader"#);
        assert!(ebms_core::players::is_player(
            &players,
            Some(r"C:\Program Files\Java\bin\javaw.exe"),
            &args
        ));
        let args = split_args(r#""C:\Windows\explorer.exe""#);
        assert!(!ebms_core::players::is_player(
            &players,
            Some(r"C:\Windows\explorer.exe"),
            &args
        ));
    }
}
