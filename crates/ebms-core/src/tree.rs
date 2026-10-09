//! 가상 드라이브에 보여줄 트리.
//!
//! 루트(곡 폴더 목록)만 메모리에 두고, 곡 안 파일 목록은 처음 열 때 인덱스에서 읽어
//! 최근에 쓴 [`SONG_CACHE`]곡만 둔다. ino는 `(song_id + 1) << NODE_BITS | 곡 안 노드 번호`로
//! 계산하므로 경로 → ino 맵이 없다. 곡 폴더는 노드 번호 0. 같은 매니페스트에서는 같은 번호가
//! 나오므로 재구성해도 같은 경로는 같은 ino를 유지한다.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::warn;

use crate::Result;
use crate::index::{ChartRow, Index};
use crate::manifest::{FileEntry, SongManifest};

pub type Ino = u64;
pub const ROOT: Ino = 1;

/// 곡 안 노드 번호에 쓰는 비트. 곡 하나에 노드 100만 개까지.
const NODE_BITS: u32 = 20;
const NODE_MASK: Ino = (1 << NODE_BITS) - 1;
/// [`crate::drive::Drive`]가 서버 안 ino에 48비트를 쓰므로 `song_id + 1`은 28비트까지.
const MAX_SONG_KEY: Ino = (1 << (48 - NODE_BITS)) - 1;
/// 파일 목록을 메모리에 두는 곡 수.
pub const SONG_CACHE: usize = 256;

/// 파일 내용을 어디서 읽는지.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// 로컬 차트 청크(무압축) 안의 바이트.
    Chart { chunk_id: u32, data_offset: u64 },
    /// 곡 zip 안의 파일. 처음 읽을 때 받는다.
    Asset { song_id: u32, entry: Arc<FileEntry> },
}

/// 곡 zip 메타데이터 (다운로드 검증용).
#[derive(Clone, Debug)]
pub struct SongInfo {
    pub song_id: u32,
    pub zip_size: u64,
    pub zip_sha256: String,
    pub files: Vec<Arc<FileEntry>>,
}

/// 노드 하나의 속성.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub ino: Ino,
    pub is_dir: bool,
    pub size: u64,
}

/// 곡 파일 목록을 읽어 오는 곳. 보통 [`Index`].
pub trait SongSource: Send + Sync {
    fn song(&self, song_id: u32) -> Result<Option<SongManifest>>;
    /// sha256으로 로컬 차트를 찾는다. 없는 것은 빠진다.
    fn charts(&self, sha256: &[String]) -> Result<Vec<ChartRow>>;
}

impl SongSource for Index {
    fn song(&self, song_id: u32) -> Result<Option<SongManifest>> {
        Index::song(self, song_id)
    }

    fn charts(&self, sha256: &[String]) -> Result<Vec<ChartRow>> {
        self.charts_by_sha(sha256)
    }
}

fn song_ino(song_id: u32) -> Option<Ino> {
    let key = song_id as Ino + 1;
    (key <= MAX_SONG_KEY).then_some(key << NODE_BITS)
}

/// ino → (song_id, 곡 안 노드 번호). 루트나 잘못된 ino면 None.
fn split(ino: Ino) -> Option<(u32, usize)> {
    let key = ino >> NODE_BITS;
    if key == 0 || key > MAX_SONG_KEY {
        return None;
    }
    Some(((key - 1) as u32, (ino & NODE_MASK) as usize))
}

/// 대소문자를 무시한 비교. 정렬과 이름 찾기에 같이 쓴다.
fn fold_cmp(a: &str, b: &str) -> Ordering {
    let lower = |c: u8| c.to_ascii_lowercase();
    a.bytes().map(lower).cmp(b.bytes().map(lower))
}

fn name_cmp(a: &str, b: &str) -> Ordering {
    fold_cmp(a, b).then_with(|| a.cmp(b))
}

/// [`name_cmp`] 순으로 정렬된 `len`개 이름(`key(i)`)에서 찾는다. 정확히 맞는 이름이 없으면 대소문자를 무시하고
/// 찾는다 (BMS는 `BGM01.WAV`로 쓰고 실제 파일은 `bgm01.wav`인 경우가 흔하다).
fn find_name<'k>(len: usize, name: &str, key: impl Fn(usize) -> &'k str) -> Option<usize> {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fold_cmp(key(mid), name) == Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut first = None;
    for i in lo..len {
        let k = key(i);
        if fold_cmp(k, name) != Ordering::Equal {
            break;
        }
        if k == name {
            return Some(i);
        }
        first.get_or_insert(i);
    }
    first
}

