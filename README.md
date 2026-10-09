# EBMS Client

EBMS 서버의 곡을 가상 드라이브로 보여주고, 구동기(beatoraja 등)가 파일을 읽을 때 에셋을 받아오는 클라이언트입니다. 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)를 보세요.

## 구성

| 크레이트 | 내용 |
| --- | --- |
| `ebms-core` | 서버 API, 로그인·세션키, 서버 목록, 동기화(차트·매니페스트·사전 청크), 가상 트리, 요청 시 다운로드, 캐시 |
| `ebms-vfs-fuse` | Linux FUSE 백엔드 |
| `ebms-vfs-winfsp` | Windows WinFsp 백엔드 |
| `ebms-cli` | 터미널에서 동기화·조회·마운트 (`ebms`) |
| `src-tauri` (`ebms-app`) | 상주 앱과 설정 창. 화면은 `ui/` (빌드 단계 없는 HTML·JS) |

## 앱

```bash
cargo run -p ebms-app                 # 설정 창을 열고 상주
cargo run -p ebms-app -- --background # 창 없이 상주 (자동 시작용, 서버가 없으면 창을 연다)
```

설정 창에서 서버 추가·삭제, 서버별 로그인·로그아웃, 계정·로그인 수단(Google, Discord)·남은 티켓·이번 달 사전 다운로드 사용량을 봅니다. "다른 계정 연결"은 브라우저로 서버의 `/account`를 엽니다. 창을 닫아도 앱은 계속 돌고, 종료는 설정 창의 "EBMS 종료"로 합니다. 이미 떠 있을 때 다시 실행하면 설정 창이 열립니다.

가상 드라이브는 서버마다 최상위 폴더 하나로 보입니다(`E:\<서버 이름>\00123 Artist - Title\`). Windows는 WinFsp, Linux는 FUSE로 마운트합니다. 기본 위치는 Windows는 비어 있는 드라이브 문자 중 가장 뒤의 것(처음 고른 문자를 설정에 저장), Linux는 `$XDG_RUNTIME_DIR/ebms`이고 설정 창에서 바꿀 수 있습니다. macOS는 아직 마운트하지 않습니다. 시작할 때와 30분마다 로그인된 서버를 모두 동기화합니다.

Linux 빌드에는 웹뷰 개발 패키지가 필요합니다: `sudo apt-get install libwebkit2gtk-4.1-dev`.

## 사용 (개발용 CLI)

```bash
ebms() { cargo run -q -p ebms-cli -- "$@"; }

ebms server add http://localhost:8000      # 서버 추가 (여러 개 가능)
ebms login                                 # 브라우저로 로그인, 세션키는 OS 키체인에 저장
ebms whoami                                # 계정, 남은 티켓, 이번 달 사전 다운로드 사용량
ebms sync                                  # 차트·매니페스트·사전 청크 동기화 (사전 다운로드)
ebms ls                                    # 곡 폴더 목록
ebms cat "00001 Artist - Title/bgm01.wav" > out.wav
ebms mount E:                              # 서버별 폴더로 마운트 (Linux는 폴더 경로, Ctrl-C로 해제)
ebms logout
```

서버가 여러 개면 `--server <id|이름|주소>`(또는 `EBMS_SERVER`)로 대상을 고릅니다. 앱 데이터 폴더는 OS 기본 위치이고 `--data`(또는 `EBMS_DATA`)로 바꿀 수 있습니다. 키체인이 없는 환경에서는 세션키를 서버 데이터 폴더의 `session` 파일(권한 600)에 둡니다(`--no-keyring`으로 강제).

Windows 마운트에는 [WinFsp](https://winfsp.dev/rel/) 설치가 필요합니다(없으면 앱은 뜨고 마운트만 실패합니다). Windows에서 빌드하려면 bindgen용 LLVM(`libclang`)이 필요합니다. Linux 마운트에는 FUSE(`/dev/fuse`, `fuse3` 패키지)가 필요합니다.

## 릴리스

`develop` → `stage` → `release` 순서로 병합합니다. `stage`에 병합하면 [Releases](https://github.com/alsrjs0725/ebms_client/releases)의 사전 릴리스 `stage`가 새 빌드로 바뀌고, `release`에 병합하면 정식 릴리스 `v<버전>`이 만들어집니다(Windows `.zip`, Linux `.tar.gz`). 정식 릴리스 전에 `Cargo.toml`의 `workspace.package.version`을 올려야 하며, 같은 버전이 이미 있으면 실패합니다.

## 개발

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
