//! EBMS 클라이언트 코어. UI·OS와 무관한 로직을 모두 담는다.
//!
//! - [`api`]: 서버 HTTP API (세션키는 `Authorization: Bearer`)
//! - [`auth`]: 브라우저 로그인 (루프백 리다이렉트 + PKCE)
//! - [`config`]: 서버 목록과 앱 데이터 폴더. 한 사용자가 여러 서버에 로그인한다
//! - [`session`]: 서버별 세션키 저장 (OS 키체인, 안 되면 파일)
//! - [`sync`]: 차트 청크·매니페스트 동기화
//! - [`index`]: 로컬 SQLite 인덱스
//! - [`tree`]: 가상 드라이브에 보여줄 곡/파일 트리
//! - [`fetch`]: 요청 시 다운로드 (사전 파일은 파일 하나, 플레이 파일은 곡 zip 전체)
//! - [`cache`]: 받은 에셋 캐시
//! - [`fs`]: OS 가상 FS 백엔드가 호출하는 읽기 전용 파일시스템
//! - [`drive`]: 여러 서버를 서버별 최상위 폴더로 합친 가상 드라이브
//! - [`hub`]: 등록된 서버 전체 (추가·삭제, 로그인, 동기화). 상주 앱이 쓴다

pub mod api;
pub mod auth;
pub mod cache;
pub mod client;
pub mod config;
pub mod drive;
pub mod error;
pub mod fetch;
pub mod fs;
pub mod hub;
pub mod index;
pub mod manifest;
pub mod paths;
pub mod session;
pub mod sync;
pub mod tree;

pub use client::{Client, Options};
pub use error::{Error, Result};

/// 서버가 차트로 취급하는 확장자 (소문자).
pub const CHART_EXTS: &[&str] = &["bms", "bme", "bml", "pms"];

/// 지원하는 서버 API 버전. 1은 로그인이 들어가고 기존 다운로드 API가 아직 남아 있는 서버.
pub const API_VERSIONS: std::ops::RangeInclusive<u32> = 1..=2;

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
