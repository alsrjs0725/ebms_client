use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::manifest::SongManifest;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chunk(
    kind TEXT NOT NULL,          -- 'chart' | 'manifest' | 'pre'
    id INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
CREATE TABLE IF NOT EXISTS chart(
    sha256 TEXT PRIMARY KEY,
    ext TEXT NOT NULL,
    size INTEGER NOT NULL,
    crc32 INTEGER NOT NULL,
    chunk_id INTEGER NOT NULL,
    data_offset INTEGER NOT NULL  -- 청크 zip(무압축) 안 데이터 시작 위치
);
CREATE INDEX IF NOT EXISTS chart_chunk ON chart(chunk_id);
CREATE TABLE IF NOT EXISTS pre_file(
    song_id INTEGER NOT NULL,
    path TEXT NOT NULL,           -- 곡 zip 안 경로
    size INTEGER NOT NULL,
    crc32 INTEGER NOT NULL,
    chunk_id INTEGER NOT NULL,
    data_offset INTEGER NOT NULL, -- 사전 청크 zip(무압축) 안 데이터 시작 위치
    PRIMARY KEY (song_id, path)
);
CREATE INDEX IF NOT EXISTS pre_file_chunk ON pre_file(chunk_id);
CREATE TABLE IF NOT EXISTS song(
    id INTEGER PRIMARY KEY,
    manifest_chunk INTEGER NOT NULL,
    manifest TEXT NOT NULL        -- SongManifest JSON
);
CREATE INDEX IF NOT EXISTS song_manifest ON song(manifest_chunk);
CREATE TABLE IF NOT EXISTS cache_entry(
    song_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    last_access INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (song_id, path)
);
CREATE TABLE IF NOT EXISTS cache_song(
    song_id INTEGER PRIMARY KEY,
    zip_sha256 TEXT NOT NULL      -- 캐시에 든 파일들이 나온 곡 zip. 매니페스트와 다르면 폐기
);
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    Chart,
    Manifest,
    Pre,
}

impl ChunkKind {
    fn as_str(self) -> &'static str {
        match self {
            ChunkKind::Chart => "chart",
            ChunkKind::Manifest => "manifest",
            ChunkKind::Pre => "pre",
        }
    }
}

/// 청크 zip 안의 차트 하나.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartRow {
    pub sha256: String,
    pub ext: String,
    pub size: u64,
    pub crc32: u32,
    pub chunk_id: u32,
    pub data_offset: u64,
}

/// 사전 청크 zip 안의 사전 파일 하나(배너·프리뷰 등).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreRow {
    pub song_id: u32,
    pub path: String,
    pub size: u64,
    pub crc32: u32,
    pub chunk_id: u32,
    pub data_offset: u64,
}

#[derive(Clone, Debug)]
pub struct CacheRow {
    pub song_id: u32,
    pub path: String,
    pub size: u64,
    pub last_access: i64,
    pub pinned: bool,
}

/// `(song_id, 캐시에 기록된 zip 해시, 매니페스트의 zip 해시)`
pub type CacheSongVersion = (u32, Option<String>, Option<String>);

