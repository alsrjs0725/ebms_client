//! 가상 드라이브에 보여줄 트리. 인덱스에서 만들고 메모리에 둔다.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::index::ChartRow;
use crate::manifest::{FileEntry, SongManifest};

pub type Ino = u64;
pub const ROOT: Ino = 1;

/// 파일 내용을 어디서 읽는지.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// 로컬 차트 청크(무압축) 안의 바이트.
    Chart { chunk_id: u32, data_offset: u64 },
    /// 곡 zip 안의 파일. 처음 읽을 때 받는다.
    Asset { song_id: u32, entry: Arc<FileEntry> },
}

#[derive(Clone, Debug)]
pub enum Node {
    Dir {
        name: String,
        parent: Ino,
        children: BTreeMap<String, Ino>,
    },
    File {
        name: String,
        parent: Ino,
        size: u64,
        source: Source,
    },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Dir { name, .. } | Node::File { name, .. } => name,
        }
    }

    pub fn parent(&self) -> Ino {
        match self {
            Node::Dir { parent, .. } | Node::File { parent, .. } => *parent,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Dir { .. })
    }

    pub fn size(&self) -> u64 {
        match self {
            Node::Dir { .. } => 0,
            Node::File { size, .. } => *size,
        }
    }
}

/// 곡 zip 메타데이터 (다운로드 검증용).
#[derive(Clone, Debug)]
pub struct SongInfo {
    pub song_id: u32,
    pub zip_size: u64,
    pub zip_sha256: String,
    pub files: Vec<Arc<FileEntry>>,
}

#[derive(Debug, Default)]
pub struct Tree {
    nodes: HashMap<Ino, Node>,
    /// 경로 → ino. 재구성해도 같은 경로는 같은 ino를 유지한다.
    inos: HashMap<String, Ino>,
    next_ino: Ino,
    songs: HashMap<u32, Arc<SongInfo>>,
    /// (부모, 대소문자 접은 이름) → ino. 대소문자만 다른 이름이 겹치지 않게 한다.
    folded: HashMap<(Ino, String), Ino>,
}

impl Tree {
    /// `prev`가 있으면 같은 경로의 ino를 재사용한다 (마운트 중 재구성 대비).
    pub fn build(
        songs: &[SongManifest],
        charts: &HashMap<String, ChartRow>,
        prev: Option<&Tree>,
    ) -> Self {
        let mut tree = Tree {
            nodes: HashMap::new(),
            inos: prev.map(|p| p.inos.clone()).unwrap_or_default(),
            next_ino: prev.map(|p| p.next_ino).unwrap_or(ROOT + 1),
            songs: HashMap::new(),
            folded: HashMap::new(),
        };
        tree.inos.insert(String::new(), ROOT);
        tree.nodes.insert(
            ROOT,
            Node::Dir {
                name: String::new(),
                parent: ROOT,
                children: BTreeMap::new(),
            },
        );

        for song in songs {
            tree.add_song(song, charts);
        }
        // 지금 트리에 없는 경로는 버린다.
        let live: std::collections::HashSet<Ino> = tree.nodes.keys().copied().collect();
        tree.inos.retain(|_, ino| live.contains(ino));
        tree
    }

