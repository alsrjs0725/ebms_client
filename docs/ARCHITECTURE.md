# EBMS Client 설계

> 상태: 초안 v2 (2026-10-06). 서버 `alsrjs0725/ebms_server@main` 기준.

## 1. 목표

사용자가 곡을 찾아 받지 않는다. 클라이언트가 서버 전체 곡을 **가상 드라이브**로 보여주고, beatoraja 같은 구동기가 파일을 읽는 순간 필요한 에셋을 받아온다.

| 대상 | 언제 받나 | 디스크 |
| --- | --- | --- |
| 차트(.bms/.bme/.bml/.pms) | 사전 다운로드(= 동기화) 때 전부 | 로컬 보관 (작음) |
| 사전 파일(배너·스테이지파일·프리뷰) | 사전 다운로드(= 동기화) 때 전부 | 로컬 보관 (사전 청크) |
| 플레이 파일(키음·BGA…) | 구동기가 파일을 열 때 곡 zip 전체 | 캐시 (용량 제한, 오래된 것부터 삭제) |

구동기 입장에서는 모든 곡이 이미 설치된 일반 폴더로 보인다.

**최우선 원칙: 가볍고 조용하게.** 처음 한 번 설정한 뒤에는 사용자가 클라이언트를 의식할 일이 없어야 한다. 알림, 팝업, 트레이 아이콘을 쓰지 않는다.

```
E:\ (또는 ~/ebms 마운트 지점)
└─ <서버 이름>\          ← 로그인한 EBMS 서버마다 하나
   └─ 00123 Artist - Title\
      ├─ _7a.bme        ← 로컬 차트, 즉시 읽힘
      ├─ bgm01.ogg      ← 크기·이름만 보임, 열면 다운로드
      └─ stagefile.png
```

## 2. 요구사항

### 기능
1. **자동 동기화(= 사전 다운로드)**: 앱 시작·서버 연결 시, 이후 주기적으로 차트 청크·매니페스트·사전 청크의 해시 비교 → 바뀐 청크만 받기 → 검증 → 인덱스·가상 트리 갱신.
2. **가상 드라이브**: 곡 폴더/파일 트리를 읽기 전용으로 마운트. 목록·크기 조회(`ls`, `stat`)는 네트워크 없이 응답.
3. **요청 시 다운로드**: 파일 `open/read` 시 해당 데이터를 받아 캐시 후 응답. 받는 동안 읽기 요청은 대기.
4. **캐시 관리**: 용량 한도(기본 20GB), LRU 삭제, 곡 단위 "항상 보관" 고정, 수동 비우기.
5. **백그라운드 상주**: 로그인 시 창 없이 자동 실행. 알림·팝업·트레이 아이콘 없음. 오류는 로그에만 남기고 조용히 재시도. 예외로, 켜질 때 서버 공지(`GET /api/notices`) 중 확인하지 않은 것이 있으면 공지 창을 띄운다.
6. **설정 창**: 첫 실행 때만 자동으로 열림(서버 URL, 마운트 위치, 캐시 한도, beatoraja 곡 폴더 등록 안내). 이후에는 앱을 다시 실행하면 열리고, 상태(연결, 마지막 동기화, 캐시 사용량)도 여기서만 보여준다.

### 비기능
- 상주 중 메모리 수십 MB 이하, 유휴 시 CPU·네트워크 사용 거의 없음. 설정 창(웹뷰)은 닫으면 해제.
- Windows 우선, Linux 지원, macOS는 PoC 후 결정 (§5 참고).
- 곡 고르는 화면에서 스크롤만 해도 곡 전체가 받아지는 일이 없어야 함 (§6).
- 오프라인이면 캐시에 있는 곡은 그대로 플레이 가능, 없는 파일은 읽기 오류.
- 다운로드 중 앱이 죽어도 캐시가 깨지지 않음 (임시 파일 → 검증 → rename).

## 3. 기술 스택

**Tauri 2 + Rust 코어 + 최소 TypeScript 설정 창** (가상 파일시스템 바인딩이 Rust에 가장 잘 갖춰져 있고, 상주 시 가벼움)

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
| 앱 | Tauri 플러그인 `single-instance`, `autostart`, `updater`(조용히 적용), `dialog` |
| UI | 설정 창 하나뿐이라 프레임워크·빌드 단계 없이 HTML + JS, 최소 CSS |

웹뷰는 설정 창을 열 때만 만들고 닫으면 해제한다. 평소에는 Rust 프로세스 하나만 돈다.

## 4. 아키텍처

