#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server returned {status} for {url}")]
    Status { status: u16, url: String },
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// 세션키가 없거나 만료됨. 다시 로그인해야 한다.
    #[error("login required")]
    Unauthorized,
    /// 플레이 다운로드 티켓이 없음. `retry_after`초 뒤에 다시 시도한다.
    #[error("no download ticket, retry after {retry_after}s")]
    NoTicket { retry_after: u64 },
    #[error("login failed: {0}")]
    Auth(String),
    #[error("config: {0}")]
    Config(String),
    #[error("unsupported server api version {0}")]
    ApiVersion(u32),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