    fn add_song(&mut self, song: &SongManifest, charts: &HashMap<String, ChartRow>) {
        let files: Vec<Arc<FileEntry>> = song.files.iter().cloned().map(Arc::new).collect();
        self.songs.insert(
            song.song_id,
            Arc::new(SongInfo {
                song_id: song.song_id,
                zip_size: song.zip_size,
                zip_sha256: song.zip_sha256.clone(),
                files: files.clone(),
            }),
        );

        let dir_name = song_dir_name(song.song_id, &song.folder);
        let song_dir = self.dir(ROOT, &dir_name);

        // 이 곡의 로컬 차트를 (size, crc32)로 찾는다. 서버 #10 반영 전 임시 매핑.
        let mut unmatched: Vec<&ChartRow> = song
            .charts
            .iter()
            .filter_map(|sha| charts.get(sha))
            .collect();

        for entry in &files {
            let mut parent = song_dir;
            let parts: Vec<&str> = entry.path.split('/').filter(|p| !p.is_empty()).collect();
            let Some((file_name, dirs)) = parts.split_last() else {
                continue;
            };
            if parts.iter().any(|p| *p == "." || *p == "..") {
                continue;
            }
            for d in dirs {
                parent = self.dir(parent, &entry_name(d));
            }

            let mut source = Source::Asset {
                song_id: song.song_id,
                entry: entry.clone(),
            };
            if crate::is_chart_path(&entry.path) {
                let crc = entry.crc32_value();
                if let Some(pos) = unmatched
                    .iter()
                    .position(|c| Some(c.crc32) == crc && c.size == entry.size)
                {
                    let c = unmatched.swap_remove(pos);
                    source = Source::Chart {
                        chunk_id: c.chunk_id,
                        data_offset: c.data_offset,
                    };
                }
            }
            self.file(parent, &entry_name(file_name), entry.size, source);
        }

        // 곡 zip에 없는 차트(나중에 연결된 차트)는 sha256 이름으로 보여준다.
        for c in unmatched {
            let name = entry_name(&format!("{}.{}", c.sha256, c.ext));
            self.file(
                song_dir,
                &name,
                c.size,
                Source::Chart {
                    chunk_id: c.chunk_id,
                    data_offset: c.data_offset,
                },
            );
        }
    }

    fn path_of(&self, parent: Ino, name: &str) -> String {
        let parent_path = self.path(parent).unwrap_or_default();
        if parent_path.is_empty() {
            name.to_string()
        } else {
            format!("{parent_path}/{name}")
        }
    }

