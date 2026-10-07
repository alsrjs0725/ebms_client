//! 클라이언트 로그인. 루프백 리다이렉트 + PKCE (RFC 8252, RFC 7636).
//!
//! 1. `127.0.0.1`의 빈 포트에서 수신을 열고 `state`, `code_verifier`를 만든다.
//! 2. 브라우저로 `/auth/client/authorize`를 연다. 사용자가 웹에서 로그인하면 서버가
//!    `http://127.0.0.1:<포트>/callback?code=…&state=…`로 돌려보낸다.
//! 3. `state`를 확인하고 `code` + `code_verifier`를 `/api/auth/client/token`에 보내 세션키를 받는다.
//!
//! 세션키 대신 1분·1회용 `code`만 URL에 실리므로 브라우저 기록에 남아도 `code_verifier` 없이는 쓸 수 없다.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::api::{Api, Token};
use crate::{Error, Result};

const CALLBACK_PATH: &str = "/callback";
/// 요청 헤더를 이만큼까지만 읽는다.
const MAX_REQUEST: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

const DONE_HTML: &str = "<!doctype html><meta charset=utf-8><title>EBMS</title>\
<p>로그인 완료. 창을 닫아도 됩니다.</p>";
const FAIL_HTML: &str = "<!doctype html><meta charset=utf-8><title>EBMS</title>\
<p>로그인하지 못했습니다. 클라이언트에서 다시 시도해 주세요.</p>";

/// 서버의 기기 목록에 보일 이름. `{app} ({호스트 이름})`.
pub fn device_name(app: &str) -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty());
    match host {
        Some(h) => format!("{app} ({h})"),
        None => app.to_string(),
    }
}

/// PKCE 값 한 쌍.
pub struct Pkce {
    pub verifier: String,
    /// `base64url(sha256(verifier))`
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        let verifier = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// 32바이트 난수를 base64url로 (43자).
fn random_token() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| Error::Other(format!("random: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// 진행 중인 로그인 한 번.
pub struct LoginRequest {
    listener: TcpListener,
    state: String,
    pkce: Pkce,
    url: String,
}

impl LoginRequest {
    /// 루프백 수신을 열고 브라우저로 열 주소를 만든다.
    pub async fn start(api: &Api, device_name: &str) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let state = random_token()?;
        let pkce = Pkce::generate()?;
        let url = api.authorize_url(&redirect_uri, &state, &pkce.challenge, device_name)?;
        Ok(Self {
            listener,
            state,
            pkce,
            url,
        })
    }

    /// 브라우저로 열 주소. 브라우저가 열리지 않으면 사용자에게 보여준다.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 브라우저가 돌아오기를 기다렸다가 세션키를 받는다. 받은 세션키는 `api`에도 설정된다.
    pub async fn finish(self, api: &Api, timeout: Duration) -> Result<Token> {
        let code = tokio::time::timeout(timeout, wait_for_code(&self.listener, &self.state))
            .await
            .map_err(|_| Error::Auth("timed out waiting for browser login".into()))??;
        api.exchange_code(&code, &self.pkce.verifier).await
    }
}

/// `/callback`으로 올바른 `state`를 가진 요청이 올 때까지 받는다. 다른 요청은 거절하고 계속 기다린다.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        let target = match tokio::time::timeout(READ_TIMEOUT, read_target(&mut stream)).await {
            Ok(Ok(Some(t))) => t,
            Ok(Ok(None)) | Err(_) => {
                let _ = respond(&mut stream, 400, FAIL_HTML).await;
                continue;
            }
            Ok(Err(e)) => {
                debug!(%peer, %e, "callback read failed");
                continue;
            }
        };
        match parse_callback(&target, state) {
            Callback::Code(code) => {
                let _ = respond(&mut stream, 200, DONE_HTML).await;
                return Ok(code);
            }
            Callback::Denied(error) => {
                let _ = respond(&mut stream, 200, FAIL_HTML).await;
                return Err(Error::Auth(error));
            }
            Callback::Ignore => {
                let _ = respond(&mut stream, 404, FAIL_HTML).await;
            }
        }
    }
}

/// 요청 줄의 대상(`/callback?...`)을 읽는다.
async fn read_target(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > MAX_REQUEST {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut parts = text.lines().next().unwrap_or("").split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Ok(Some(target.to_string())),
        _ => Ok(None),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code(String),
    /// 서버가 `error`를 붙여 돌려보냄
    Denied(String),
    /// 다른 경로이거나 `state`가 맞지 않음
    Ignore,
}

fn parse_callback(target: &str, state: &str) -> Callback {
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignore;
    }
    let get = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    if get("state").as_deref() != Some(state) {
        return Callback::Ignore;
    }
    match (get("code"), get("error")) {
        (Some(code), _) if !code.is_empty() => Callback::Code(code),
        (_, Some(error)) => Callback::Denied(error),
        _ => Callback::Ignore,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        // RFC 7636 Appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");

        let p = Pkce::generate().unwrap();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge.len(), 43);
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback("/callback?code=abc&state=s1", "s1"),
            Callback::Code("abc".into())
        );
        assert_eq!(
            parse_callback("/callback?code=abc&state=other", "s1"),
            Callback::Ignore
        );
        assert_eq!(parse_callback("/favicon.ico", "s1"), Callback::Ignore);
        assert_eq!(
            parse_callback("/callback?error=access_denied&state=s1", "s1"),
            Callback::Denied("access_denied".into())
        );
    }
}
