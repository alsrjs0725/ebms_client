use serde::{Deserialize, Serialize};

/// `GET /api/pre/manifest/{chunk_id}`의 곡 하나.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SongManifest {
    pub song_id: u32,
    pub folder: String,
    pub zip_size: u64,
    pub zip_sha256: String,
    pub charts: Vec<String>,
    pub files: Vec<FileEntry>,
}

/// 곡 zip 안의 파일 하나.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    /// local file header 위치
    pub offset: u64,
    pub comp_size: u64,
    /// hex 문자열 (예: "1a2b3c4d")
    pub crc32: String,
    /// 0 무압축, 8 Deflate
    pub method: u16,
    /// 사전 파일인지 플레이 파일인지. 서버가 곡 등록 때 정한다.
    #[serde(default)]
    pub kind: FileKind,
}

/// 파일을 어느 다운로드 API로 받는지. 서버가 알려주지 않으면 티켓을 쓰지 않는 `Pre`로 본다
/// (서버는 플레이 파일의 사전 다운로드를 거절하므로 잘못 골라도 티켓이 빠지지 않는다).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// 배너·스테이지파일·프리뷰 등. 사전 API로 그 파일만 받는다.
    #[default]
    Pre,
    /// 키음·BGA 등. 플레이 API로 곡 zip 전체를 받는다(티켓 1개).
    Play,
}

impl FileEntry {
    pub fn crc32_value(&self) -> Option<u32> {
        u32::from_str_radix(&self.crc32, 16).ok()
    }
}