struct RootEntry {
    name: Box<str>,
    song_id: u32,
}

pub struct Tree {
    /// 곡 폴더. [`name_cmp`] 순.
    root: Vec<RootEntry>,
    /// song_id → `root` 위치
    by_song: HashMap<u32, usize>,
    source: Arc<dyn SongSource>,
    songs: Mutex<SongCache>,
}

impl Tree {
    /// 인덱스의 곡 목록으로 루트를 만든다. 곡 안 파일 목록은 처음 쓸 때 읽는다.
    pub fn open(index: Arc<Index>) -> Result<Self> {
        let folders = index.song_folders()?;
        Ok(Self::new(folders, index))
    }

    /// `folders`는 `(song_id, folder)`.
    pub fn new(folders: Vec<(u32, String)>, source: Arc<dyn SongSource>) -> Self {
        let mut root: Vec<RootEntry> = folders
            .into_iter()
            .filter(|(id, _)| {
                let ok = song_ino(*id).is_some();
                if !ok {
                    warn!(song_id = id, "song id too large, skipped");
                }
                ok
            })
            .map(|(song_id, folder)| RootEntry {
                name: song_dir_name(song_id, &folder).into(),
                song_id,
            })
            .collect();
        root.sort_by(|a, b| name_cmp(&a.name, &b.name));
        let by_song = root
            .iter()
            .enumerate()
            .map(|(i, e)| (e.song_id, i))
            .collect();
        Self {
            root,
            by_song,
            source,
            songs: Mutex::new(SongCache::default()),
        }
    }

