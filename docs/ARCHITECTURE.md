# EBMS Client 설계

> 상태: 초안 v2 (2026-10-06). 서버 `alsrjs0725/ebms_server@main` 기준.

## 1. 목표

사용자가 곡을 찾아 받지 않는다. 클라이언트가 서버 전체 곡을 **가상 드라이브**로 보여주고, beatoraja 같은 구동기가 파일을 읽는 순간 필요한 에셋을 받아온다.

| 대상 | 언제 받나 | 디스크 |
| --- | --- | --- |
| 차트(.bms/.bme/.bml/.pms) | 서버 연결 시 전부 미리 동기화 | 로컬 보관 (작음) |
| 곡 에셋(wav/ogg/bmp/png/mp4…) | 구동기가 파일을 열 때 | 캐시 (용량 제한, 오래된 것부터 삭제) |

구동기 입장에서는 모든 곡이 이미 설치된 일반 폴더로 보인다.

```
E:\ (또는 ~/ebms 마운트 지점)
└─ 00123 Artist - Title\
   ├─ _7a.bme        ← 로컬 차트, 즉시 읽힘
   ├─ bgm01.ogg      ← 크기·이름만 보임, 열면 다운로드
   └─ stagefile.png
```

## 2. 요구사항

### 기능
1. **자동 차트 동기화**: 앱 시작·서버 연결 시, 이후 주기적으로 `charthash` 비교 → 바뀐 청크만 받기 → 검증 → 인덱스·가상 트리 갱신.
2. **가상 드라이브**: 곡 폴더/파일 트리를 읽기 전용으로 마운트. 목록·크기 조회(`ls`, `stat`)는 네트워크 없이 응답.
3. **요청 시 다운로드**: 파일 `open/read` 시 해당 데이터를 받아 캐시 후 응답. 받는 동안 읽기 요청은 대기.
4. **캐시 관리**: 용량 한도(기본 20GB), LRU 삭제, 곡 단위 "항상 보관" 고정, 수동 비우기.
5. **트레이 앱 UI**: 연결·동기화 상태, 진행 중 다운로드, 캐시 사용량, 설정(서버 URL, 마운트 위치, 캐시 한도).
6. **구동기 안내**: beatoraja 곡 폴더에 마운트 경로 추가 방법 안내, 동기화 후 "곡 목록 갱신 필요" 알림.

### 비기능
- Windows 우선, Linux 지원, macOS는 PoC 후 결정 (§5 참고).
- 곡 고르는 화면에서 스크롤만 해도 곡 전체가 받아지는 일이 없어야 함 (§6).
- 오프라인이면 캐시에 있는 곡은 그대로 플레이 가능, 없는 파일은 읽기 오류.
- 다운로드 중 앱이 죽어도 캐시가 깨지지 않음 (임시 파일 → 검증 → rename).

## 3. 기술 스택

**Tauri 2 + Rust 코어 + React/TypeScript UI** (v1과 동일. 가상 파일시스템 바인딩이 Rust에 가장 잘 갖춰져 있어 더 확실해짐)

| 용도 | 크레이트/도구 |
| --- | --- |
| 가상 FS (Windows) | `winfsp` 0.13 (WinFsp 2.1 바인딩) |
| 가상 FS (Linux, macOS) | `fuser` 0.18 |
| 비동기/HTTP | `tokio`, `reqwest` (stream, Range, rustls) |
| zip | `zip` |
| 해시 | `sha2`, `md-5` |
| DB | `rusqlite` (bundled) + `rusqlite_migration` |
| BMS 인코딩 | `encoding_rs` (UTF-8 → Shift_JIS → EUC-KR) |
| 설정/직렬화 | `serde`, `toml` |
| 로그/에러 | `tracing`, `thiserror` |
| 앱 | Tauri 플러그인 `tray`, `updater`, `autostart`, `dialog` |
| UI | React 19, Vite, TanStack Query, Tailwind + shadcn/ui, i18next |

