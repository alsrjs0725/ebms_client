# EBMS Client 설계

> 상태: 초안 (2026-10-06). 서버 `alsrjs0725/ebms_server@main` 기준.

## 1. 클라이언트가 하는 일

서버는 BMS 차트 파일과 곡 에셋을 배포한다. 클라이언트는 이를 받아 사용자의 BMS 폴더에 설치한다.

| 서버 API | 내용 | 클라이언트 용도 |
| --- | --- | --- |
| `GET /api/charthash` | `{chunk_id: sha256}` | 바뀐 청크만 골라 받기 |
| `GET /api/files/chart/{chunk_id}` | 차트 파일(.bms/.bme/.bml/.pms) 묶음 zip, 청크당 약 64MB, 무압축 | 전체 차트 목록을 로컬에 보유 → 검색·인덱싱 |
| `GET /api/files/song/{chart_sha256}` | 해당 차트가 속한 곡 폴더 전체 zip | 곡 설치 |

핵심 흐름: **차트(가벼움)는 전부 동기화 → 로컬에서 검색/난이도표 매칭 → 필요한 곡(무거움)만 받아 설치.**

차트를 로컬에 갖고 있어야 하는 이유: 난이도표는 대부분 **md5**로 차트를 가리키는데 서버는 sha256만 안다. 클라이언트가 차트 원본으로 md5↔sha256 매핑을 직접 만든다.

## 2. 요구사항

### 기능
1. **차트 동기화**: `charthash` 비교 → 변경 청크만 다운로드 → sha256 검증 → 인덱스 갱신. 보통 마지막 청크 1개만 바뀐다(서버는 마지막 청크에만 추가).
2. **차트 인덱스/검색**: BMS 헤더(TITLE, SUBTITLE, ARTIST, GENRE, BPM, PLAYLEVEL, DIFFICULTY, 키 모드) 파싱. 제목·아티스트 검색, 필터, 정렬.
3. **보유 곡 판별**: beatoraja `songdata.db`(sha256/md5/path) 읽기. 없으면 지정 폴더 스캔 후 해시 계산.
4. **곡 설치**: 다운로드 큐(동시 N개, 진행률, 취소/재시도) → 임시 파일 → 압축 해제 → 라이브러리 폴더에 원자적 이동.
5. **난이도표**: BMS 표 URL(`<meta name="bmstable">` → header.json → data.json) 등록, 레벨별 보유/미보유 표시, "이 레벨 미보유 전부 설치".
6. **설정**: 서버 URL, 라이브러리 폴더, 플레이어 종류(beatoraja/LR2/없음), 동시 다운로드 수, 언어(ko/ja/en).

### 비기능
- Windows / macOS / Linux 동일 동작. Windows 우선(BMS 사용자 대부분).
- 차트 10만 개 이상에서도 검색 즉시 응답 (SQLite FTS + 가상 스크롤).
- 다운로드 중 앱이 죽어도 라이브러리 폴더가 깨지지 않음 (임시 폴더 → rename).
- 배포 파일 작게(수십 MB 이하), 자동 업데이트.

## 3. 기술 스택 (결정)

**Tauri 2 + Rust 코어 + React/TypeScript UI**

| 후보 | 판단 |
| --- | --- |
| **Tauri 2 (Rust + Web UI)** | ✅ 선택. 바이너리 수 MB, 대용량 IO·해싱·zip이 빠름, 3개 OS 빌드·자동 업데이트 내장 |
| Electron | 무겁고(100MB+) Node로 대용량 zip/해싱 처리 부담 |
| Python + PySide6 | 서버와 언어는 같지만 패키징이 크고 느림, 배포·업데이트 불편 |
| Flutter / .NET Avalonia | 가능하나 파일 IO·인코딩 생태계가 Rust만 못함 |

### Rust 크레이트
| 용도 | 크레이트 |
| --- | --- |
| 비동기/HTTP | `tokio`, `reqwest` (stream, rustls) |
| zip | `zip` |
| 해시 | `sha2`, `md-5` |
| DB | `rusqlite` (bundled, FTS5) + `rusqlite_migration` |
| BMS 인코딩 | `encoding_rs` (UTF-8 → Shift_JIS → EUC-KR 순 판별) |
| 직렬화/설정 | `serde`, `serde_json`, `toml` |
| 에러/로그 | `thiserror`, `anyhow`(앱 계층만), `tracing` |
| 난이도표 HTML | `scraper` |
| 경로 | `directories` (OS별 앱 데이터 경로) |
| Tauri 플러그인 | `dialog`, `updater`, `log`, `opener` |
| TS 타입 생성 | `specta` + `tauri-specta` (Rust 커맨드 → TS 타입 자동 생성) |

### 프론트엔드
- React 19 + TypeScript + Vite
- 상태: TanStack Query(백엔드 데이터), Zustand(UI 상태)
- 목록: TanStack Table + TanStack Virtual
- UI: Tailwind CSS + shadcn/ui
- 라우팅: TanStack Router
- i18n: i18next

## 4. 아키텍처

