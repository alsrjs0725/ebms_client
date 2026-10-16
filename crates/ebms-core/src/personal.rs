//! 개인 채보. 사용자가 가진 채보 파일을 서버 곡에 연결해 가상 드라이브의 그 곡 폴더에 보여준다.
//!
//! 서버에 올리지 않는다. 연결할 때 파일을 서버 데이터 폴더의 `personal/<sha256>.<ext>`로 복사하고
//! 인덱스에 `(곡, 원래 파일 이름)`을 남긴다. 원래 위치(전체 경로)는 남기지 않는다.
//! 복사본을 쓰므로 원본을 옮기거나 지워도 그대로 보이고, 고친 채보는 같은 이름으로 다시 연결하면 바뀐다.

use std::path::Path;

use serde::Serialize;

use crate::index::Index;
use crate::paths::Paths;
use crate::{Error, Result};

/// 개인 채보로 받는 확장자 (소문자). 서버 차트 확장자와 같다.
pub const PERSONAL_EXTS: &[&str] = crate::CHART_EXTS;
/// 채보 하나의 최대 크기. 채보가 아닌 큰 파일을 잘못 고른 경우를 막는다.
pub const MAX_SIZE: u64 = 16 << 20;

/// 곡에 연결한 개인 채보 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersonalChart {
    pub song_id: u32,
    /// 원래 파일 이름. 곡 폴더에 이 이름으로 보인다(겹치면 번호를 붙인다).
    pub name: String,
    pub sha256: String,
    /// 소문자 확장자
    pub ext: String,
    pub size: u64,
    /// 연결한 시각(unix 초)
    pub added_at: i64,
}

/// `file`을 복사해 `song_id` 곡에 연결한다. 같은 곡에 같은 이름이 있으면 바꾼다.
pub fn add(paths: &Paths, index: &Index, song_id: u32, file: &Path) -> Result<PersonalChart> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::Config(format!("not a file name: {}", file.display())))?
        .to_string();
    let ext = crate::extension(&name)
        .filter(|e| PERSONAL_EXTS.contains(&e.as_str()))
        .ok_or_else(|| {
            Error::Config(format!(
                "{name}: not a chart file ({})",
                PERSONAL_EXTS.join(", ")
            ))
        })?;
    if !index.has_song(song_id)? {
        return Err(Error::Config(format!("unknown song: {song_id}")));
    }
    let meta = std::fs::metadata(file)?;
    if !meta.is_file() {
        return Err(Error::Config(format!("{name}: not a file")));
    }
    if meta.len() > MAX_SIZE {
        return Err(Error::Config(format!(
            "{name}: too large for a chart ({} MB max)",
            MAX_SIZE >> 20
        )));
    }
    let data = std::fs::read(file)?;
    if data.len() as u64 > MAX_SIZE {
        return Err(Error::Config(format!("{name}: too large for a chart")));
    }
    let sha256 = crate::sha256_hex(&data);
    let dest = paths.personal_chart(&sha256, &ext);
    if !dest.is_file() {
        std::fs::create_dir_all(paths.personal())?;
        let tmp = paths.tmp_file(&format!("personal_{sha256}"));
        std::fs::write(tmp.path(), &data)?;
        tmp.persist(&dest)?;
    }
    let chart = PersonalChart {
        song_id,
        name,
        sha256,
        ext,
        size: data.len() as u64,
        added_at: crate::hub::unix_now(),
    };
    if let Some(old) = index.personal_put(&chart)? {
        remove_file_if_unused(paths, index, &old.sha256, &old.ext);
    }
    Ok(chart)
}

/// 연결을 끊는다. 다른 곡이 같은 채보를 쓰지 않으면 복사본도 지운다. 없던 연결이면 false.
pub fn remove(paths: &Paths, index: &Index, song_id: u32, name: &str) -> Result<bool> {
    let Some(old) = index.personal_remove(song_id, name)? else {
        return Ok(false);
    };
    remove_file_if_unused(paths, index, &old.sha256, &old.ext);
    Ok(true)
}

