# 런처 앱 아이콘

런처는 시스템의 `org.gnome.desktop.interface icon-theme` 설정을 읽는다.
현재 선택한 테마의 `index.theme`에 선언된 폴더를 먼저 검색하고, 상속
테마와 기본 테마, `hicolor`, 테마 외 아이콘 순서로 대체한다.
동일 테마에서는 앱 표시용 48px에 가까운 크기를 우선한다.
[freedesktop 아이콘 검색 규칙](https://specifications.freedesktop.org/icon-theme/latest/)을
참고하며 GTK 런타임은 사용하지 않는다.

Papirus-Dark의 `48x48`처럼 다른 폴더를 가리키는 심볼릭 링크와 앱
별칭 링크를 정상적으로 따라간다. 임의의 폴더 순서나 파일 수 제한으로
작은 아이콘 또는 기본 아이콘을 먼저 고르지 않는다. 사용자 아이콘
경로와 `XDG_DATA_DIRS`를 반영하고, 앱이 명시한 절대 아이콘 경로는 유지한다.

조회 결과와 중복 디코딩은 한 번의 목록 갱신 안에서 캐시한다. 전체 테마
파일을 재귀 열거하지 않으며, 아이콘 로딩은 기존 작업 스레드에서 수행한다.
앱 아이콘은 48px, 이름은 시스템 글꼴의 13px로 표시한다. 아이콘은 96px로
한 번 디코딩해 높은 배율에서도 흐려지지 않도록 한다. 5열·600×486px 창에서
네 번째 행 아이콘이 절반 보이는 배치를 유지한다. 앱 사이 간격은 가로·세로
각각 6px 늘려 셀 간격을 112×116px로 하고, 포커스 영역과 안쪽 8px 여백은
유지한다. 제어센터의 연결 이름
크기·상태바 아이콘·커서·투명도는 변경하지 않는다.

```sh
cargo test --release --locked --offline
cargo clippy --release --locked --offline -- -D warnings
# Papirus-Dark가 설치된 실제 시스템에서 경로와 픽셀 확인
cargo test --release --locked --offline --bin lucy-shell \
  launcher::icon_theme::tests::installed_papirus_application_icons_resolve_and_decode \
  -- --ignored --nocapture
```

실기 검사는 Nautilus·Firefox·Foot의 선택 파일과 디코딩 결과를 확인하고,
실제 앱 목록에도 같은 Papirus 픽셀이 들어갔는지 비교한다. 테마에 없는
앱과 앱이 절대 경로를 지정한 경우에는 원래 아이콘이 표시될 수 있다.
