use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 다른 프로세스의 임시 파일·고아 캐시 파일을 이만큼 지나야 버려진 것으로 보고 지운다.
/// 쓰는 중인 파일은 수정 시각이 계속 바뀐다.
pub const STALE_AGE: Duration = Duration::from_secs(60 * 60);

/// 로컬 데이터 폴더 구조.
///
/// ```text
/// <root>/index.sqlite
/// <root>/charts/chart_chunk_00000.<sha256 앞 16자>.zip
/// <root>/pre/pre_chunk_00000.<sha256 앞 16자>.zip
/// <root>/cache/<song_id>/<path>
/// <root>/tmp/
/// <root>/no_ticket_until
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
        for dir in [self.charts(), self.pre(), self.cache(), self.tmp()] {
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

    /// 차트 청크. 이름에 해시를 넣어, 교체 중에도 옛 트리는 옛 파일을, 새 인덱스는 새 파일을 읽게 한다.
    pub fn chart_chunk(&self, id: u32, sha256: &str) -> PathBuf {
        self.charts().join(chunk_name("chart_chunk", id, sha256))
    }

    /// 해시 없는 이전 이름. 옮기기용.
    pub fn legacy_chart_chunk(&self, id: u32) -> PathBuf {
        self.charts().join(format!("chart_chunk_{id:05}.zip"))
    }

    /// 사전 청크(곡들의 배너·프리뷰 등). 캐시와 달리 지우지 않는다.
    pub fn pre(&self) -> PathBuf {
        self.root.join("pre")
    }

    pub fn pre_chunk(&self, id: u32, sha256: &str) -> PathBuf {
        self.pre().join(chunk_name("pre_chunk", id, sha256))
    }

    pub fn legacy_pre_chunk(&self, id: u32) -> PathBuf {
        self.pre().join(format!("pre_chunk_{id:05}.zip"))
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// 같은 폴더 안 임시 파일. 완성 후 [`TmpFile::persist`]로 옮기고, 실패하면 drop될 때 지운다.
    pub fn tmp_file(&self, name: &str) -> TmpFile {
        TmpFile::new(
            self.tmp()
                .join(format!("{name}.{}.part", std::process::id())),
        )
    }

    /// 이전 실행이 남긴 임시 파일을 지운다. 이 프로세스의 파일과 아직 쓰는 중일 수 있는
    /// 최근 파일([`STALE_AGE`] 이내)은 남긴다. 지운 개수.
    pub fn clean_tmp(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(self.tmp()) else {
            return 0;
        };
        let own = format!(".{}.", std::process::id());
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // 임시 파일 이름은 `<이름>.<pid>.part` 또는 `cache.<pid>.<n>.part`
            let own_file =
                name.starts_with(&format!("cache{own}")) || name.ends_with(&format!("{own}part"));
            if own_file || !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            if is_stale(&entry.path()) && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// 티켓이 없어 플레이 다운로드를 멈춘 경우 다시 시도할 시각(unix 초).
    pub fn no_ticket(&self) -> PathBuf {
        self.root.join("no_ticket_until")
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

fn chunk_name(prefix: &str, id: u32, sha256: &str) -> String {
    let short = sha256.get(..16).unwrap_or(sha256);
    format!("{prefix}_{id:05}.{short}.zip")
}

/// 수정한 지 [`STALE_AGE`]가 지났는지. 시각을 읽지 못하면 false.
pub fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= STALE_AGE)
}

/// 완성하지 못하면 drop될 때 지워지는 임시 파일.
#[derive(Debug)]
pub struct TmpFile {
    path: PathBuf,
    done: bool,
}

impl TmpFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path, done: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `dest`로 옮긴다. 실패하면 임시 파일은 drop될 때 지워진다.
    pub fn persist(mut self, dest: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, dest)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn tmp_file_is_removed_unless_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        paths.create_dirs().unwrap();

        let tmp = paths.tmp_file("a");
        std::fs::write(tmp.path(), b"x").unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        assert!(!path.exists());

        let tmp = paths.tmp_file("b");
        std::fs::write(tmp.path(), b"x").unwrap();
        let dest = dir.path().join("b");
        tmp.persist(&dest).unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn clean_tmp_removes_only_stale_files_of_other_processes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        paths.create_dirs().unwrap();
        let pid = std::process::id();
        let other = pid.wrapping_add(1);
        let files = [
            (format!("song_1.{other}.part"), 2 * 3600, false),
            (format!("cache.{other}.3.part"), 2 * 3600, false),
            (format!("song_2.{other}.part"), 10, true),
            (format!("song_3.{pid}.part"), 2 * 3600, true),
            (format!("cache.{pid}.0.part"), 2 * 3600, true),
        ];
        for (name, secs, _) in &files {
            let path = paths.tmp().join(name);
            std::fs::write(&path, b"x").unwrap();
            age(&path, *secs);
        }
        assert_eq!(paths.clean_tmp(), 2);
        for (name, _, kept) in &files {
            assert_eq!(paths.tmp().join(name).exists(), *kept, "{name}");
        }
    }
}
