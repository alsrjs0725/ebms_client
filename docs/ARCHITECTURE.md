# EBMS Client 설계

> 상태: 초안 v2 (2026-10-06). 서버 `alsrjs0725/ebms_server@main` 기준.

## 1. 목표

사용자가 곡을 찾아 받지 않는다. 클라이언트가 서버 전체 곡을 **가상 드라이브**로 보여주고, beatoraja 같은 구동기가 파일을 읽는 순간 필요한 에셋을 받아온다.

| 대상 | 언제 받나 | 디스크 |
| --- | --- | --- |
| 차트(.bms/.bme/.bml/.pms) | 서버 연결 시 전부 미리 동기화 | 로컬 보관 (작음) |
| 곡 에셋(wav/ogg/bmp/png/mp4…) | 구동기가 파일을 열 때 | 캐시 (용량 제한, 오래된 것부터 삭제) |

구동기 입장에서는 모든 곡이 이미 설치된 일반 폴더로 보인다.

**최우선 원칙: 가볍고 조용하게.** 처음 한 번 설정한 뒤에는 사용자가 클라이언트를 의식할 일이 없어야 한다. 알림, 팝업, 트레이 아이콘을 쓰지 않는다.

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
5. **백그라운드 상주**: 로그인 시 창 없이 자동 실행. 알림·팝업·트레이 아이콘 없음. 오류는 로그에만 남기고 조용히 재시도.
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
| UI | 설정 창 하나뿐이라 프레임워크 없이 TypeScript + Vite, 최소 CSS |

웹뷰는 설정 창을 열 때만 만들고 닫으면 해제한다. 평소에는 Rust 프로세스 하나만 돈다.

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
 src-tauri (상주 프로세스 + 설정 창) ── ui (TS)
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

다운로드 진행은 따로 표시하지 않는다. 구동기 입장에서는 파일 읽기가 조금 오래 걸리는 것으로 보일 뿐이다.

## 7. 서버에 필요한 변경

> 서버에 반영이 끝난 항목은 이 절에서 삭제한다. 남아 있는 항목은 아직 반영되지 않은 것이다.

| 필요 | 이유 | 이슈 |
| --- | --- | --- |
| 매니페스트에 차트 sha256 ↔ 경로 매핑 | 가상 폴더의 차트 파일을 로컬 청크와 연결. 나중에 기존 곡에 연결된 차트는 원래 파일명이 없음 | [#10](https://github.com/alsrjs0725/ebms_server/issues/10) |

임시 우회: 매니페스트 `files`의 (size, crc32)를 청크 zip 항목과 맞춰 매핑하고, 맞지 않는 차트는 `{sha256}{ext}` 이름으로 보여준다.

## 8. 저장소 구조

```
ebms_client/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ ebms-core/src/{api,sync,bms,tree,fetcher,cache,vfs}/
│  ├─ ebms-vfs-winfsp/        # Windows 백엔드
│  ├─ ebms-vfs-fuse/          # Linux/macOS 백엔드
│  └─ ebms-cli/
├─ src-tauri/                 # 상주 프로세스 + 설정 창
├─ ui/                        # 설정 창 (TS)
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
- 로그인 시 자동 실행, 업데이트는 다음 실행 때 조용히 적용

## 11. 구현 순서

1. core: api + sync + index + tree + fetcher + cache, CLI로 검증 (서버 반영 완료로 바로 시작)
2. Linux FUSE 백엔드: 같은 코어를 실제 마운트로 검증
3. **Windows WinFsp 백엔드 + beatoraja 확인** (가장 큰 위험 요소. Windows 실기 필요)
4. 상주 프로세스, 설정 창, 자동 시작, 설치기(WinFsp 포함)
5. macOS 검토
