# EBMS Client

EBMS 서버의 곡을 가상 드라이브로 보여주고, 구동기(beatoraja, LR2, Qwilight 등)가 파일을 읽을 때 에셋을 받아오는 Windows 전용 클라이언트입니다. 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)를 보세요.

## 구성

| 크레이트 | 내용 |
| --- | --- |
| `ebms-core` | 서버 API, 로그인·세션키, 설정, 동기화(차트·매니페스트·사전 청크), 가상 트리, 요청 시 다운로드, 캐시 |
| `ebms-vfs-winfsp` | Windows WinFsp 백엔드 |
| `src-tauri` (`ebms-app`) | 앱과 설정 창. 화면은 `ui/` (빌드 단계 없는 HTML·JS) |

## 앱

```bash
cargo run -p ebms-app   # 설정 창을 열고, 창을 닫으면 종료
```

설정 창에서 서버 추가·삭제(서버는 하나만 연결), 로그인·로그아웃, 계정·로그인 수단(Google, Discord)·남은 티켓·이번 달 사전 다운로드 사용량을 봅니다. "다른 계정 연결"은 브라우저로 서버의 `/account`를 엽니다. 앱은 설정 창이 떠 있는 동안만 돌고, 창을 닫으면 가상 드라이브를 내리고 완전히 종료합니다(백그라운드 상주·자동 시작 없음). 이미 떠 있을 때 다시 실행하면 떠 있는 설정 창을 앞으로 가져옵니다.

앱 데이터(설정, 서버별 폴더의 인덱스·차트·캐시, 로그)는 실행 파일 옆 `data` 폴더에 둡니다. 그 폴더를 만들 수 없으면 `%APPDATA%\ebms`를 씁니다. `EBMS_DATA`로 위치를 직접 정할 수 있습니다.

켜질 때 서버 공지(`GET /api/notices`)를 확인해, 아직 확인하지 않은 공지가 있으면 공지 창을 띄웁니다. "확인"을 누른 공지는 다시 띄우지 않고(서버에서 고치면 다시 띄움), 그냥 닫으면 다음에 켤 때 다시 띄웁니다. 서버에 닿지 않으면 30초, 2분 뒤에 다시 확인합니다.

"개인 채보" 버튼으로 가진 채보 파일(.bms/.bme/.bml/.pms/.bmson)을 서버의 곡에 연결할 수 있습니다. 표에서 연결한 채보를 보고 뺄 수 있고, 곡을 검색해 고른 뒤 채보 파일을 고르면 연결됩니다. 연결한 채보는 가상 드라이브의 그 곡 폴더에 바로 보이고 키음·BGA는 그 곡 것을 씁니다. 채보는 서버에 올리지 않고 서버 데이터 폴더의 `personal`에 복사해 두므로 원본을 옮겨도 됩니다.

가상 드라이브에는 서버 이름 폴더 아래에 곡이 보입니다(`E:\<서버 이름>\00123 Artist - Title\`). WinFsp로 마운트하며, 기본 위치는 비어 있는 드라이브 문자 중 가장 뒤의 것(처음 고른 문자를 설정에 저장)이고 설정 창에서 바꿀 수 있습니다. 로그인돼 있으면 시작할 때 한 번 동기화합니다. 그 뒤로는 설정 창의 "지금 동기화"로 받습니다(동기화도 사전 다운로드 한도에서 빠집니다).

다른 서버(예: stage)로 바꾸려면 지금 서버를 삭제한 뒤 추가합니다. 로컬 데이터는 서버별 폴더에 따로 있어, 남겨 두면 다시 추가할 때 그대로 씁니다.

켜질 때 GitHub Releases의 최신 정식 릴리스(사전 릴리스 `stage` 제외)를 확인해, 지금보다 새 버전이면 업데이트할지 묻는 창을 띄웁니다. "업데이트"를 누르면 `ebms-windows-x64.zip`을 받아(GitHub가 준 SHA-256으로 확인) 실행 파일 폴더의 파일만 바꾸고 다시 시작합니다. 실행 파일 옆 `data` 폴더는 건드리지 않습니다. 실행 중인 exe는 `*.old`로 비켜 두었다가 다음 실행 때 지웁니다. 새 버전을 배포하려면 `Cargo.toml`의 workspace 버전을 올려 release에 병합합니다. 개발 빌드(debug)에서는 확인하지 않습니다.

## 지원 플랫폼

Windows만 지원합니다. 다른 OS에서도 코어 테스트와 앱 빌드는 되지만(Linux는 `libwebkit2gtk-4.1-dev` 필요) 가상 드라이브는 띄우지 않습니다. 키체인이 없는 환경에서는 세션키를 서버 데이터 폴더의 `session` 파일(권한 600)에 둡니다(`EBMS_NO_KEYRING`으로 강제).

Windows 마운트에는 [WinFsp](https://winfsp.dev/rel/) 설치가 필요합니다(없으면 앱은 뜨고 마운트만 실패합니다). Windows에서 빌드하려면 bindgen용 LLVM(`libclang`)이 필요합니다.

## 릴리스

`develop` → `stage` → `release` 순서로 병합합니다. `stage`에 병합하면 [Releases](https://github.com/alsrjs0725/ebms_client/releases)의 사전 릴리스 `stage`가 새 빌드로 바뀌고, `release`에 병합하면 정식 릴리스 `v<버전>`이 만들어집니다(Windows `.zip`, 실행 파일은 `ebms-app.exe`만). 정식 릴리스 전에 `Cargo.toml`의 `workspace.package.version`을 올려야 하며, 같은 버전이 이미 있으면 실패합니다.

## 개발

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 라이선스

EBMS 클라이언트 코드는 [MIT 라이선스](LICENSE)입니다. 다만 Windows 백엔드가 쓰는 `winfsp`·`winfsp-sys` 크레이트가 GPL-3.0이라, 이를 정적 링크한 배포용 실행 파일(`ebms-app.exe`)은 전체가 GPL-3.0 조건을 따릅니다. 해당 소스는 이 저장소의 릴리스 태그에 있습니다.

릴리스 zip에는 포함된 크레이트의 저작권·라이선스 전문을 모은 `THIRD_PARTY_LICENSES.html`이 함께 들어 있습니다. [cargo-about](https://github.com/EmbarkStudios/cargo-about)으로 만들며(`about.toml`, `about.hbs`), 허용 목록에 없는 라이선스의 크레이트가 들어오면 CI가 실패하므로 확인 후 `about.toml`에 추가합니다.

```bash
cargo install cargo-about --features cli
cargo about generate --workspace about.hbs -o THIRD_PARTY_LICENSES.html
```
