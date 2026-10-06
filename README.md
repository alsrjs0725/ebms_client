# EBMS Client

EBMS 서버의 곡을 가상 드라이브로 보여주고, 구동기(beatoraja 등)가 파일을 읽을 때 에셋을 받아오는 클라이언트입니다. 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)를 보세요.

## 구성

| 크레이트 | 내용 |
| --- | --- |
| `ebms-core` | 서버 API, 차트·매니페스트 동기화, 가상 트리, 요청 시 다운로드, 캐시 |
| `ebms-vfs-fuse` | Linux FUSE 백엔드 |
| `ebms-cli` | 터미널에서 동기화·조회·마운트 (`ebms`) |

## 사용 (개발용 CLI)

```bash
export EBMS_SERVER=http://localhost:8000   # 서버 주소
export EBMS_DATA=./ebms-data               # 로컬 데이터 폴더

cargo run -p ebms-cli -- sync              # 차트 청크·매니페스트 동기화
cargo run -p ebms-cli -- ls                # 곡 폴더 목록
cargo run -p ebms-cli -- cat "00001 Artist - Title/bgm01.wav" > out.wav
cargo run -p ebms-cli -- mount ~/ebms      # Linux: 가상 드라이브로 마운트 (Ctrl-C로 해제)
```

Linux 마운트에는 FUSE(`/dev/fuse`, `fuse3` 패키지)가 필요합니다.

## 개발

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