```
 beatoraja / LR2 ──파일 읽기──▶ 가상 드라이브
                                  │
┌─────────────────────────────────▼───────────────┐
│ vfs       OS 백엔드(WinFsp / FUSE) → 공통 trait  │
├──────────────────────────────────────────────────┤
│ tree      곡/파일 트리 (index.sqlite에서 메모리로) │
│ fetcher   요청 합치기, 사전/플레이 다운로드       │
│ cache     에셋 캐시, LRU, 고정, 무결성            │
│ sync      차트·매니페스트·사전 청크 동기화        │
│ api       서버 HTTP 클라이언트 (Bearer 세션키)    │
│ auth      브라우저 로그인 (루프백 + PKCE)         │
│ bms       차트 파서·해시                          │
└──────────────────────────────────────────────────┘
        ▲ 상태/이벤트
 src-tauri (상주 프로세스 + 설정 창) ── ui (TS)
 ebms-cli  (마운트·동기화를 터미널에서, 테스트용)
```

원칙
- 로직은 전부 `ebms-core`. FS 백엔드는 `ReadOnlyFs` trait(`lookup`, `getattr`, `readdir`, `open`, `read_async`) 하나만 쓴다.
- FS 콜백 스레드는 절대 네트워크를 직접 기다리지 않는다. `read_async`로 `fetcher`에 맡기고 바로 돌아오며, 다운로드가 끝나면 완료 콜백이 응답한다. 같은 파일 동시 요청은 한 번만 받는다.

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
| 선곡 화면 이동 | `#BANNER`, `#STAGEFILE`, `preview*.ogg` | 로컬이라 즉시 |
| 플레이 시작 | 키음·BGA 수백 개 | **곡 전체**를 한 번에 받기 |

다운로드는 두 단계다: **사전 다운로드(= 동기화)**와 **플레이 다운로드**. 서버가 곡 등록 때 파일마다 `kind`(`pre`/`play`)를 정해 매니페스트에 싣는다.
1. **사전 파일**(`pre`: 차트·배너·스테이지파일·프리뷰): 동기화 때 받는다. 차트는 차트 청크, 나머지는 사전 청크(`/api/pre/asset/{id}`, 32곡 단위 무압축 zip, 항목 이름 `{song_id}/{경로}`)로 받아 `pre/`에 보관하고 캐시처럼 지우지 않는다. 가상 트리는 매니페스트의 크기·crc32가 같은 항목만 사전 청크에서 읽는다. 이번 달 사전 다운로드 사용량에 더해지고, 한도를 넘으면 서버가 감속한다. 사전 청크에 아직 없는 파일(서버가 만드는 중, API 3 미만 서버)은 `/api/pre/song/{id}/file?path=`로 그 파일만 받아 캐시에 넣는다.
2. **플레이 파일**(`play`: 키음·BGA): 처음 열 때 `/api/play/song/{id}`로 곡 zip 전체를 받는다. 티켓 1개를 쓴다. 나머지 파일 요청은 이 다운로드를 기다린다.
3. 티켓이 없으면 서버가 `429` + `Retry-After`를 준다. 그 파일 읽기는 오류로 돌리고, `Retry-After` 동안 그 곡의 플레이 다운로드를 다시 요청하지 않는다. 알림 없이 로그에만 남긴다.

다운로드 진행은 따로 표시하지 않는다. 구동기 입장에서는 파일 읽기가 조금 오래 걸리는 것으로 보일 뿐이다.

## 7. 로그인과 여러 서버

한 사용자가 여러 EBMS 서버에 로그인한다. 계정은 서버마다 따로이고 서버끼리는 통신하지 않는다.