/// 로컬 SQLite 인덱스.
pub struct Index {
    con: Mutex<Connection>,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let con = Connection::open(path)?;
        con.pragma_update(None, "journal_mode", "WAL")?;
        con.pragma_update(None, "synchronous", "NORMAL")?;
        con.execute_batch(SCHEMA)?;
        Ok(Self {
            con: Mutex::new(con),
        })
    }

    fn con(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.con.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn chunk_hashes(&self, kind: ChunkKind) -> Result<BTreeMap<u32, String>> {
        let con = self.con();
        let mut stmt = con.prepare("SELECT id, sha256 FROM chunk WHERE kind = ?1")?;
        let rows = stmt.query_map([kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 차트 청크 하나의 차트 목록과 해시를 한 트랜잭션으로 바꾼다.
    pub fn replace_chart_chunk(
        &self,
        chunk_id: u32,
        sha256: &str,
        charts: &[ChartRow],
    ) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM chart WHERE chunk_id = ?1", [chunk_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO chart (sha256, ext, size, crc32, chunk_id, data_offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for c in charts {
                stmt.execute(params![
                    c.sha256,
                    c.ext,
                    c.size as i64,
                    c.crc32,
                    c.chunk_id,
                    c.data_offset as i64
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO chunk (kind, id, sha256) VALUES ('chart', ?1, ?2)",
            params![chunk_id, sha256],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_chart_chunk(&self, chunk_id: u32) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM chart WHERE chunk_id = ?1", [chunk_id])?;
        tx.execute(
            "DELETE FROM chunk WHERE kind = 'chart' AND id = ?1",
            [chunk_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 사전 청크 하나의 파일 목록과 해시를 한 트랜잭션으로 바꾼다.
    pub fn replace_pre_chunk(&self, chunk_id: u32, sha256: &str, files: &[PreRow]) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM pre_file WHERE chunk_id = ?1", [chunk_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO pre_file (song_id, path, size, crc32, chunk_id, data_offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for f in files {
                stmt.execute(params![
                    f.song_id,
                    f.path,
                    f.size as i64,
                    f.crc32,
                    f.chunk_id,
                    f.data_offset as i64
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO chunk (kind, id, sha256) VALUES ('pre', ?1, ?2)",
            params![chunk_id, sha256],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_pre_chunk(&self, chunk_id: u32) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM pre_file WHERE chunk_id = ?1", [chunk_id])?;
        tx.execute(
            "DELETE FROM chunk WHERE kind = 'pre' AND id = ?1",
            [chunk_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 곡의 로컬 사전 파일.
    pub fn pre_files(&self, song_id: u32) -> Result<Vec<PreRow>> {
        let con = self.con();
        let mut stmt = con.prepare_cached(
            "SELECT path, size, crc32, chunk_id, data_offset FROM pre_file WHERE song_id = ?1",
        )?;
        let rows = stmt.query_map([song_id], |r| {
            Ok(PreRow {
                song_id,
                path: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
                crc32: r.get(2)?,
                chunk_id: r.get(3)?,
                data_offset: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 매니페스트 청크 하나의 곡 목록과 해시를 한 트랜잭션으로 바꾼다.
    pub fn replace_manifest_chunk(
        &self,
        chunk_id: u32,
        sha256: &str,
        songs: &[SongManifest],
    ) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM song WHERE manifest_chunk = ?1", [chunk_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO song (id, manifest_chunk, manifest) VALUES (?1, ?2, ?3)",
            )?;
            for s in songs {
                stmt.execute(params![s.song_id, chunk_id, serde_json::to_string(s)?])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO chunk (kind, id, sha256) VALUES ('manifest', ?1, ?2)",
            params![chunk_id, sha256],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_manifest_chunk(&self, chunk_id: u32) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM song WHERE manifest_chunk = ?1", [chunk_id])?;
        tx.execute(
            "DELETE FROM chunk WHERE kind = 'manifest' AND id = ?1",
            [chunk_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 곡 폴더 목록 `(song_id, folder)`. 파일 목록은 읽지 않는다.
    pub fn song_folders(&self) -> Result<Vec<(u32, String)>> {
        let con = self.con();
        let mut stmt =
            con.prepare("SELECT id, json_extract(manifest, '$.folder') FROM song ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn song(&self, song_id: u32) -> Result<Option<SongManifest>> {
        let json: Option<String> = self
            .con()
            .query_row("SELECT manifest FROM song WHERE id = ?1", [song_id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    pub fn charts(&self) -> Result<HashMap<String, ChartRow>> {
        let con = self.con();
        let mut stmt =
            con.prepare("SELECT sha256, ext, size, crc32, chunk_id, data_offset FROM chart")?;
        let rows = stmt.query_map([], chart_row)?;
        let mut out = HashMap::new();
        for row in rows {
            let row = row?;
            out.insert(row.sha256.clone(), row);
        }
        Ok(out)
    }

    /// sha256으로 차트를 찾는다. 없는 것은 빠진다.
    pub fn charts_by_sha(&self, sha256: &[String]) -> Result<Vec<ChartRow>> {
        let con = self.con();
        let mut stmt = con.prepare_cached(
            "SELECT sha256, ext, size, crc32, chunk_id, data_offset FROM chart WHERE sha256 = ?1",
        )?;
        let mut out = Vec::new();
        for sha in sha256 {
            if let Some(row) = stmt.query_row([sha], chart_row).optional()? {
                out.push(row);
            }
        }
        Ok(out)
    }

    pub fn cache_get(&self, song_id: u32, path: &str) -> Result<Option<CacheRow>> {
        let con = self.con();
        Ok(con
            .query_row(
                "SELECT size, last_access, pinned FROM cache_entry WHERE song_id = ?1 AND path = ?2",
                params![song_id, path],
                |r| {
                    Ok(CacheRow {
                        song_id,
                        path: path.to_string(),
                        size: r.get::<_, i64>(0)? as u64,
                        last_access: r.get(1)?,
                        pinned: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn cache_put(&self, song_id: u32, path: &str, size: u64, now: i64) -> Result<()> {
        self.con().execute(
            "INSERT INTO cache_entry (song_id, path, size, last_access) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(song_id, path) DO UPDATE SET size = excluded.size, last_access = excluded.last_access",
            params![song_id, path, size as i64, now],
        )?;
        Ok(())
    }

    pub fn cache_touch(&self, song_id: u32, path: &str, now: i64) -> Result<()> {
        self.con().execute(
            "UPDATE cache_entry SET last_access = ?3 WHERE song_id = ?1 AND path = ?2",
            params![song_id, path, now],
        )?;
        Ok(())
    }

    pub fn cache_remove(&self, song_id: u32, path: &str) -> Result<()> {
        self.con().execute(
            "DELETE FROM cache_entry WHERE song_id = ?1 AND path = ?2",
            params![song_id, path],
        )?;
        Ok(())
    }

    /// 캐시에 든 곡 파일들의 곡 zip 해시.
    pub fn cache_song_sha(&self, song_id: u32) -> Result<Option<String>> {
        Ok(self
            .con()
            .query_row(
                "SELECT zip_sha256 FROM cache_song WHERE song_id = ?1",
                [song_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn cache_set_song_sha(&self, song_id: u32, zip_sha256: &str) -> Result<()> {
        self.con().execute(
            "INSERT OR REPLACE INTO cache_song (song_id, zip_sha256) VALUES (?1, ?2)",
            params![song_id, zip_sha256],
        )?;
        Ok(())
    }

    /// 캐시에 파일이 있는 곡마다 `(song_id, 기록된 zip 해시, 매니페스트의 zip 해시)`.
    /// 매니페스트에 없는 곡은 세 번째가 `None`.
    pub fn cache_song_versions(&self) -> Result<Vec<CacheSongVersion>> {
        let con = self.con();
        let mut stmt = con.prepare(
            "SELECT e.song_id, c.zip_sha256, json_extract(s.manifest, '$.zip_sha256')
             FROM (SELECT DISTINCT song_id FROM cache_entry) e
             LEFT JOIN cache_song c ON c.song_id = e.song_id
             LEFT JOIN song s ON s.id = e.song_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 곡의 캐시 항목 경로.
    pub fn cache_song_paths(&self, song_id: u32) -> Result<Vec<(String, u64)>> {
        let con = self.con();
        let mut stmt = con.prepare("SELECT path, size FROM cache_entry WHERE song_id = ?1")?;
        let rows = stmt.query_map([song_id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 곡의 캐시 항목과 해시 기록을 모두 지운다.
    pub fn cache_remove_song(&self, song_id: u32) -> Result<()> {
        let mut con = self.con();
        let tx = con.transaction()?;
        tx.execute("DELETE FROM cache_entry WHERE song_id = ?1", [song_id])?;
        tx.execute("DELETE FROM cache_song WHERE song_id = ?1", [song_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn cache_set_pinned(&self, song_id: u32, pinned: bool) -> Result<()> {
        self.con().execute(
            "UPDATE cache_entry SET pinned = ?2 WHERE song_id = ?1",
            params![song_id, pinned],
        )?;
        Ok(())
    }

    pub fn cache_total(&self) -> Result<u64> {
        Ok(self
            .con()
            .query_row("SELECT COALESCE(SUM(size), 0) FROM cache_entry", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }

    /// 고정되지 않은 곡을 마지막 접근(곡 파일 중 가장 최근)이 오래된 순으로 `(song_id, 크기 합)`.
    /// 파일 하나라도 고정된 곡은 빠진다.
    pub fn cache_eviction_songs(&self, limit: usize) -> Result<Vec<(u32, u64)>> {
        let con = self.con();
        let mut stmt = con.prepare(
            "SELECT song_id, SUM(size) FROM cache_entry
             GROUP BY song_id HAVING MAX(pinned) = 0
             ORDER BY MAX(last_access) LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn chart_row(r: &rusqlite::Row) -> rusqlite::Result<ChartRow> {
    Ok(ChartRow {
        sha256: r.get(0)?,
        ext: r.get(1)?,
        size: r.get::<_, i64>(2)? as u64,
        crc32: r.get(3)?,
        chunk_id: r.get(4)?,
        data_offset: r.get::<_, i64>(5)? as u64,
    })
}
