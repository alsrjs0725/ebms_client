use std::path::{Path, PathBuf};

/// 로컬 데이터 폴더 구조.
///
/// ```text
/// <root>/index.sqlite
/// <root>/charts/chart_chunk_00000.zip
/// <root>/cache/<song_id>/<path>
/// <root>/tmp/
/// ```
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn create_dirs(&self) -> std::io::Result<()> {
        for dir in [self.charts(), self.cache(), self.tmp()] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn index(&self) -> PathBuf {
        self.root.join("index.sqlite")
    }

    pub fn charts(&self) -> PathBuf {
        self.root.join("charts")
    }

    pub fn chart_chunk(&self, id: u32) -> PathBuf {
        self.charts().join(format!("chart_chunk_{id:05}.zip"))
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// 같은 폴더 안 임시 파일. 완성 후 rename으로 옮긴다.
    pub fn tmp_file(&self, name: &str) -> PathBuf {
        self.tmp()
            .join(format!("{name}.{}.part", std::process::id()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 캐시 폴더를 뺀 로컬 데이터 크기 (인덱스, 차트 등). 캐시 크기는 인덱스에서 바로 읽는다.
    pub fn size_without_cache(&self) -> u64 {
        let cache = self.cache();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|e| e.path() != cache)
            .map(|e| dir_size(&e.path()))
            .sum()
    }
}

/// 파일이나 폴더의 크기 합. 읽지 못한 항목은 0으로 친다.
pub fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| dir_size(&e.path())).sum())
        .unwrap_or(0)
}
