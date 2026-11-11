//! 사용자가 고른 난이도표. 표 주소는 **이 PC가 직접** 읽고, 서버에는 차트 해시만 보낸다
//! (`POST /api/tables/match`). 서버가 표 주소에 접속하지 않으므로 서버 IP가 표 호스트에 드러나지 않는다.
//!
//! 읽는 방식은 beatoraja(서버 `tables.py`)와 같다.
//! 1. 주소가 `.json`으로 끝나면 헤더 JSON. 아니면 HTML의 `<meta name="bmstable" content="...">`가 가리키는 주소.
//! 2. 헤더의 `name`·`symbol`(필수), `data_url`(문자열 또는 목록), `data_rule`(레벨 → 바꿀 레벨, ''이면 뺌).
//! 3. 데이터 JSON의 행 중 `level`이 있고 `md5`(32자)나 `sha256`(64자)이 있는 것만 쓴다.
//!
//! 표 요청에는 세션키를 싣지 않는다(서버 [`crate::api::Api`]와 다른 HTTP 클라이언트).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// 주소 하나의 본문 크기 상한 (서버와 같음)
const MAX_BYTES: usize = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// 표를 읽은 결과.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableHashes {
    pub name: String,
    pub symbol: String,
    pub md5: Vec<String>,
    pub sha256: Vec<String>,
    /// 표에 실린 차트 수 (한 행이 md5·sha256을 둘 다 가질 수 있어 따로 센다)
    pub charts: usize,
}

/// 사용자가 고른 표 하나와 마지막으로 맞춘 결과. 로컬 인덱스에 저장한다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTable {
    pub url: String,
    pub name: String,
    pub symbol: String,
    /// 표에 실린 차트 수
    pub charts: u32,
    /// 그중 서버에 있는 차트 수
    pub matched_charts: u32,
    /// 그 차트가 든 서버 곡 (곡 번호 순)
    pub songs: Vec<u32>,
    /// 곡 zip 크기 합
    pub bytes: u64,
    /// 마지막으로 표를 읽은 시각 (unix 초)
    pub checked_at: i64,
}

/// 표를 읽는 HTTP 클라이언트. 세션키 없음.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!(
            "ebms-client/",
            env!("CARGO_PKG_VERSION"),
            " (difficulty table)"
        ))
        .connect_timeout(Duration::from_secs(10))
        .timeout(TIMEOUT)
        .build()?)
}

async fn get(http: &reqwest::Client, url: &Url) -> Result<Vec<u8>> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Other(format!("http(s) 주소가 아닙니다: {url}")));
    }
    let resp = http.get(url.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "{url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            return Err(Error::Other(format!(
                "{url}: {MAX_BYTES} 바이트보다 큽니다"
            )));
        }
    }
    Ok(body)
}

