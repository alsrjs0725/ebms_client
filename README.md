# EBMS Client

EBMS 서버의 곡을 가상 드라이브로 보여주고, 구동기(beatoraja, LR2, Qwilight 등)가 파일을 읽을 때 에셋을 받아오는 Windows 전용 클라이언트입니다. 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)를 보세요.

## 구성

| 크레이트 | 내용 |
| --- | --- |
| `ebms-core` | 서버 API, 로그인·세션키, 서버 목록, 동기화(차트·매니페스트·사전 청크), 가상 트리, 요청 시 다운로드, 캐시 |
| `ebms-vfs-fuse` | Linux FUSE 백엔드 (지원 중단, 개발·테스트용으로만 남김) |
| `ebms-vfs-winfsp` | Windows WinFsp 백엔드 |
| `ebms-cli` | 터미널에서 동기화·조회·마운트 (`ebms`) |
| `src-tauri` (`ebms-app`) | 앱과 설정 창. 화면은 `ui/` (빌드 단계 없는 HTML·JS) |

## 앱

```bash
cargo run -p ebms-app   # 설정 창을 열고, 창을 닫으면 종료
```

설정 창에서 서버 추가·삭제, 서버별 로그인·로그아웃, 계정·로그인 수단(Google, Discord)·남은 티켓·이번 달 사전 다운로드 사용량을 봅니다. "다른 계정 연결"은 브라우저로 서버의 `/account`를 엽니다. 앱은 설정 창이 떠 있는 동안만 돌고, 창을 닫으면 가상 드라이브를 내리고 완전히 종료합니다(백그라운드 상주·자동 시작 없음). 이미 떠 있을 때 다시 실행하면 떠 있는 설정 창을 앞으로 가져옵니다.

앱 데이터(설정, 서버별 인덱스·차트·캐시, 로그)는 실행 파일 옆 `data` 폴더에 둡니다. 그 폴더를 만들 수 없으면 예전 위치(`%APPDATA%\ebms`)를 씁니다. 예전 위치에 데이터가 있고 새 폴더에 설정이 없으면 처음 켤 때 옮기고 예전 폴더는 지웁니다. 같은 드라이브면 통째로 옮기고, 드라이브가 다르면 설정·공지 확인 기록·세션 파일만 옮겨 인덱스·차트는 다시 동기화하고 캐시는 다시 받습니다. `EBMS_DATA`로 위치를 직접 정할 수 있습니다.

켜질 때 서버마다 공지(`GET /api/notices`)를 확인해, 아직 확인하지 않은 공지가 있으면 공지 창을 띄웁니다. "확인"을 누른 공지는 다시 띄우지 않고(서버에서 고치면 다시 띄움), 그냥 닫으면 다음에 켤 때 다시 띄웁니다. 닿지 않은 서버는 30초, 2분 뒤에 다시 확인합니다.

서버마다 "개인 채보" 버튼으로 가진 채보 파일(.bms/.bme/.bml/.pms/.bmson)을 서버의 곡에 연결할 수 있습니다. 표에서 연결한 채보를 보고 뺄 수 있고, 곡을 검색해 고른 뒤 채보 파일을 고르면 연결됩니다. 연결한 채보는 가상 드라이브의 그 곡 폴더에 바로 보이고 키음·BGA는 그 곡 것을 씁니다. 채보는 서버에 올리지 않고 서버 데이터 폴더의 `personal`에 복사해 두므로 원본을 옮겨도 됩니다.

가상 드라이브는 서버마다 최상위 폴더 하나로 보입니다(`E:\<서버 이름>\00123 Artist - Title\`). WinFsp로 마운트하며, 기본 위치는 비어 있는 드라이브 문자 중 가장 뒤의 것(처음 고른 문자를 설정에 저장)이고 설정 창에서 바꿀 수 있습니다. 시작할 때와 30분마다 로그인된 서버를 모두 동기화합니다.

켜질 때 GitHub Releases의 최신 정식 릴리스(사전 릴리스 `stage` 제외)를 확인해, 지금보다 새 버전이면 업데이트할지 묻는 창을 띄웁니다. "업데이트"를 누르면 `ebms-windows-x64.zip`을 받아(GitHub가 준 SHA-256으로 확인) 실행 파일 폴더의 파일만 바꾸고 다시 시작합니다. 실행 파일 옆 `data` 폴더는 건드리지 않습니다. 실행 중인 exe는 `*.old`로 비켜 두었다가 다음 실행 때 지웁니다. 새 버전을 배포하려면 `Cargo.toml`의 workspace 버전을 올려 release에 병합합니다. 개발 빌드(debug)에서는 확인하지 않습니다.

## 지원 플랫폼

Windows만 지원합니다. Linux 클라이언트는 지원을 중단했고 릴리스에 포함하지 않습니다. Linux FUSE 백엔드(`ebms-vfs-fuse`)는 코어를 Linux에서 개발·테스트할 수 있게 코드로만 남겨 두었으며(`$XDG_RUNTIME_DIR/ebms`에 마운트, `libwebkit2gtk-4.1-dev`·`fuse3` 필요), 동작을 보장하지 않습니다. macOS는 지원 계획이 없습니다.

## 사용 (개발용 CLI)

```bash
ebms() { cargo run -q -p ebms-cli -- "$@"; }

ebms server add http://localhost:8000      # 서버 추가 (여러 개 가능)
ebms login                                 # 브라우저로 로그인, 세션키는 OS 키체인에 저장
ebms whoami                                # 계정, 남은 티켓, 이번 달 사전 다운로드 사용량
ebms sync                                  # 차트·매니페스트·사전 청크 동기화 (사전 다운로드)
ebms tables [--select 1,2 | --all]         # 난이도표 목록과 사전 다운로드 범위(고른 표의 곡만 / 전곡)
ebms ls                                    # 곡 폴더 목록
ebms cat "00001 Artist - Title/bgm01.wav" > out.wav
ebms mount E:                              # 서버별 폴더로 마운트 (Ctrl-C로 해제)
ebms logout
```

서버가 여러 개면 `--server <id|이름|주소>`(또는 `EBMS_SERVER`)로 대상을 고릅니다. 앱 데이터 폴더는 실행 파일 옆 `data`이고 `--data`(또는 `EBMS_DATA`)로 바꿀 수 있습니다. 키체인이 없는 환경에서는 세션키를 서버 데이터 폴더의 `session` 파일(권한 600)에 둡니다(`--no-keyring`으로 강제).

Windows 마운트에는 [WinFsp](https://winfsp.dev/rel/) 설치가 필요합니다(없으면 앱은 뜨고 마운트만 실패합니다). Windows에서 빌드하려면 bindgen용 LLVM(`libclang`)이 필요합니다.

## 릴리스

`develop` → `stage` → `release` 순서로 병합합니다. `stage`에 병합하면 [Releases](https://github.com/alsrjs0725/ebms_client/releases)의 사전 릴리스 `stage`가 새 빌드로 바뀌고, `release`에 병합하면 정식 릴리스 `v<버전>`이 만들어집니다(Windows `.zip`, 실행 파일은 `ebms-app.exe`만). 정식 릴리스 전에 `Cargo.toml`의 `workspace.package.version`을 올려야 하며, 같은 버전이 이미 있으면 실패합니다.

## 개발

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