```
┌──────────── UI (React/TS) ────────────┐
│ 검색 · 난이도표 · 다운로드 큐 · 설정    │
└──────────▲───────────────┬────────────┘
   이벤트(진행률 등)        │ invoke (타입 자동 생성)
┌──────────┴───────────────▼────────────┐
│ src-tauri  : 커맨드/이벤트 얇은 어댑터  │
├───────────────────────────────────────┤
│ ebms-core (UI 무관 순수 Rust 라이브러리)│
│  api      서버 HTTP 클라이언트          │
│  sync     청크 비교·다운로드·검증       │
│  bms      헤더 파서, 인코딩, 해시       │
│  index    SQLite 스키마/쿼리            │
│  library  보유 판별(songdata.db, 스캔)  │
│  install  다운로드 큐, 압축 해제, 이동  │
│  table    난이도표 로딩/매칭            │
└───────────────────────────────────────┘
        ebms-cli : 같은 코어를 쓰는 CLI (테스트·헤드리스용)
```

원칙: **로직은 전부 `ebms-core`.** Tauri 계층은 직렬화와 이벤트 전달만 한다. 그래서 코어는 UI 없이 `cargo test`와 CLI로 검증할 수 있다.

## 5. 저장소 구조

```
ebms_client/
├─ Cargo.toml                # workspace
├─ crates/
│  ├─ ebms-core/
│  │  └─ src/{api,sync,bms,index,library,install,table}/
│  └─ ebms-cli/
├─ src-tauri/                # Tauri 앱 (commands.rs, events.rs, main.rs)
├─ ui/                       # React 앱
│  └─ src/{routes,features/{search,tables,downloads,settings},components,lib/bindings.ts}
├─ docs/
└─ .github/workflows/        # lint·test, 3 OS 빌드·릴리스
```

## 6. 로컬 데이터

OS별 앱 데이터 폴더(`directories` 크레이트) 아래:

```
config.toml
index.sqlite
chunks/chart_chunk_00000.zip ...   # 원본 보관, 해시 비교용 (압축 해제 안 함)
tmp/                               # 다운로드 중 파일
logs/
```

### index.sqlite 주요 테이블
| 테이블 | 컬럼 |
| --- | --- |
| `chunk` | id, sha256, size, synced_at |
| `chart` | sha256 PK, md5, size, chunk_id, filename, title, subtitle, artist, subartist, genre, bpm, playlevel, difficulty, mode |
| `chart_fts` | FTS5(title, subtitle, artist, genre) |
| `owned` | sha256, path, source(`songdata`/`scan`) |
| `bms_table`, `bms_table_entry` | url, name, symbol / table_id, md5, sha256?, level |
| `download_job` | chart_sha256, state, bytes, error, updated_at |

## 7. 주요 흐름

**차트 동기화**
1. `GET /api/charthash`
2. 로컬 `chunk`와 sha256 비교 → 다른 것만 `tmp/`로 스트리밍 다운로드
3. sha256 검증 → `chunks/`로 rename
4. 해당 chunk_id 차트 삭제 후 재인덱싱 (zip 엔트리는 **이름이 아닌 인덱스로 순회**, 해시는 내용으로 계산)

**곡 설치**
1. 큐에 chart sha256 추가
2. `GET /api/files/song/{sha256}` → `tmp/` 스트리밍 (Content-Length로 진행률)
3. 압축 해제: zip-slip 방지, 파일명 인코딩 처리
4. 폴더명 `"{artist} - {title}"`(OS 금지문자 치환, 충돌 시 접미사) → 라이브러리 폴더로 rename
5. `owned` 갱신, UI 이벤트. 플레이어에는 "곡 목록 갱신 필요" 안내

## 8. 기본값으로 정한 것 (바꿀 수 있음)

- UI 프레임워크: React (Svelte도 가능하나 생태계·라이브러리 폭 우선)
- 청크는 압축 해제 없이 zip 그대로 보관
- 설치 폴더명: `아티스트 - 제목`
- 보유 판별: beatoraja `songdata.db` 우선, 없으면 폴더 스캔
- 최소 지원: Windows 10+, macOS 12+, Ubuntu 22.04+

## 9. 서버에 제안할 변경

| 문제 | 영향 | 제안 |
| --- | --- | --- |
| 청크 zip 내부 이름이 원본 파일명 | 다른 곡의 `_7a.bme` 등이 같은 청크에서 충돌 | arcname을 `{sha256}{ext}`로 |
| `Range` 미지원 | 큰 곡 다운로드 이어받기 불가 | Range/ETag 지원 |
| 곡 zip 해시 미노출 | 받은 곡 무결성 검증 불가 | 응답 헤더에 sha256 (`song.sha256` 이미 있음) |
| md5 조회 없음 | 클라가 차트 전부 받아야 매핑 가능 | 현재 설계로 해결됨. 필요 시 `/api/chart?md5=` |
| API 버전 없음 | 호환성 깨질 때 대응 어려움 | `/api/v1/...` 또는 `/api/version` |

## 10. 구현 순서

1. 워크스페이스·CI 골격 (Tauri 빈 앱, 3 OS 빌드)
2. `ebms-core`: api + sync + bms 파서 + index (CLI로 동기화·검색 확인)
3. UI: 검색 화면, 동기화 버튼
4. install + 다운로드 큐 UI
5. library(보유 판별) + 난이도표
6. 자동 업데이트, 릴리스 파이프라인