fn parse_url(url: &str) -> Result<Url> {
    let url =
        Url::parse(url.trim()).map_err(|e| Error::Other(format!("주소가 잘못됐습니다: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Other("http(s) 주소를 넣어 주세요".into()));
    }
    Ok(url)
}

/// 표 주소를 읽어 차트 해시를 모은다.
pub async fn fetch(http: &reqwest::Client, url: &str) -> Result<TableHashes> {
    let url = parse_url(url)?;
    let header_url = if url.path().to_ascii_lowercase().ends_with(".json") {
        url
    } else {
        let html = String::from_utf8_lossy(&get(http, &url).await?).into_owned();
        find_header_url(&html, &url).ok_or_else(|| {
            Error::Other(format!("{url}: <meta name=\"bmstable\"> 태그가 없습니다"))
        })?
    };
    let header = parse_header(&get(http, &header_url).await?, &header_url)?;
    let mut acc = Collector::default();
    let mut loaded = 0;
    let mut errors = Vec::new();
    for (i, data_url) in header.data_urls.iter().enumerate() {
        let rule = header.rules.get(i).cloned().unwrap_or_default();
        let Ok(full) = header_url.join(data_url) else {
            errors.push(format!("{data_url}: 주소가 잘못됐습니다"));
            continue;
        };
        // beatoraja처럼 읽지 못한 데이터는 건너뛰고, 하나도 못 읽었을 때만 실패로 본다.
        let rows = match get(http, &full).await.and_then(|b| json(&b, &full)) {
            Ok(Value::Array(rows)) => rows,
            Ok(_) => {
                errors.push(format!("{full}: 데이터가 배열이 아닙니다"));
                continue;
            }
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        loaded += 1;
        acc.add_rows(&rows, &rule);
    }
    if loaded == 0 {
        return Err(Error::Other(if errors.is_empty() {
            format!("{header_url}: data_url이 없습니다")
        } else {
            errors.join("; ")
        }));
    }
    Ok(TableHashes {
        name: header.name,
        symbol: header.symbol,
        md5: acc.md5,
        sha256: acc.sha256,
        charts: acc.rows,
    })
}

fn json(body: &[u8], url: &Url) -> Result<Value> {
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    serde_json::from_slice(body).map_err(|e| Error::Other(format!("{url}: JSON이 아닙니다 ({e})")))
}

/// HTML의 `<meta name="bmstable" content="...">`가 가리키는 헤더 주소. 여러 개면 마지막 것.
pub fn find_header_url(html: &str, page: &Url) -> Option<Url> {
    let lower = html.to_ascii_lowercase();
    let mut found = None;
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<meta") {
        let start = pos + start;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &html[start..end];
        if attr(tag, "name").is_some_and(|n| n.eq_ignore_ascii_case("bmstable"))
            && let Some(content) = attr(tag, "content")
                .map(str::trim)
                .filter(|c| !c.is_empty())
        {
            found = Some(content.to_string());
        }
        pos = end;
    }
    page.join(&found?).ok()
}

/// 태그 안 속성 값(따옴표로 감싼 것). 이름은 대소문자를 무시한다.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let i = from + i;
        from = i + name.len();
        // 이름 앞은 공백이어야 한다(`data-name` 등 제외).
        if i == 0 || !bytes[i - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let value = &rest[1..];
        return value.find(quote).map(|e| &value[..e]);
    }
    None
}

struct Header {
    name: String,
    symbol: String,
    data_urls: Vec<String>,
    rules: Vec<BTreeMap<String, String>>,
}

fn parse_header(body: &[u8], url: &Url) -> Result<Header> {
    let header = json(body, url)?;
    let text = |k: &str| {
        header
            .get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
    };
    let (Some(name), Some(symbol)) = (text("name"), text("symbol")) else {
        return Err(Error::Other(format!(
            "{url}: 헤더에 name·symbol이 없습니다"
        )));
    };
    let data_urls = match header.get("data_url") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    // 빈 주소는 건너뛰되 규칙 순서는 data_url 순서를 따른다.
    let rules: Vec<BTreeMap<String, String>> = match header.get("data_rule") {
        Some(Value::Array(list)) => list
            .iter()
            .map(|r| {
                r.as_object()
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect(),
        _ => Vec::new(),
    };
    let (data_urls, rules) = data_urls
        .into_iter()
        .enumerate()
        .filter(|(_, u)| !u.trim().is_empty())
        .map(|(i, u)| {
            (
                u.trim().to_string(),
                rules.get(i).cloned().unwrap_or_default(),
            )
        })
        .unzip();
    Ok(Header {
        name,
        symbol,
        data_urls,
        rules,
    })
}

#[derive(Default)]
struct Collector {
    md5: Vec<String>,
    sha256: Vec<String>,
    seen_md5: HashSet<String>,
    seen_sha256: HashSet<String>,
    rows: usize,
}

impl Collector {
    fn add_rows(&mut self, rows: &[Value], rule: &BTreeMap<String, String>) {
        for row in rows {
            let Some(row) = row.as_object() else { continue };
            // title·mode·md5·sha256이 문자열이 아닌 행은 건너뛴다.
            if ["title", "mode", "md5", "sha256"]
                .iter()
                .any(|k| row.get(*k).is_some_and(|v| !v.is_null() && !v.is_string()))
            {
                continue;
            }
            let level = match row.get("level") {
                None | Some(Value::Null) => continue,
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
            };
            if rule.get(&level).is_some_and(String::is_empty) {
                continue;
            }
            let md5 = hash(row.get("md5"), 32);
            let sha256 = hash(row.get("sha256"), 64);
            if md5.is_empty() && sha256.is_empty() {
                continue;
            }
            if (!md5.is_empty() && self.seen_md5.contains(&md5))
                || (!sha256.is_empty() && self.seen_sha256.contains(&sha256))
            {
                continue;
            }
            self.rows += 1;
            if !md5.is_empty() {
                self.seen_md5.insert(md5.clone());
                self.md5.push(md5);
            }
            if !sha256.is_empty() {
                self.seen_sha256.insert(sha256.clone());
                self.sha256.push(sha256);
            }
        }
    }
}

/// 정해진 길이의 16진수 해시만 소문자로. 아니면 빈 문자열.
fn hash(value: Option<&Value>, len: usize) -> String {
    let Some(s) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let s = s.trim().to_ascii_lowercase();
    if s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        s
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn meta_tag_relative_and_last_wins() {
        let html = r#"<html><head><meta charset="utf-8">
            <META content='a.json' NAME='bmstable'>
            <meta data-name="bmstable" content="x.json">
            <meta name="bmstable" content=" data/header.json "></head>"#;
        let page = u("https://t.example/table/");
        assert_eq!(
            find_header_url(html, &page),
            Some(u("https://t.example/table/data/header.json"))
        );
        assert_eq!(find_header_url("<html></html>", &page), None);
    }

    #[test]
    fn header_and_rules() {
        let body = json!({"name": " N ", "symbol": "n", "data_url": ["a.json", " ", "b.json"],
            "data_rule": [{"1": "", "2": "II"}, {}, "bad"]})
        .to_string();
        let h = parse_header(body.as_bytes(), &u("https://t.example/h.json")).unwrap();
        assert_eq!(h.name, "N");
        assert_eq!(h.data_urls, vec!["a.json", "b.json"]);
        assert_eq!(h.rules[0].get("1").map(String::as_str), Some(""));
        assert!(h.rules[1].is_empty());
        assert!(parse_header(br#"{"name": "x"}"#, &u("https://t.example/h.json")).is_err());
    }

    #[test]
    fn rows_filtered_and_deduped() {
        let a = "a".repeat(32);
        let b = "B".repeat(64);
        let rows = json!([
            {"md5": a.to_uppercase(), "level": "1"},
            {"sha256": b, "level": 2},
            {"md5": "c".repeat(32)},                       // level 없음
            {"md5": "abc", "level": "1"},                   // 짧음
            {"md5": "z".repeat(32), "level": "1"},          // 16진수 아님
            {"md5": "d".repeat(32), "level": "1", "title": 3}, // title이 문자열 아님
            {"md5": "e".repeat(32), "level": "0"},          // 규칙으로 뺌
            {"md5": a, "level": "9"},                       // 중복
            "not a row"
        ]);
        let rule = BTreeMap::from([("0".to_string(), String::new())]);
        let mut c = Collector::default();
        c.add_rows(rows.as_array().unwrap(), &rule);
        assert_eq!(c.md5, vec!["a".repeat(32)]);
        assert_eq!(c.sha256, vec!["b".repeat(64)]);
        assert_eq!(c.rows, 2);
    }
}