    fn ino_for(&mut self, path: String) -> Ino {
        if let Some(&ino) = self.inos.get(&path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inos.insert(path, ino);
        ino
    }

    /// 대소문자만 다른 폴더는 하나로 합친다.
    fn dir(&mut self, parent: Ino, name: &str) -> Ino {
        if let Some(&ino) = self.folded.get(&(parent, fold(name)))
            && self.nodes.get(&ino).is_some_and(Node::is_dir)
        {
            return ino;
        }
        let name = &self.free_name(parent, name);
        let ino = self.ino_for(self.path_of(parent, name));
        self.nodes.insert(
            ino,
            Node::Dir {
                name: name.to_string(),
                parent,
                children: BTreeMap::new(),
            },
        );
        self.link(parent, name, ino);
        ino
    }

    /// 이름이 겹치면 `a (2).wav`처럼 번호를 붙인다.
    fn file(&mut self, parent: Ino, name: &str, size: u64, source: Source) {
        let name = &self.free_name(parent, name);
        let ino = self.ino_for(self.path_of(parent, name));
        self.nodes.insert(
            ino,
            Node::File {
                name: name.to_string(),
                parent,
                size,
                source,
            },
        );
        self.link(parent, name, ino);
    }

    fn link(&mut self, parent: Ino, name: &str, ino: Ino) {
        if let Some(Node::Dir { children, .. }) = self.nodes.get_mut(&parent) {
            children.insert(name.to_string(), ino);
            self.folded.insert((parent, fold(name)), ino);
        }
    }

    /// `parent` 안에서 대소문자를 무시해도 겹치지 않는 이름.
    fn free_name(&self, parent: Ino, name: &str) -> String {
        let taken = |n: &str| self.folded.contains_key(&(parent, fold(n)));
        if !taken(name) {
            return name.to_string();
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => name.split_at(i),
            _ => (name, ""),
        };
        (2..)
            .map(|n| format!("{stem} ({n}){ext}"))
            .find(|n| !taken(n))
            .expect("unbounded")
    }

    pub fn get(&self, ino: Ino) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    /// 이름으로 찾는다. 정확히 맞는 이름이 없으면 대소문자를 무시하고 찾는다
    /// (BMS는 `BGM01.WAV`로 쓰고 실제 파일은 `bgm01.wav`인 경우가 흔하다).
    pub fn lookup(&self, parent: Ino, name: &str) -> Option<Ino> {
        let Some(Node::Dir { children, .. }) = self.nodes.get(&parent) else {
            return None;
        };
        if let Some(&ino) = children.get(name) {
            return Some(ino);
        }
        children
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, &ino)| ino)
    }

    pub fn children(&self, ino: Ino) -> Option<impl Iterator<Item = (&str, Ino)>> {
        match self.nodes.get(&ino)? {
            Node::Dir { children, .. } => Some(children.iter().map(|(n, &i)| (n.as_str(), i))),
            Node::File { .. } => None,
        }
    }

    /// `/`로 구분한 경로로 찾는다. 빈 문자열은 루트.
    pub fn resolve(&self, path: &str) -> Option<Ino> {
        let mut ino = ROOT;
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty()) {
            ino = self.lookup(ino, part)?;
        }
        Some(ino)
    }

    pub fn path(&self, ino: Ino) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = ino;
        while cur != ROOT {
            let node = self.nodes.get(&cur)?;
            parts.push(node.name().to_string());
            cur = node.parent();
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    pub fn song(&self, song_id: u32) -> Option<&Arc<SongInfo>> {
        self.songs.get(&song_id)
    }

    pub fn song_count(&self) -> usize {
        self.songs.len()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// zip 안 폴더·파일 이름을 드라이브에 보일 이름으로.
fn entry_name(name: &str) -> String {
    let clean = sanitize(name);
    if clean.is_empty() { "_".into() } else { clean }
}

/// `{song_id:05} {folder}`. OS에서 쓸 수 없는 문자는 `_`로 바꾼다.
pub fn song_dir_name(song_id: u32, folder: &str) -> String {
    let clean = sanitize(folder);
    if clean.is_empty() || clean == song_id.to_string() {
        format!("{song_id:05}")
    } else {
        format!("{song_id:05} {clean}")
    }
}

/// 이름 한 개를 Windows에서 쓸 수 있게 바꾼다. 서버 문자열은 믿지 않는다.
/// 금지 문자·제어 문자는 `_`, 끝의 점·공백은 제거, 예약어(`CON` 등)는 앞에 `_`,
/// 길이는 확장자를 남기고 200자까지.
pub(crate) fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = truncate_keep_ext(s.trim_start(), MAX_NAME_CHARS);
    // Windows는 끝의 점·공백을 허용하지 않는다.
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if is_reserved(&s) {
        s.insert(0, '_');
    }
    s
}

const MAX_NAME_CHARS: usize = 200;

fn truncate_keep_ext(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let ext = match s.rfind('.') {
        Some(i) if i > 0 && s[i..].chars().count() <= 16 => &s[i..],
        _ => "",
    };
    let stem: String = s.chars().take(max - ext.chars().count()).collect();
    stem + ext
}

/// `CON`, `nul.txt`, `COM1 .wav`처럼 Windows가 장치로 해석하는 이름.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|n| {
                (n.len() == 1 && n.as_bytes()[0].is_ascii_digit()) || matches!(n, "¹" | "²" | "³")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, crc: u32) -> FileEntry {
        FileEntry {
            path: path.into(),
            size,
            offset: 0,
            comp_size: size,
            crc32: format!("{crc:08x}"),
            method: 0,
            kind: Default::default(),
        }
    }

    fn chart(sha: &str, size: u64, crc: u32) -> ChartRow {
        ChartRow {
            sha256: sha.into(),
            ext: "bme".into(),
            size,
            crc32: crc,
            chunk_id: 0,
            data_offset: 100,
        }
    }

    fn sample() -> (Vec<SongManifest>, HashMap<String, ChartRow>) {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let songs = vec![SongManifest {
            song_id: 7,
            folder: "Artist - Title?".into(),
            zip_size: 1000,
            zip_sha256: "z".repeat(64),
            charts: vec![a.clone(), b.clone()],
            files: vec![
                entry("_7a.bme", 10, 1),
                entry("bgm01.ogg", 20, 2),
                entry("bga/movie.mp4", 30, 3),
            ],
        }];
        let charts = HashMap::from([(a.clone(), chart(&a, 10, 1)), (b.clone(), chart(&b, 11, 9))]);
        (songs, charts)
    }

    #[test]
    fn builds_folders_and_maps_charts() {
        let (songs, charts) = sample();
        let tree = Tree::build(&songs, &charts, None);

        let song = tree.resolve("00007 Artist - Title_").expect("song dir");
        assert!(tree.get(song).unwrap().is_dir());

        let bme = tree.resolve("00007 Artist - Title_/_7a.bme").unwrap();
        assert!(matches!(
            tree.get(bme).unwrap(),
            Node::File {
                source: Source::Chart { .. },
                ..
            }
        ));

        let ogg = tree
            .resolve("00007 Artist - Title_/BGM01.OGG")
            .expect("case-insensitive lookup");
        assert!(matches!(
            tree.get(ogg).unwrap(),
            Node::File {
                source: Source::Asset { .. },
                size: 20,
                ..
            }
        ));

        assert!(
            tree.resolve("00007 Artist - Title_/bga/movie.mp4")
                .is_some()
        );
        // zip에 없는 차트는 sha256 이름으로
        assert!(
            tree.resolve(&format!("00007 Artist - Title_/{}.bme", "b".repeat(64)))
                .is_some()
        );
    }

    #[test]
    fn keeps_inodes_across_rebuilds() {
        let (songs, charts) = sample();
        let first = Tree::build(&songs, &charts, None);
        let second = Tree::build(&songs, &charts, Some(&first));
        let p = "00007 Artist - Title_/bgm01.ogg";
        assert_eq!(first.resolve(p), second.resolve(p));
    }

    #[test]
    fn dir_names() {
        assert_eq!(song_dir_name(3, "3"), "00003");
        assert_eq!(song_dir_name(3, "a:b. "), "00003 a_b");
    }

    #[test]
    fn sanitizes_windows_names() {
        assert_eq!(sanitize("a.wav:x"), "a.wav_x");
        assert_eq!(sanitize("x. "), "x");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("com1 .wav"), "_com1 .wav");
        assert_eq!(sanitize("LPT²"), "_LPT²");
        assert_eq!(sanitize("console.wav"), "console.wav");
        assert_eq!(sanitize("COM10"), "COM10");
        assert_eq!(sanitize("가나다"), "가나다");
        let long = format!("{}.wav", "가".repeat(300));
        let cut = sanitize(&long);
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
        assert!(cut.ends_with(".wav"));
    }

    #[test]
    fn hostile_entry_names() {
        let a = "a".repeat(64);
        let songs = vec![SongManifest {
            song_id: 1,
            folder: "s".into(),
            zip_size: 0,
            zip_sha256: String::new(),
            charts: vec![a.clone()],
            files: vec![
                entry("CON", 1, 1),
                entry("x.wav:evil", 2, 2),
                entry("A.wav", 3, 3),
                entry("a.wav", 4, 4),
                entry("x.", 5, 5),
                entry("x", 6, 6),
                entry("Sub/1.ogg", 7, 7),
                entry("sub/2.ogg", 8, 8),
            ],
        }];
        let tree = Tree::build(&songs, &HashMap::new(), None);
        let song = tree.resolve("00001 s").unwrap();
        let mut names: Vec<&str> = tree.children(song).unwrap().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "A.wav",
                "Sub",
                "_CON",
                "a (2).wav",
                "x",
                "x (2)",
                "x.wav_evil"
            ]
        );
        // 대소문자만 다른 폴더는 합쳐진다.
        let sub = tree.resolve("00001 s/Sub").unwrap();
        assert_eq!(tree.children(sub).unwrap().count(), 2);
    }
}