UI는 v1보다 작아진다(검색·설치 화면 대신 트레이·상태·설정 위주).

## 4. 아키텍처

```
 beatoraja / LR2 ──파일 읽기──▶ 가상 드라이브
                                  │
┌─────────────────────────────────▼───────────────┐
│ vfs       OS 백엔드(WinFsp / FUSE) → 공통 trait  │
├──────────────────────────────────────────────────┤
│ tree      곡/파일 트리 (index.sqlite에서 메모리로) │
│ fetcher   요청 합치기, 우선순위, Range 다운로드    │
│ cache     에셋 캐시, LRU, 고정, 무결성            │
│ sync      청크·매니페스트 동기화                  │
│ api       서버 HTTP 클라이언트                    │
│ bms       차트 파서·해시                          │
└──────────────────────────────────────────────────┘
        ▲ 상태/이벤트
 src-tauri (트레이 앱) ── ui (React)
 ebms-cli  (마운트·동기화를 터미널에서, 테스트용)
```

원칙
- 로직은 전부 `ebms-core`. FS 백엔드는 `ReadOnlyFs` trait(`lookup`, `getattr`, `readdir`, `open`, `read`) 하나만 구현한다.
- FS 콜백 스레드는 절대 네트워크를 직접 기다리지 않는다. `fetcher`에 요청하고 완료 신호를 기다린다. 같은 파일 동시 요청은 한 번만 받는다.

## 5. OS별 가상 FS 선택

| OS | 선택 | 이유 / 대안 |
| --- | --- | --- |
| Windows | **WinFsp** | 진짜 가상 드라이브, 읽기 대기 시간 제한 없음, rclone·sshfs-win이 쓰는 검증된 드라이버. 설치기에 WinFsp 포함 필요. 대안 Cloud Files API(`cloud-filter`, OneDrive 방식)는 드라이버 설치가 없지만 10만 개 이상 placeholder를 실제 NTFS에 만들어야 하고 콜백 60초 제한이 있어 2순위 |
| Linux | **FUSE** (`fuser`) | 표준. `fuse3` 패키지만 있으면 됨 |
| macOS | **보류** | macFUSE는 커널 확장 승인이 필요하고, FUSE-T(커널 확장 없음)는 `fuser` 호환을 PoC로 확인해야 함. beatoraja 사용자 비중이 낮아 마지막 단계로 |

## 6. 다운로드 전략 (핵심)

beatoraja가 파일을 읽는 시점은 두 가지다.

| 시점 | 읽는 파일 | 원하는 동작 |
| --- | --- | --- |
| 곡 목록 갱신 | 차트 파일 | 로컬이라 즉시 |
| 선곡 화면 이동 | `#BANNER`, `#STAGEFILE`, `preview*.ogg` | **그 파일만** 작게 받기 |
| 플레이 시작 | 키음·BGA 수백 개 | **곡 전체**를 한 번에 받기 |

그래서 혼합 전략을 쓴다.
1. **파일 단위**: 곡 zip은 항목마다 따로 압축되어 있으므로 HTTP `Range`로 해당 항목 바이트만 받아 압축을 푼다. 배너·프리뷰가 여기에 해당.
2. **곡 단위 승격**: 한 곡에서 키음(.wav/.ogg/.flac)이나 영상이 N개(기본 3개) 이상 열리면 곡 zip 전체를 한 번에 받는다. 나머지 파일 요청은 이 다운로드를 기다린다.
3. 차트 헤더로 배너·스테이지파일 이름을 미리 알 수 있으므로, 이 파일들은 "가벼운 파일"로 분류해 승격 판단에서 뺀다.

진행 상황은 트레이 알림으로 보여준다("다운로드 중: Title 45%").

## 7. 서버에 필요한 변경 (필수)

