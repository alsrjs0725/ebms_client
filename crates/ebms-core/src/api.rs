use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

use crate::manifest::SongManifest;
use crate::{Error, Result};

/// 서버가 Retry-After를 주지 않았을 때 기다릴 시간(초).
const DEFAULT_RETRY_AFTER: u64 = 60;

/// Retry-After 상한(초). 서버 버그나 악의적인 값으로 오래 멈추거나 시각 계산이 넘치지 않게 자른다.
pub const MAX_RETRY_AFTER: u64 = 60 * 60;

/// 응답 바이트가 이만큼 오지 않으면 연결이 멈춘 것으로 본다.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 플레이 다운로드가 중간에 끊겼을 때 이어받기를 시도할 횟수.
const MAX_RESUME: u32 = 3;

/// 사전 파일 하나를 받는 전체 시간 상한. 감속 중에도 프리뷰 몇 MB는 받을 수 있게 넉넉히.
const PRE_FILE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
pub struct Version {
    pub api: u32,
    pub server: Option<String>,
    /// 로그인에 쓸 수 있는 OAuth. 비어 있으면 로그인을 받지 않는 서버.
    #[serde(default)]
    pub auth: Vec<String>,
}

/// `GET /api/notices`의 공지 하나.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub id: u64,
    pub title: String,
    /// 일반 텍스트
    #[serde(default)]
    pub body: String,
    /// `info` | `warning`
    #[serde(default)]
    pub level: String,
    /// unix 초. 공지를 고치면 바뀐다.
    pub updated_at: i64,
}

/// `POST /api/auth/client/token` 응답.
#[derive(Deserialize)]
pub struct Token {
    pub session_key: String,
    /// unix 초
    pub expires_at: i64,
    pub user: User,
}

/// 로그에 세션키가 새지 않게 가린다.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("session_key", &REDACTED)
            .field("expires_at", &self.expires_at)
            .field("user", &self.user)
            .finish()
    }
}

/// `Debug`에서 세션키 대신 보여준다.
pub(crate) const REDACTED: &str = "<redacted>";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    /// 서버 안의 계정 UUID. 서버마다 다르다.
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
}

