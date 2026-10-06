use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::manifest::SongManifest;
use crate::{Error, Result};

#[derive(Debug, Deserialize)]
pub struct Version {
    pub api: u32,
    pub server: Option<String>,
}

/// 서버 HTTP API 클라이언트.
#[derive(Clone, Debug)]
pub struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ebms-client/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let url = self.url(path);
        let resp = self.http.get(&url).send().await?;
        check(resp, StatusCode::OK)
    }

    pub async fn version(&self) -> Result<Version> {
        Ok(self.get("/api/version").await?.json().await?)
    }

    pub async fn chart_hash(&self) -> Result<BTreeMap<u32, String>> {
        self.hash_map("/api/charthash").await
    }

    pub async fn manifest_hash(&self) -> Result<BTreeMap<u32, String>> {
        self.hash_map("/api/manifest/hash").await
    }

    async fn hash_map(&self, path: &str) -> Result<BTreeMap<u32, String>> {
        // JSON 키는 문자열이다.
        let raw: BTreeMap<String, String> = self.get(path).await?.json().await?;
        raw.into_iter()
            .map(|(k, v)| {
                k.parse()
                    .map(|k| (k, v))
                    .map_err(|_| Error::Other(format!("bad chunk id {k:?} from {path}")))
            })
            .collect()
    }

    /// 매니페스트 청크를 받아 압축 해제된 JSON의 sha256과 함께 돌려준다.
    pub async fn manifest(&self, chunk_id: u32) -> Result<(String, Vec<SongManifest>)> {
        let body = self
            .get(&format!("/api/manifest/{chunk_id}"))
            .await?
            .bytes()
            .await?;
        Ok((crate::sha256_hex(&body), serde_json::from_slice(&body)?))
    }

    /// 차트 청크 zip을 `dest`에 저장하고 sha256을 돌려준다.
    pub async fn download_chart_chunk(&self, chunk_id: u32, dest: &Path) -> Result<String> {
        let resp = self.get(&format!("/api/files/chart/{chunk_id}")).await?;
        save(resp, dest).await
    }

    /// 곡 zip 전체를 `dest`에 저장하고 sha256을 돌려준다.
    pub async fn download_song(&self, song_id: u32, dest: &Path) -> Result<String> {
        let resp = self.get(&format!("/api/files/song/id/{song_id}")).await?;
        save(resp, dest).await
    }

    /// 곡 zip의 `[start, end]`(양끝 포함) 바이트를 받는다.
    ///
    /// `If-Range`에 zip sha256을 넣어, 서버의 곡 파일이 바뀌었으면 엉뚱한 바이트 대신 오류를 낸다.
    pub async fn song_range(
        &self,
        song_id: u32,
        zip_sha256: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes> {
        let url = self.url(&format!("/api/files/song/id/{song_id}"));
        let resp = self
            .http
            .get(&url)
            .header(header::RANGE, format!("bytes={start}-{end}"))
            .header(header::IF_RANGE, format!("\"{zip_sha256}\""))
            .send()
            .await?;
        if resp.status() == StatusCode::OK {
            // 전체 응답 = 파일이 바뀜. 본문은 읽지 않고 버린다.
            return Err(Error::Integrity(format!(
                "song {song_id} changed on server"
            )));
        }
        let resp = check(resp, StatusCode::PARTIAL_CONTENT)?;
        let body = resp.bytes().await?;
        let expected = end - start + 1;
        if body.len() as u64 != expected {
            return Err(Error::Integrity(format!(
                "song {song_id} range {start}-{end}: got {} bytes",
                body.len()
            )));
        }
        Ok(body)
    }
}

fn check(resp: reqwest::Response, expected: StatusCode) -> Result<reqwest::Response> {
    if resp.status() == expected {
        Ok(resp)
    } else {
        Err(Error::Status {
            status: resp.status().as_u16(),
            url: resp.url().to_string(),
        })
    }
}

async fn save(resp: reqwest::Response, dest: &Path) -> Result<String> {
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(hex::encode(hasher.finalize()))
}