이슈: [#6 매니페스트](https://github.com/alsrjs0725/ebms_server/issues/6) · [#7 Range·song_id·해시](https://github.com/alsrjs0725/ebms_server/issues/7) · [#8 청크 파일명 충돌](https://github.com/alsrjs0725/ebms_server/issues/8)

가상 트리를 만들려면 **곡마다 파일 목록**이 필요한데, 지금 서버는 이를 주지 않는다.

| 필요 | 이유 | 제안 |
| --- | --- | --- |
| **곡 매니페스트** | 폴더·파일명·크기 트리를 다운로드 없이 표시 | 차트 청크처럼 청크 단위로 제공: `GET /api/manifest/hash`, `GET /api/manifest/{chunk_id}`. 내용: song_id, 폴더명, 차트 sha256 목록, 파일별 `path, size, zip 내 offset, 압축 크기, crc32` |
| **Range 지원** | 파일 단위 다운로드 | `/api/files/song/...`에 `Range`/`206` 지원. DB는 이미 `SUBSTRING`으로 부분 읽기 중이라 구현 쉬움 |
| **song_id로 다운로드** | 매니페스트가 song_id 기준 | `GET /api/files/song/id/{song_id}` |
| 곡 zip 해시 노출 | 전체 다운로드 검증 | 응답 헤더 `X-Content-SHA256` (`song.sha256` 이미 있음) |
| 청크 내 파일명 충돌 | 다른 곡의 같은 이름(`_7a.bme`) 충돌 | 청크 arcname을 `{sha256}{ext}`로. 원래 이름은 매니페스트에 |
| API 버전 | 호환성 | `/api/version` |

매니페스트는 곡을 넣을 때(`insert_song`) zip을 만들며 함께 계산해 DB에 저장하면 된다.

## 8. 저장소 구조

```
ebms_client/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ ebms-core/src/{api,sync,bms,tree,fetcher,cache,vfs}/
│  ├─ ebms-vfs-winfsp/        # Windows 백엔드
│  ├─ ebms-vfs-fuse/          # Linux/macOS 백엔드
│  └─ ebms-cli/
├─ src-tauri/                 # 트레이 앱
├─ ui/                        # React
├─ docs/
└─ .github/workflows/
```

## 9. 로컬 데이터

```
<앱 데이터>/
├─ config.toml
├─ index.sqlite        # chunk, chart, song, song_file, cache_entry
├─ charts/             # 청크 zip 원본 (가상 FS가 차트를 여기서 읽음)
├─ cache/<song_id>/    # 받은 에셋
└─ tmp/
```

| 테이블 | 주요 컬럼 |
| --- | --- |
| `song` | id, folder_name, manifest_chunk |
| `song_file` | song_id, path, size, zip_offset, comp_size, crc32, kind(chart/light/heavy) |
| `chart` | sha256, md5, song_id, title, artist, … |
| `cache_entry` | song_id, path, size, last_access, pinned |

## 10. 기본값으로 정한 것

- Windows 마운트: 빈 드라이브 문자 자동 선택(설정에서 폴더 마운트로 변경 가능)
- 폴더명: `{song_id:05} {artist} - {title}` (ID 접두사로 이름 충돌 방지)
- 읽기 전용 마운트
- 캐시 한도 20GB, 곡 단위 승격 임계값 3개
- 자동 동기화: 시작 시 + 30분마다

## 11. 구현 순서

1. **PoC**: Windows WinFsp에 고정 트리를 마운트하고 beatoraja가 곡 인식·플레이하는지 확인 (가장 큰 위험 요소)
2. 서버: 매니페스트, Range, song_id 다운로드 추가
3. core: api + sync + tree + 차트 읽기 → 다운로드 없이 목록·차트까지 동작
4. fetcher + cache: 파일 단위/곡 단위 다운로드
5. 트레이 앱 UI, 자동 시작, 설치기(WinFsp 포함)
6. Linux FUSE, 이후 macOS 검토