    /// 곡 파일 목록. 메모리에 없으면 인덱스에서 읽는다.
    fn load(&self, song_id: u32) -> Option<Arc<SongTree>> {
        let &pos = self.by_song.get(&song_id)?;
        if let Some(t) = self.cache().get(song_id) {
            return Some(t);
        }
        let loaded = (|| {
            let Some(song) = self.source.song(song_id)? else {
                return Ok(None);
            };
            let charts = self.source.charts(&song.charts)?;
            Ok::<_, crate::Error>(Some(SongTree::build(&song, &self.root[pos].name, charts)))
        })();
        match loaded {
            Ok(Some(t)) => {
                let t = Arc::new(t);
                self.cache().insert(song_id, t.clone());
                Some(t)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(song_id, %e, "could not load song files");
                None
            }
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, SongCache> {
        self.songs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn node(&self, ino: Ino) -> Option<(Arc<SongTree>, usize)> {
        let (song_id, idx) = split(ino)?;
        let song = self.load(song_id)?;
        (idx < song.nodes.len()).then_some((song, idx))
    }

    pub fn get(&self, ino: Ino) -> Option<Stat> {
        if ino == ROOT {
            return Some(Stat {
                ino,
                is_dir: true,
                size: 0,
            });
        }
        let (song, idx) = self.node(ino)?;
        Some(song.stat(ino & !NODE_MASK, idx))
    }

    pub fn parent(&self, ino: Ino) -> Option<Ino> {
        if ino == ROOT {
            return Some(ROOT);
        }
        let (song, idx) = self.node(ino)?;
        Some(match idx {
            0 => ROOT,
            _ => (ino & !NODE_MASK) | song.nodes[idx].parent as Ino,
        })
    }

    pub fn lookup(&self, parent: Ino, name: &str) -> Option<Ino> {
        if parent == ROOT {
            let i = find_name(self.root.len(), name, |i| &self.root[i].name)?;
            return song_ino(self.root[i].song_id);
        }
        let (song, idx) = self.node(parent)?;
        let Kind::Dir(children) = &song.nodes[idx].kind else {
            return None;
        };
        let i = find_name(children.len(), name, |i| {
            &song.nodes[children[i] as usize].name
        })?;
        Some((parent & !NODE_MASK) | children[i] as Ino)
    }

    /// `offset`번째 항목부터 `add(ino, name, is_dir)`에 넘긴다. `add`가 true를 돌려주면 멈춘다.
    /// 폴더가 아니면 None.
    pub fn read_dir(
        &self,
        ino: Ino,
        offset: usize,
        mut add: impl FnMut(Ino, &str, bool) -> bool,
    ) -> Option<()> {
        if ino == ROOT {
            for e in self.root.iter().skip(offset) {
                if let Some(child) = song_ino(e.song_id)
                    && add(child, &e.name, true)
                {
                    break;
                }
            }
            return Some(());
        }
        let (song, idx) = self.node(ino)?;
        let Kind::Dir(children) = &song.nodes[idx].kind else {
            return None;
        };
        let base = ino & !NODE_MASK;
        for &c in children.iter().skip(offset) {
            let node = &song.nodes[c as usize];
            if add(base | c as Ino, &node.name, node.is_dir()) {
                break;
            }
        }
        Some(())
    }

    /// 파일이면 (곡, 크기, 읽을 곳).
    pub fn file(&self, ino: Ino) -> Option<(Arc<SongInfo>, u64, Source)> {
        let (song, idx) = self.node(ino)?;
        match &song.nodes[idx].kind {
            Kind::File { size, source } => Some((song.info.clone(), *size, source.clone())),
            Kind::Dir(_) => None,
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
        if ino == ROOT {
            return Some(String::new());
        }
        let (song, mut idx) = self.node(ino)?;
        let mut parts = Vec::new();
        loop {
            parts.push(song.nodes[idx].name.as_ref());
            if idx == 0 {
                break;
            }
            idx = song.nodes[idx].parent as usize;
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    pub fn song(&self, song_id: u32) -> Option<Arc<SongInfo>> {
        self.load(song_id).map(|t| t.info.clone())
    }

    pub fn song_count(&self) -> usize {
        self.root.len()
    }

    /// 파일 목록을 메모리에 둔 곡 수.
    pub fn loaded_songs(&self) -> usize {
        self.cache().map.len()
    }
}

/// 최근에 쓴 곡 [`SONG_CACHE`]개.
#[derive(Default)]
struct SongCache {
    map: HashMap<u32, (Arc<SongTree>, u64)>,
    tick: u64,
}

impl SongCache {
    fn get(&mut self, song_id: u32) -> Option<Arc<SongTree>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&song_id).map(|(t, used)| {
            *used = tick;
            t.clone()
        })
    }

    fn insert(&mut self, song_id: u32, tree: Arc<SongTree>) {
        if self.map.len() >= SONG_CACHE
            && !self.map.contains_key(&song_id)
            && let Some(&oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k)
        {
            self.map.remove(&oldest);
        }
        self.tick += 1;
        self.map.insert(song_id, (tree, self.tick));
    }
}

enum Kind {
    /// 자식 노드 번호. [`name_cmp`] 순.
    Dir(Vec<u32>),
    File {
        size: u64,
        source: Source,
    },
}

struct SongNode {
    name: Box<str>,
    parent: u32,
    kind: Kind,
}

impl SongNode {
    fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir(_))
    }
}

/// 곡 하나의 파일 목록. 노드 0이 곡 폴더.
struct SongTree {
    info: Arc<SongInfo>,
    nodes: Vec<SongNode>,
}

impl SongTree {
    fn build(song: &SongManifest, dir_name: &str, charts: Vec<ChartRow>) -> Self {
        let files: Vec<Arc<FileEntry>> = song.files.iter().cloned().map(Arc::new).collect();
        let mut b = Builder {
            nodes: vec![SongNode {
                name: dir_name.into(),
                parent: 0,
                kind: Kind::Dir(Vec::new()),
            }],
            names: HashMap::new(),
            song_id: song.song_id,
            full: false,
        };

        // 이 곡의 로컬 차트를 (size, crc32)로 찾는다. 서버 #10 반영 전 임시 매핑.
        let mut unmatched = charts;

        for entry in &files {
            let parts: Vec<&str> = entry.path.split('/').filter(|p| !p.is_empty()).collect();
            let Some((file_name, dirs)) = parts.split_last() else {
                continue;
            };
            if parts.iter().any(|p| *p == "." || *p == "..") {
                continue;
            }
            let mut parent = Some(0);
            for d in dirs {
                parent = parent.and_then(|p| b.dir(p, &entry_name(d)));
            }
            let Some(parent) = parent else { continue };

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
            b.file(parent, &entry_name(file_name), entry.size, source);
        }

        // 곡 zip에 없는 차트(나중에 연결된 차트)는 sha256 이름으로 보여준다.
        for c in unmatched {
            let name = entry_name(&format!("{}.{}", c.sha256, c.ext));
            b.file(
                0,
                &name,
                c.size,
                Source::Chart {
                    chunk_id: c.chunk_id,
                    data_offset: c.data_offset,
                },
            );
        }

        let mut nodes = b.nodes;
        // 자식을 이름순으로. 이름은 노드에 있으므로 잠시 꺼내 정렬한다.
        for i in 0..nodes.len() {
            if let Kind::Dir(children) = &mut nodes[i].kind {
                let mut children = std::mem::take(children);
                children
                    .sort_by(|&a, &b| name_cmp(&nodes[a as usize].name, &nodes[b as usize].name));
                nodes[i].kind = Kind::Dir(children);
            }
        }
        nodes.shrink_to_fit();

        Self {
            info: Arc::new(SongInfo {
                song_id: song.song_id,
                zip_size: song.zip_size,
                zip_sha256: song.zip_sha256.clone(),
                files,
            }),
            nodes,
        }
    }

    fn stat(&self, base: Ino, idx: usize) -> Stat {
        let (is_dir, size) = match &self.nodes[idx].kind {
            Kind::Dir(_) => (true, 0),
            Kind::File { size, .. } => (false, *size),
        };
        Stat {
            ino: base | idx as Ino,
            is_dir,
            size,
        }
    }
}

struct Builder {
    nodes: Vec<SongNode>,
    /// (부모, 대소문자 접은 이름) → 노드. 만드는 동안만 쓴다.
    names: HashMap<(u32, String), u32>,
    song_id: u32,
    full: bool,
}

impl Builder {
    /// 노드를 더한다. 노드 번호가 모자라면 None. `name`은 [`Builder::free_name`]으로 고른 이름.
    fn add(&mut self, parent: u32, name: String, kind: Kind) -> Option<u32> {
        if self.nodes.len() as Ino > NODE_MASK {
            if !self.full {
                warn!(
                    song_id = self.song_id,
                    "too many files in song, rest skipped"
                );
                self.full = true;
            }
            return None;
        }
        let idx = self.nodes.len() as u32;
        self.names.insert((parent, fold(&name)), idx);
        self.nodes.push(SongNode {
            name: name.into(),
            parent,
            kind,
        });
        if let Kind::Dir(children) = &mut self.nodes[parent as usize].kind {
            children.push(idx);
        }
        Some(idx)
    }

    /// 대소문자만 다른 폴더는 하나로 합친다.
    fn dir(&mut self, parent: u32, name: &str) -> Option<u32> {
        if let Some(&idx) = self.names.get(&(parent, fold(name)))
            && self.nodes[idx as usize].is_dir()
        {
            return Some(idx);
        }
        let name = self.free_name(parent, name);
        self.add(parent, name, Kind::Dir(Vec::new()))
    }

    /// 이름이 겹치면 `a (2).wav`처럼 번호를 붙인다.
    fn file(&mut self, parent: u32, name: &str, size: u64, source: Source) {
        let name = self.free_name(parent, name);
        self.add(parent, name, Kind::File { size, source });
    }

    /// `parent` 안에서 대소문자를 무시해도 겹치지 않는 이름.
    fn free_name(&self, parent: u32, name: &str) -> String {
        let taken = |n: &str| self.names.contains_key(&(parent, fold(n)));
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
/// 길이는 확장자를 남기고 UTF-16 200자까지 (Windows 이름 한도 255자 안).
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

/// Windows는 이름 길이를 UTF-16 단위로 센다. 이모지처럼 BMP 밖 글자는 2로 센다.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn truncate_keep_ext(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    let ext = match s.rfind('.') {
        Some(i) if i > 0 && s[i..].chars().count() <= 16 => &s[i..],
        _ => "",
    };
    let mut left = max - utf16_len(ext);
    let stem: String = s
        .chars()
        .take_while(|c| {
            let fits = c.len_utf16() <= left;
            if fits {
                left -= c.len_utf16();
            }
            fits
        })
        .collect();
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

    /// 메모리에 둔 인덱스. 곡을 몇 번 읽었는지 센다.
    #[derive(Default)]
    struct Mem {
        songs: Vec<SongManifest>,
        charts: HashMap<String, ChartRow>,
        loads: std::sync::atomic::AtomicUsize,
    }

    impl SongSource for Mem {
        fn song(&self, song_id: u32) -> Result<Option<SongManifest>> {
            self.loads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.songs.iter().find(|s| s.song_id == song_id).cloned())
        }

        fn charts(&self, sha256: &[String]) -> Result<Vec<ChartRow>> {
            Ok(sha256
                .iter()
                .filter_map(|s| self.charts.get(s).cloned())
                .collect())
        }
    }

    fn tree(mem: Mem) -> (Tree, Arc<Mem>) {
        let mem = Arc::new(mem);
        let folders = mem
            .songs
            .iter()
            .map(|s| (s.song_id, s.folder.clone()))
            .collect();
        (Tree::new(folders, mem.clone()), mem)
    }

    fn sample() -> Mem {
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
        Mem {
            songs,
            charts,
            ..Default::default()
        }
    }

    #[test]
    fn builds_folders_and_maps_charts() {
        let (tree, _) = tree(sample());

        let song = tree.resolve("00007 Artist - Title_").expect("song dir");
        assert!(tree.get(song).unwrap().is_dir);
        assert_eq!(tree.resolve("00007 artist - title_"), Some(song));

        let bme = tree.resolve("00007 Artist - Title_/_7a.bme").unwrap();
        assert!(matches!(
            tree.file(bme),
            Some((_, 10, Source::Chart { .. }))
        ));

        let ogg = tree
            .resolve("00007 Artist - Title_/BGM01.OGG")
            .expect("case-insensitive lookup");
        assert!(matches!(
            tree.file(ogg),
            Some((_, 20, Source::Asset { .. }))
        ));
        assert_eq!(tree.path(ogg).unwrap(), "00007 Artist - Title_/bgm01.ogg");

        let mp4 = tree.resolve("00007 Artist - Title_/bga/movie.mp4").unwrap();
        let bga = tree.parent(mp4).unwrap();
        assert_eq!(tree.parent(bga), Some(song));
        assert_eq!(tree.parent(song), Some(ROOT));
        // zip에 없는 차트는 sha256 이름으로
        assert!(
            tree.resolve(&format!("00007 Artist - Title_/{}.bme", "b".repeat(64)))
                .is_some()
        );
        assert!(tree.resolve("00007 Artist - Title_/missing").is_none());
        assert!(tree.get(song | 999).is_none());
    }

    #[test]
    fn read_dir_from_offset() {
        let (tree, _) = tree(sample());
        let song = tree.resolve("00007 Artist - Title_").unwrap();
        let mut all = Vec::new();
        tree.read_dir(song, 0, |ino, name, _| {
            all.push((ino, name.to_string()));
            false
        })
        .unwrap();
        let names: Vec<&str> = all.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(
            names[..3],
            ["_7a.bme", &format!("{}.bme", "b".repeat(64)), "bga"]
        );
        assert_eq!(names.len(), 4);

        let mut rest = Vec::new();
        tree.read_dir(song, 2, |ino, name, _| {
            rest.push((ino, name.to_string()));
            false
        })
        .unwrap();
        assert_eq!(rest, all[2..]);

        let file = tree.resolve("00007 Artist - Title_/_7a.bme").unwrap();
        assert!(tree.read_dir(file, 0, |_, _, _| false).is_none());
    }

    #[test]
    fn keeps_inodes_across_rebuilds() {
        let p = "00007 Artist - Title_/bgm01.ogg";
        let first = tree(sample()).0.resolve(p);
        let second = tree(sample()).0.resolve(p);
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[test]
    fn keeps_only_recent_songs_in_memory() {
        let songs = (0..SONG_CACHE as u32 + 10)
            .map(|id| SongManifest {
                song_id: id,
                folder: format!("s{id}"),
                zip_size: 0,
                zip_sha256: String::new(),
                charts: vec![],
                files: vec![entry("a.ogg", 1, 0)],
            })
            .collect();
        let (tree, mem) = tree(Mem {
            songs,
            ..Default::default()
        });
        let first = tree.resolve("00000 s0/a.ogg").unwrap();
        for id in 1..SONG_CACHE as u32 + 10 {
            assert!(tree.resolve(&format!("{id:05} s{id}/a.ogg")).is_some());
        }
        assert_eq!(tree.loaded_songs(), SONG_CACHE);
        // 내보낸 곡도 같은 ino로 다시 읽힌다.
        let before = mem.loads.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(tree.get(first).unwrap().size, 1);
        assert_eq!(
            mem.loads.load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );
    }

    #[test]
    fn inodes_fit_drive_slot() {
        assert_eq!(song_ino(0), Some(1 << NODE_BITS));
        assert!(song_ino(u32::MAX).is_none());
        let max = song_ino((MAX_SONG_KEY - 1) as u32).unwrap();
        assert!(max + NODE_MASK < 1 << 48);
        assert_eq!(split(max | 3), Some(((MAX_SONG_KEY - 1) as u32, 3)));
        assert_eq!(split(ROOT), None);
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
        // BMP 밖 글자(이모지)는 UTF-16 2자로 센다.
        let cut = sanitize(&format!("{}.ogg", "🎵".repeat(150)));
        assert_eq!(cut.encode_utf16().count(), MAX_NAME_CHARS);
        assert!(cut.ends_with(".ogg"));
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
        let (tree, _) = tree(Mem {
            songs,
            ..Default::default()
        });
        let song = tree.resolve("00001 s").unwrap();
        let mut names = Vec::new();
        tree.read_dir(song, 0, |_, n, _| {
            names.push(n.to_string());
            false
        });
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
        let mut count = 0;
        tree.read_dir(sub, 0, |_, _, _| {
            count += 1;
            false
        });
        assert_eq!(count, 2);
    }
}
