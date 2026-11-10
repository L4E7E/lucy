# 시스템 모니터

오른쪽 상태바의 온도·CPU·RAM·디스크 영역을 클릭하면 기존 오른쪽 바의
`wl_surface`가 제어센터와 같은 480×500px 패널로 확장된다. 제어센터의
리퀴드 글래스·블러·열기/닫기 애니메이션·바깥 클릭 처리를 재사용한다.
`lucy-shell system` 또는 `lucy-shell monitor` 명령으로도 열 수 있다.

- CPU: 실제 논리 스레드 수만큼 세로 게이지. 전체 사용률·CPU 이름·온도를
  표시하고, 스레드에 마우스를 올리면 해당 번호와 사용률을 표시한다.
- GPU: 장치별 사용률·온도·VRAM 사용량/전체 용량. AMD APU의 작은 전용
  예약 VRAM은 `VRAM · 예약`으로 구분한다. 공유 RAM 전체를 VRAM으로 오인하지 않는다.
- RAM: `MemTotal - MemAvailable` 기준 사용량/전체 용량과 퍼센트.
- 디스크: 루트 파일시스템의 실제 형식·마운트 위치·사용량/전체 용량·퍼센트.
  Btrfs 서브볼륨을 각각 독립 디스크로 중복 합산하지 않는다.

CPU/GPU는 패널이 열려 있을 때만 작업 스레드에서 500ms마다 읽고, RAM과
디스크는 2초마다 읽는다. 최초 CPU 사용률은 두 샘플이 모이기 전까지 `—`다.
EGL 경로에서는 배경·글자·게이지 트랙을 캐시하고, 변하는 게이지 채움만 작은
OpenGL 사각형으로 그린다. 기존 프레임 콜백으로 240ms 보간하므로 애니메이션
프레임마다 Cairo 전체 그리기·텍스처 업로드를 반복하지 않는다. 갱신 사이에는
추가 프레임 요청을 멈추며, SHM 폴백은 기존 Cairo 그리기를 유지한다.
닫거나 다른 페이지로 이동하면 작업 스레드는 채널 대기 상태로 전환하고 GPU
라이브러리를 해제한다. 측정할 수 없는 값은 0%로 꾸미지 않고 `—`로 표시한다.

AMD는 sysfs, NVIDIA는 선택적으로 동적 로드한 NVML을 사용한다. NVIDIA
드라이버/라이브러리가 없어도 다른 항목은 동작한다. `nvidia-smi`나 셸을
반복 실행하지 않는다. GPU의 `power/runtime_status`가 `active`일 때만 장치
조회하며, 이미 절전 중인 장치는 `절전 중`으로 표시한다. 센서·시스템 설정을
변경하지 않으며 새 패키지나 Cargo 의존성을 추가하지 않는다.
NVML ABI와 초기화는 [공식 조회 API](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html)와
[초기화 문서](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlInitializationAndCleanup.html)를 따른다.
조회와 화면 표시에 드는 비용까지 0인 것은 아니다. 특히 화면 출력·다른 앱이
이미 GPU를 사용 중이면 실행 상태만으로 추가 소비 전력을 분리할 수 없다.

```sh
cargo test --release --locked --offline
cargo clippy --release --locked --offline -- -D warnings
# 읽기 전용 미리보기: 원래 바 표면 재사용, 상세 수치 갱신/중지 검증
cargo test --release --locked --offline --test expansion -- --ignored --nocapture
# Niri 가상 포인터로 연결된 출력별 시스템 정보 영역의 실제 클릭도 검증
LUCY_TEST_INPUT=1 LUCY_TEST_MONITOR_ONLY=1 \
  cargo test --release --locked --offline --test expansion -- --ignored --nocapture
# 설치본: 닫힘 → 열림 → 닫힘 상태의 CPU/PSS·샘플·그리기 횟수 비교
python3 scripts/benchmark-monitor.py --seconds 10
# 실행 파일 간 비교: 같은 출력의 입력 없는 독립 미리보기 사용
python3 scripts/benchmark-monitor.py --seconds 10 --binary target/release/lucy-shell
```

벤치마크 CPU 값은 논리 코어 1개 기준이며 `cpu_machine_percent`는 전체 논리
코어 수로 나눈 값이다. PSS는 공유 매핑을 비례 배분한 메모리다. 미리보기와
실행 중인 서비스의 절대 메모리를 직접 비교하지 말고 같은 실행 방식으로
전후 비교한다. 측정 중 다른 팝업을 열거나 바깥을 클릭하면 결과를 폐기한다.