fn remove_file_if_unused(paths: &Paths, index: &Index, sha256: &str, ext: &str) {
    match index.personal_in_use(sha256, ext) {
        Ok(false) => {
            // 트리가 아직 옛 복사본을 가리킬 수 있지만 채보는 연결을 끊은 것이므로 읽기 실패로 충분하다.
            let _ = std::fs::remove_file(paths.personal_chart(sha256, ext));
        }
        Ok(true) => {}
        Err(e) => tracing::warn!(%e, "could not check personal chart use"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SongManifest;

    fn setup() -> (tempfile::TempDir, Paths, Index) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"));
        paths.create_dirs().unwrap();
        let index = Index::open(&paths.index()).unwrap();
        let song = SongManifest {
            song_id: 3,
            folder: "A - B".into(),
            zip_size: 0,
            zip_sha256: String::new(),
            charts: vec![],
            files: vec![],
        };
        index.replace_manifest_chunk(0, "m", &[song]).unwrap();
        (dir, paths, index)
    }

    #[test]
    fn copies_links_and_unlinks() {
        let (dir, paths, index) = setup();
        let src = dir.path().join("my [ANOTHER].bme");
        std::fs::write(&src, b"#TITLE x").unwrap();

        let c = add(&paths, &index, 3, &src).unwrap();
        assert_eq!(c.name, "my [ANOTHER].bme");
        assert_eq!(c.ext, "bme");
        assert_eq!(c.size, 8);
        // 원본을 지워도 복사본으로 읽힌다.
        std::fs::remove_file(&src).unwrap();
        let copy = paths.personal_chart(&c.sha256, "bme");
        assert_eq!(std::fs::read(&copy).unwrap(), b"#TITLE x");
        assert_eq!(index.personal_for_song(3).unwrap(), vec![c.clone()]);

        // 같은 이름으로 다시 연결하면 바뀌고 옛 복사본은 지운다.
        std::fs::write(&src, b"#TITLE y").unwrap();
        let c2 = add(&paths, &index, 3, &src).unwrap();
        assert_ne!(c2.sha256, c.sha256);
        assert!(!copy.exists());
        assert_eq!(index.personal_list().unwrap().len(), 1);

        assert!(remove(&paths, &index, 3, &c2.name).unwrap());
        assert!(!paths.personal_chart(&c2.sha256, "bme").exists());
        assert!(!remove(&paths, &index, 3, &c2.name).unwrap());
        assert!(index.personal_list().unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_files() {
        let (dir, paths, index) = setup();
        let wav = dir.path().join("a.wav");
        std::fs::write(&wav, b"x").unwrap();
        assert!(add(&paths, &index, 3, &wav).is_err());
        let bms = dir.path().join("a.bms");
        std::fs::write(&bms, b"x").unwrap();
        assert!(add(&paths, &index, 99, &bms).is_err(), "unknown song");
        let big = dir.path().join("big.bmson");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_SIZE + 1)
            .unwrap();
        assert!(add(&paths, &index, 3, &big).is_err());
        assert!(index.personal_list().unwrap().is_empty());
    }

    #[test]
    fn shared_copy_stays_while_used() {
        let (dir, paths, index) = setup();
        let song = SongManifest {
            song_id: 4,
            folder: "C".into(),
            zip_size: 0,
            zip_sha256: String::new(),
            charts: vec![],
            files: vec![],
        };
        index.replace_manifest_chunk(1, "m2", &[song]).unwrap();
        let src = dir.path().join("x.bms");
        std::fs::write(&src, b"same").unwrap();
        let a = add(&paths, &index, 3, &src).unwrap();
        add(&paths, &index, 4, &src).unwrap();
        remove(&paths, &index, 3, &a.name).unwrap();
        assert!(paths.personal_chart(&a.sha256, "bms").exists());
    }
}
