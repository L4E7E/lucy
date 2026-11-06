# Repository Guidelines

## 전체 규칙
- wayland 에 최적화된 rust 경량 desktop shell 개발.
- 낮은 메모리 점유율과 매우 빠른 반응속도를 구현.

## 프로젝트 구조와 모듈 구성

Lucy (`lucy-shell`)는 GTK3와 gtk-layer-shell을 사용하는 Rust 2021 기반 데스크톱 제어 프로그램입니다. 현재 Wayland용 오디오·NetworkManager 네트워크 팝업을 구현했으며, Niri에서 동작을 검증했습니다.

- `src/main.rs`: 실행 모드 분기, 오디오 제어, 공통 layer-shell 바인딩, 오디오 테스트.
- `src/network.rs`: 네트워크 상태·화면·인증·자식 프로세스 관리와 테스트.
- `src/style.css`: 민트 강조색과 반투명 배경을 사용하는 공통 스타일.
- `integration/`: 기존 설정에 병합할 Waybar·Niri 연동 예시.
- `scripts/install.sh`: 빌드 후 `~/.local/bin/lucy-shell`을 원자적으로 교체하는 설치 스크립트.
- `target/`: 버전 관리에서 제외하는 빌드 산출물.

## 빌드·테스트·개발 명령

시스템에 GTK3, gtk-layer-shell, NetworkManager의 `nmcli`, 오디오 제어용 `pactl`이 필요합니다. 프로젝트 루트에서 실행합니다.

```sh
cargo build --release --locked       # 실행 파일 빌드
cargo test --release --locked        # 단위 테스트와 가상 PTY 테스트
cargo fmt --check                    # Rust 코드 서식 검사
cargo clippy --release --locked -- -D warnings  # 경고까지 오류로 검사
./target/release/lucy-shell audio     # 오디오 팝업 실행
./target/release/lucy-shell network --vpn  # VPN 탭 실행
./scripts/install.sh                 # 사용자 실행 파일 설치
```

의존성이 캐시에 있으면 Cargo 명령에 `--offline`을 추가할 수 있습니다. 화면 실행에는 호환되는 Wayland 세션과 관련 데스크톱 서비스 접근 권한이 필요합니다.

## 코드 스타일과 이름 규칙

rustfmt 기본 규칙과 공백 4칸 들여쓰기를 따릅니다. 함수·모듈은 `snake_case`, 타입은 `PascalCase`로 작성합니다. 실행 파일 이름은 `lucy-shell`, 레이어 네임스페이스 접두사는 `lucy-`를 유지합니다. GTK 화면은 메인 스레드에서 갱신하고, 대기 시간이 발생하는 백엔드 작업은 별도 스레드에서 처리합니다. 이벤트 기반 감시 구조를 유지하고 팝업 종료 시 자식 프로세스를 정리합니다.