/// `GET /api/me` 응답.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Me {
    #[serde(flatten)]
    pub user: User,
    /// 이 계정에 연결된 OAuth(Google, Discord)
    #[serde(default)]
    pub oauths: Vec<LinkedOAuth>,
    pub tickets: Tickets,
    pub pre: PreUsage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkedOAuth {
    pub oauth: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tickets {
    pub available: u32,
    pub max: u32,
    pub refill_seconds: u64,
    /// 다음 티켓이 차는 시각(unix 초). 가득 찼으면 None.
    pub next_refill_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreUsage {
    /// `YYYY-MM`
    pub month: String,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub throttled_kbps: u64,
    pub throttled: bool,
}

/// 서버 HTTP API 클라이언트. 세션키가 있으면 모든 요청에 `Authorization: Bearer`로 싣는다.
pub struct Api {
    base: String,
    http: reqwest::Client,
    session: RwLock<Option<String>>,
    /// 401을 받았음. 다시 로그인할 때까지 켜져 있다.
    unauthorized: AtomicBool,
}

/// 세션키는 가리고 있는지만 보여준다.
impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .field("session", &self.has_session().then_some(REDACTED))
            .field("unauthorized", &self.unauthorized.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Api {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ebms-client/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            session: RwLock::new(None),
            unauthorized: AtomicBool::new(false),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// 요청에 실을 세션키를 바꾼다.
    pub fn set_session(&self, key: Option<String>) {
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = key;
        self.unauthorized.store(false, Ordering::SeqCst);
    }

    pub fn has_session(&self) -> bool {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 세션키가 없거나 서버가 401로 거절했으면 true. 설정 창에 "다시 로그인 필요"로 보여준다.
    pub fn needs_login(&self) -> bool {
        !self.has_session() || self.unauthorized.load(Ordering::SeqCst)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        let req = self.http.request(method, url);
        match &*self.session.read().unwrap_or_else(|e| e.into_inner()) {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let resp = self.request(Method::GET, self.url(path)).send().await?;
        self.check(resp, StatusCode::OK)
    }

    fn check(&self, resp: reqwest::Response, expected: StatusCode) -> Result<reqwest::Response> {
        match resp.status() {
            s if s == expected => Ok(resp),
            StatusCode::UNAUTHORIZED => {
                // 조용히 로그아웃 상태로 바꾼다. 알림은 띄우지 않는다.
                if !self.unauthorized.swap(true, Ordering::SeqCst) {
                    warn!(server = %self.base, "session rejected, login required");
                }
                Err(Error::Unauthorized)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(DEFAULT_RETRY_AFTER)
                    .min(MAX_RETRY_AFTER);
                Err(Error::NoTicket { retry_after })
            }
            s => Err(Error::Status {
                status: s.as_u16(),
                url: resp.url().to_string(),
            }),
        }
    }

    pub async fn version(&self) -> Result<Version> {
        Ok(self.get("/api/version").await?.json().await?)
    }

    /// 지금 게시 중인 공지. 공지 API가 없는 이전 서버면 빈 목록.
    pub async fn notices(&self) -> Result<Vec<Notice>> {
        match self.get("/api/notices").await {
            Ok(resp) => Ok(resp.json().await?),
            Err(Error::Status { status: 404, .. }) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    // ---- 로그인 ----

    /// 브라우저로 열 클라이언트 로그인 주소.
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        code_challenge: &str,
        device_name: &str,
    ) -> Result<String> {
        let mut url = Url::parse(&self.url("/auth/client/authorize"))
            .map_err(|e| Error::Config(format!("bad server url {}: {e}", self.base)))?;
        url.query_pairs_mut()
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("device_name", device_name);
        Ok(url.into())
    }

    /// 1회용 코드와 PKCE verifier를 세션키로 바꾼다.
    pub async fn exchange_code(&self, code: &str, code_verifier: &str) -> Result<Token> {
        let resp = self
            .http
            .post(self.url("/api/auth/client/token"))
            .json(&serde_json::json!({"code": code, "code_verifier": code_verifier}))
            .send()
            .await?;
        if resp.status().is_client_error() {
            #[derive(Deserialize)]
            struct Detail {
                detail: String,
            }
            let status = resp.status();
            let detail = resp
                .json::<Detail>()
                .await
                .map(|d| d.detail)
                .unwrap_or_else(|_| status.to_string());
            return Err(Error::Auth(detail));
        }
        let token: Token = self.check(resp, StatusCode::OK)?.json().await?;
        self.set_session(Some(token.session_key.clone()));
        Ok(token)
    }

    /// 서버에서 현재 세션키를 폐기한다. 이미 무효인 세션키면 그대로 성공으로 본다.
    pub async fn logout(&self) -> Result<()> {
        let resp = self
            .request(Method::POST, self.url("/api/auth/client/logout"))
            .send()
            .await?;
        let result = match self.check(resp, StatusCode::NO_CONTENT) {
            Ok(_) | Err(Error::Unauthorized) => Ok(()),
            Err(e) => Err(e),
        };
        self.set_session(None);
        result
    }

    pub async fn me(&self) -> Result<Me> {
        Ok(self.get("/api/me").await?.json().await?)
    }

    // ---- 사전 다운로드 ----

    pub async fn chart_hash(&self) -> Result<BTreeMap<u32, String>> {
        self.hash_map("/api/pre/charthash").await
    }

    pub async fn pre_hash(&self) -> Result<BTreeMap<u32, String>> {
        self.hash_map("/api/pre/assethash").await
    }

    pub async fn manifest_hash(&self) -> Result<BTreeMap<u32, String>> {
        self.hash_map("/api/pre/manifest/hash").await
    }

    async fn hash_map(&self, path: &str) -> Result<BTreeMap<u32, String>> {
        // JSON 키는 문자열이다.
        let raw: BTreeMap<String, String> = self.get(path).await?.json().await?;
        raw.into_iter()
            .map(|(k, v)| {
                k.parse()
                    .map(|k| (k, v))
                    .map_err(|_| Error::Other(format!("bad chunk id {k:?} from {path}")))
            })
            .collect()
    }

    /// 매니페스트 청크를 받아 압축 해제된 JSON의 sha256과 함께 돌려준다.
    pub async fn manifest(&self, chunk_id: u32) -> Result<(String, Vec<SongManifest>)> {
        let body = self
            .get(&format!("/api/pre/manifest/{chunk_id}"))
            .await?
            .bytes()
            .await?;
        Ok((crate::sha256_hex(&body), serde_json::from_slice(&body)?))
    }

    /// 차트 청크 zip을 `dest`에 저장하고 sha256을 돌려준다.
    pub async fn download_chart_chunk(&self, chunk_id: u32, dest: &Path) -> Result<String> {
        let resp = self.get(&format!("/api/pre/chart/{chunk_id}")).await?;
        save(resp, dest).await
    }

    /// 사전 청크 zip을 `dest`에 저장하고 sha256을 돌려준다.
    pub async fn download_pre_chunk(&self, chunk_id: u32, dest: &Path) -> Result<String> {
        let resp = self.get(&format!("/api/pre/asset/{chunk_id}")).await?;
        save(resp, dest).await
    }

    /// 사전 청크에 아직 없는 곡의 사전 파일(배너·프리뷰 등) 하나를 압축 푼 내용으로 받는다.
    pub async fn pre_file(&self, song_id: u32, path: &str) -> Result<Bytes> {
        let mut url = Url::parse(&self.url(&format!("/api/pre/song/{song_id}/file")))
            .map_err(|e| Error::Config(format!("bad server url {}: {e}", self.base)))?;
        url.query_pairs_mut().append_pair("path", path);
        let resp = self
            .request(Method::GET, url)
            .timeout(PRE_FILE_TIMEOUT)
            .send()
            .await?;
        Ok(self.check(resp, StatusCode::OK)?.bytes().await?)
    }

    // ---- 플레이 다운로드 ----

    /// 곡 zip 전체를 `dest`에 저장하고 sha256을 돌려준다. 서버에서 티켓 1개가 빠진다.
    ///
    /// 전송이 중간에 끊기면 `Range` + `If-Range: <ETag>`로 받은 데까지 이어받는다.
    /// 서버는 grant 시간 안의 이어받기(206)에는 티켓을 더 쓰지 않는다.
    pub async fn play_song(&self, song_id: u32, dest: &Path) -> Result<String> {
        let path = format!("/api/play/song/{song_id}");
        let url = self.url(&path);
        let resp = self.get(&path).await?;
        // ETag가 없는 서버면 이어받지 않는다.
        let etag = resp.headers().get(header::ETAG).cloned();
        let mut file = tokio::fs::File::create(dest).await?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut next: Result<reqwest::Response> = Ok(resp);
        let mut attempt = 0;
        loop {
            let err = match next {
                Ok(r) => match copy_body(r, &mut file, &mut hasher, &mut written).await {
                    Ok(()) => break,
                    Err(e) => e,
                },
                Err(e) => e,
            };
            // 네트워크 오류만 이어받는다. 상태 코드·디스크 오류는 그대로 돌려준다.
            let Some(etag) = etag.as_ref().filter(|_| matches!(err, Error::Http(_))) else {
                return Err(err);
            };
            if attempt >= MAX_RESUME {
                return Err(err);
            }
            attempt += 1;
            warn!(song_id, written, attempt, %err, "play download interrupted, resuming");
            tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
            let r = match self
                .request(Method::GET, &url)
                .header(header::RANGE, format!("bytes={written}-"))
                .header(header::IF_RANGE, etag.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    next = Err(e.into());
                    continue;
                }
            };
            if r.status() == StatusCode::PARTIAL_CONTENT {
                if content_range_start(&r) != Some(written) {
                    return Err(Error::Other(format!(
                        "song {song_id}: unexpected Content-Range on resume"
                    )));
                }
                next = Ok(r);
            } else {
                // 200이면 서버가 처음부터 다시 보냈다(If-Range 불일치 등). 받은 것을 버린다.
                let r = self.check(r, StatusCode::OK)?;
                file.set_len(0).await?;
                file.seek(std::io::SeekFrom::Start(0)).await?;
                hasher = Sha256::new();
                written = 0;
                next = Ok(r);
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok(hex::encode(hasher.finalize()))
    }
}

/// 응답 본문을 `file` 끝에 덧붙이고 해시·받은 크기를 갱신한다.
async fn copy_body(
    resp: reqwest::Response,
    file: &mut tokio::fs::File,
    hasher: &mut Sha256,
    written: &mut u64,
) -> Result<()> {
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        *written += chunk.len() as u64;
    }
    Ok(())
}

/// `Content-Range: bytes <start>-<end>/<total>`의 시작 위치.
fn content_range_start(resp: &reqwest::Response) -> Option<u64> {
    let v = resp.headers().get(header::CONTENT_RANGE)?.to_str().ok()?;
    let range = v.trim().strip_prefix("bytes ")?;
    range.split('-').next()?.trim().parse().ok()
}

async fn save(resp: reqwest::Response, dest: &Path) -> Result<String> {
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut written = 0;
    copy_body(resp, &mut file, &mut hasher, &mut written).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(hex::encode(hasher.finalize()))
}
