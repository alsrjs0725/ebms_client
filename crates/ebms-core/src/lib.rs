//! EBMS 클라이언트 코어. UI·OS와 무관한 로직을 모두 담는다.
//!
//! - [`api`]: 서버 HTTP API
//! - [`sync`]: 차트 청크·매니페스트 동기화
//! - [`index`]: 로컬 SQLite 인덱스
//! - [`tree`]: 가상 드라이브에 보여줄 곡/파일 트리
//! - [`fetch`]: 요청 시 다운로드 (파일 단위 Range, 곡 단위 승격)
//! - [`cache`]: 받은 에셋 캐시
//! - [`fs`]: OS 가상 FS 백엔드가 호출하는 읽기 전용 파일시스템

pub mod api;
pub mod cache;
pub mod client;
pub mod error;
pub mod fetch;
pub mod fs;
pub mod index;
pub mod manifest;
pub mod paths;
pub mod sync;
pub mod tree;

pub use client::{Client, Options};
pub use error::{Error, Result};

/// 서버가 차트로 취급하는 확장자 (소문자).
pub const CHART_EXTS: &[&str] = &["bms", "bme", "bml", "pms"];

/// 지원하는 서버 API 버전.
pub const API_VERSION: u32 = 1;

pub(crate) fn is_chart_path(path: &str) -> bool {
    extension(path).is_some_and(|e| CHART_EXTS.contains(&e.as_str()))
}

pub(crate) fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}
