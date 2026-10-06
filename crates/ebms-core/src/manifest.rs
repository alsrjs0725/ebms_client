use serde::{Deserialize, Serialize};

/// `GET /api/manifest/{chunk_id}`의 곡 하나.
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
}

impl FileEntry {
    pub fn crc32_value(&self) -> Option<u32> {
        u32::from_str_radix(&self.crc32, 16).ok()
    }
}