- 서버 목록은 `config.toml`의 `servers = [{id, url, name}]`. 로컬 데이터는 서버별 폴더에 따로 둔다.
- 로그인: `127.0.0.1` 빈 포트에서 수신을 열고 브라우저로 `/auth/client/authorize`를 연다(PKCE `S256`). 서버가 1회용 `code`를 붙여 루프백으로 돌려보내면 `state`를 확인하고 `code` + `code_verifier`를 `/api/auth/client/token`에 보내 세션키를 받는다.
- 세션키는 OS 키체인(`keyring`)에 서버 주소별로 저장한다. 키체인을 못 쓰면 서버 데이터 폴더의 `session` 파일(권한 600).
- 모든 API 요청에 `Authorization: Bearer <세션키>`. `401`을 받으면 조용히 로그아웃 상태로 바꾸고(`Api::needs_login`) 설정 창에만 "다시 로그인 필요"를 보여준다.
- CLI: `ebms server add|list|remove`, `ebms login|logout|whoami`, 나머지 명령은 `--server`로 대상을 고른다(서버가 하나면 생략). `mount`는 `--server`가 없으면 모든 서버를 마운트한다.
- 가상 드라이브는 서버별 최상위 폴더로 합친다(`drive::Drive`): `E:\<서버 이름>\00123 Artist - Title\`. 같은 곡이 여러 서버에 있어도 합치지 않는다. 한 서버의 세션이 만료되거나 서버가 꺼져도 다른 서버는 그대로이고, 받아 둔 파일은 계속 읽힌다.
- 상주 앱은 `hub::Hub`로 서버 전체를 관리한다(추가·삭제, 로그인, 서버별 동기화와 상태).

### 설정 창

| 항목 | 내용 |
| --- | --- |
| 서버 | 추가(주소, 이름 선택, `/api/version` 확인) · 삭제(로그아웃 후 목록과 드라이브에서 뺌, 로컬 데이터는 남김) |
| 로그인 | 서버별 로그인/로그아웃 버튼. 상태: 로그인됨 · 다시 로그인 필요(401) · 로그아웃 상태 |
| 계정 | `/api/me`: 이름, 로그인 수단(Google, Discord), 플레이 티켓, 이번 달 사전 다운로드 사용량·감속 여부 |
| 다른 계정 연결 | 브라우저로 서버의 `/account`를 연다 |
| 동기화 | 마지막 동기화 시각 또는 오류, "지금 동기화" |
| 가상 드라이브 | 마운트 위치와 상태 |

화면은 빌드 단계 없는 HTML·JS(`ui/`)다. 설정 창 하나뿐이라 TypeScript·Vite를 쓰지 않고 Node 없이 빌드되게 했다.

## 8. 서버에 필요한 변경

> 서버에 반영이 끝난 항목은 이 절에서 삭제한다. 남아 있는 항목은 아직 반영되지 않은 것이다.

| 필요 | 이유 | 이슈 |
| --- | --- | --- |
| 매니페스트에 차트 sha256 ↔ 경로 매핑 | 가상 폴더의 차트 파일을 로컬 청크와 연결. 나중에 기존 곡에 연결된 차트는 원래 파일명이 없음 | [#10](https://github.com/alsrjs0725/ebms_server/issues/10) |

임시 우회: 매니페스트 `files`의 (size, crc32)를 청크 zip 항목과 맞춰 매핑하고, 맞지 않는 차트는 `{sha256}{ext}` 이름으로 보여준다.

## 9. 저장소 구조

```
ebms_client/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ ebms-core/src/{api,sync,bms,tree,fetcher,cache,vfs}/
│  ├─ ebms-vfs-winfsp/        # Windows 백엔드
│  ├─ ebms-vfs-fuse/          # Linux/macOS 백엔드
│  └─ ebms-cli/
├─ src-tauri/                 # 상주 프로세스 + 설정 창 (ebms-app)
├─ ui/                        # 설정 창 (HTML·JS, 빌드 단계 없음)
├─ docs/
└─ .github/workflows/
```

## 10. 로컬 데이터

```
<앱 데이터>/
├─ config.toml                 # 서버 목록, 마운트 위치
├─ logs/ebms.<날짜>.log        # 상주 앱 로그 (하루 단위, 최근 7일). 설정 창에서 폴더를 열 수 있음
└─ servers/<server_id>/
   ├─ index.sqlite             # chunk, chart, song, song_file, cache_entry
   ├─ charts/                  # 청크 zip 원본 (가상 FS가 차트를 여기서 읽음)
   ├─ cache/<song_id>/         # 받은 에셋
   ├─ session                  # 세션키 (키체인을 못 쓸 때만)
   └─ tmp/
```

| 테이블 | 주요 컬럼 |
| --- | --- |
| `song` | id, folder_name, manifest_chunk |
| `song_file` | song_id, path, size, zip_offset, comp_size, crc32, kind(chart/light/heavy) |
| `chart` | sha256, md5, song_id, title, artist, … |
| `cache_entry` | song_id, path, size, last_access, pinned |

## 11. 기본값으로 정한 것

- Windows 마운트: 빈 드라이브 문자 자동 선택(설정에서 폴더 마운트로 변경 가능)
- 폴더명: `{song_id:05} {artist} - {title}` (ID 접두사로 이름 충돌 방지)
- 읽기 전용 마운트
- 캐시 한도 20GB (서버별)
- 자동 동기화: 시작 시 + 30분마다
- 로그인 시 자동 실행, 업데이트는 다음 실행 때 조용히 적용

## 12. 구현 순서

1. core: api + sync + index + tree + fetcher + cache, CLI로 검증 (서버 반영 완료로 바로 시작)
2. Linux FUSE 백엔드: 같은 코어를 실제 마운트로 검증
3. **Windows WinFsp 백엔드 + beatoraja 확인** (백엔드 완료: `ebms-vfs-winfsp`. 남음: Windows 실기에서 beatoraja 확인)
4. 상주 프로세스, 설정 창 (완료: Linux 마운트, 서버·로그인 관리) / 남음: 자동 시작, 설치기(WinFsp 포함)
5. macOS 검토
