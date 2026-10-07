# EBMS Client

EBMS 서버의 곡을 가상 드라이브로 보여주고, 구동기(beatoraja 등)가 파일을 읽을 때 에셋을 받아오는 클라이언트입니다. 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)를 보세요.

## 구성

| 크레이트 | 내용 |
| --- | --- |
| `ebms-core` | 서버 API, 로그인·세션키, 서버 목록, 차트·매니페스트 동기화, 가상 트리, 요청 시 다운로드, 캐시 |
| `ebms-vfs-fuse` | Linux FUSE 백엔드 |
| `ebms-cli` | 터미널에서 동기화·조회·마운트 (`ebms`) |

## 사용 (개발용 CLI)

```bash
ebms() { cargo run -q -p ebms-cli -- "$@"; }

ebms server add http://localhost:8000      # 서버 추가 (여러 개 가능)
ebms login                                 # 브라우저로 로그인, 세션키는 OS 키체인에 저장
ebms whoami                                # 계정, 남은 티켓, 이번 달 사전 다운로드 사용량
ebms sync                                  # 차트 청크·매니페스트 동기화
ebms ls                                    # 곡 폴더 목록
ebms cat "00001 Artist - Title/bgm01.wav" > out.wav
ebms mount ~/ebms                          # Linux: 가상 드라이브로 마운트 (Ctrl-C로 해제)
ebms logout
```

서버가 여러 개면 `--server <id|이름|주소>`(또는 `EBMS_SERVER`)로 대상을 고릅니다. 앱 데이터 폴더는 OS 기본 위치이고 `--data`(또는 `EBMS_DATA`)로 바꿀 수 있습니다. 키체인이 없는 환경에서는 세션키를 서버 데이터 폴더의 `session` 파일(권한 600)에 둡니다(`--no-keyring`으로 강제).

Linux 마운트에는 FUSE(`/dev/fuse`, `fuse3` 패키지)가 필요합니다.

## 개발

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
