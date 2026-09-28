# page-screen-capturer

루트 URL에서 시작해 같은 origin 안에서 링크로 도달 가능한 페이지를 크롤링하고,
각 페이지를 **데스크탑 / 모바일 전체 페이지(full-page)** 로 캡쳐해 경로별 디렉터리에 저장하는 CLI 도구입니다.
쿠키를 주입해 로그인이 필요한 페이지도 캡쳐할 수 있습니다.

## 요구 사항

- Rust (edition 2024)
- Chrome 또는 Chromium (자동 탐지, 못 찾으면 `--chrome <경로>`)

```sh
cargo build --release
```

## 사용법

```sh
# 기본: 데스크탑(1440x900) + 모바일(390x844 @2x) 캡쳐 → ./captures
page-screen-capturer https://example.com/

# 쿠키로 인증 세션 주입
page-screen-capturer https://app.example.com/ -c "session=abc123; csrftoken=xyz"

# 서브도메인과 공유되는 쿠키
page-screen-capturer https://app.example.com/ -c "sid=abc" --cookie-domain .example.com

# 브라우저에서 export한 쿠키 파일 사용
page-screen-capturer https://app.example.com/ --cookie-file cookies.json

# 범위/규모 제한
page-screen-capturer https://example.com/docs/ --stay-under-root --max-depth 3 --max-pages 50 -j 4

# 특정 경로 제외, 링크로 연결되지 않은 페이지 추가
page-screen-capturer https://example.com/ --exclude '^/admin/danger' --seed /hidden/page
```

로그 레벨은 `RUST_LOG=page_screen_capturer=debug` 로 조정할 수 있습니다.

### 쿠키 입력 형식

| 방식 | 형식 |
|---|---|
| `-c`, `--cookie` (반복 가능) | `name=value` 또는 `a=1; b=2` (DevTools의 `Cookie` 요청 헤더 값을 그대로 붙여넣기) |
| `--cookie-file` | JSON 배열 (EditThisCookie, Cookie-Editor, Puppeteer `page.cookies()` 등) |
| | Playwright `storageState` JSON (`{"cookies": [...]}`) |
| | Netscape `cookies.txt` |

`--cookie`로 넣은 쿠키는 기본적으로 루트 URL 호스트 전용 쿠키로 설정되며, `--cookie-domain`으로 도메인을 지정할 수 있습니다.
루트 페이지가 다른 경로(예: `/login`)로 리다이렉트되면 쿠키가 만료되었을 가능성이 있다는 경고를 출력합니다.

## 출력 구조

```
captures/
├── manifest.json            # 페이지별 URL, 최종 URL, HTTP 상태, 제목, 파일 경로, 건너뛴 이유, 오류
├── _root/                   # "/"
│   ├── desktop.png
│   └── mobile.png
├── about/
│   ├── desktop.png
│   ├── mobile.png
│   └── team/                # "/about/team"
│       ├── desktop.png
│       └── mobile.png
└── 검색/                    # "/%EA%B2%80%EC%83%89" (percent-decoding)
```

- `/about`과 `/about/`은 같은 페이지로 취급합니다.
- 쿼리스트링은 기본적으로 무시합니다. `--keep-query`를 주면 `search?q=a` → `search__q=a/` 처럼 별도 저장합니다.
- 파일시스템에서 쓸 수 없는 문자는 `_`로 바꾸고, 너무 긴 경로 조각은 해시를 붙여 줄입니다.

## 크롤링 규칙

- 루트 URL과 같은 origin(scheme + host + port)의 `<a href>`만 따라갑니다.
  루트가 `http→https`나 `www` 추가/제거로 리다이렉트되면 최종 origin을 범위로 사용합니다.
- 작업 큐 기반으로 최대 `-j`개 페이지를 동시에 처리합니다. 한 페이지가 끝나면 발견한 링크를 바로 큐에 넣고 다음 페이지를 투입합니다 (큐는 FIFO라 대체로 얕은 페이지부터 처리).
- 페이지마다 데스크탑/모바일 탭을 동시에 열어 로드·캡쳐하고, 두 디바이스에서 찾은 링크를 합칩니다 (모바일 전용 메뉴 대응).
- 탭마다 별도 창을 열어 모든 탭이 visible 상태로 렌더링됩니다 (백그라운드 탭은 lazy 이미지 로딩이 멈추기 때문).
- 리다이렉트된 페이지는 최종 경로에 저장하고, 이미 캡쳐한 페이지로 리다이렉트되면 건너뜁니다.
- 4xx/5xx 응답, HTML이 아닌 응답, 이미지/PDF 등의 파일 링크는 캡쳐하지 않습니다 (`--capture-errors`로 오류 페이지 포함).
- **세션 보호**: `logout`, `log-out`, `signout`, `sign_off` 같은 경로는 기본적으로 방문하지 않습니다 (`--no-default-excludes`로 해제).

## 캡쳐 방식

1. 디바이스별로 새 탭을 열고 뷰포트, device scale factor, 모바일/터치, User-Agent를 에뮬레이션합니다.
2. 페이지 로드 → 애니메이션/트랜지션 비활성화 → 끝까지 스크롤해 lazy-load 콘텐츠 로딩 → 이미지/웹폰트 대기 → `--wait-ms` 만큼 추가 대기.
3. 전체 페이지 캡쳐 (`--max-height`, 기본 20000 CSS px에서 잘림)
   - **모바일**: 맨 위부터 뷰포트 높이씩 **끝까지 스크롤하며** 각 위치에서 화면을 캡쳐해 이어 붙입니다.
     - 스크롤할 때마다 화면 안 이미지 로딩과 DOM 변경(등장 효과 등)이 멈출 때까지 기다린 뒤 캡쳐합니다.
     - `position: fixed` 요소는 위쪽에 붙은 것(헤더)은 첫 조각에만, 아래쪽에 붙은 것(하단 탭바, 플로팅 버튼)은
       마지막 조각에만 나오게 해서, 이어 붙인 이미지에서 헤더는 맨 위·탭바는 맨 아래에 한 번씩만 보입니다.
     - `sticky` 요소는 첫 조각 이후 원래 위치에 둡니다.
     - 스크롤하면 헤더가 접히는 등 콘텐츠가 밀리는 페이지는 이동량을 측정해 조각 위치를 보정합니다.
   - **데스크탑**: `captureBeyondViewport`로 한 번에 캡쳐합니다. 결과가 16000px을 넘으면 모바일과 같은 스크롤 방식으로 전환합니다.
     (Chrome은 GPU 텍스처 한계인 16384px을 넘는 영역을 한 번에 그리면 위쪽 내용이 반복된 이미지를 만듭니다.)

## 주요 옵션

| 옵션 | 기본값 | 설명 |
|---|---|---|
| `-o, --out` | `captures` | 저장 디렉터리 |
| `--devices` | `desktop,mobile` | 캡쳐할 디바이스 |
| `--max-depth` / `--max-pages` | `5` / `200` | 탐색 깊이 / 최대 방문 페이지 수 |
| `-j, --concurrency` | `4` | 동시 처리 페이지 수 (최대 탭 수 = concurrency × 디바이스 수) |
| `--include` / `--exclude` | | 경로 정규식 필터 (반복 가능) |
| `--wait-ms` | `1000` | 캡쳐 전 추가 대기 |
| `--timeout` | `30` | 페이지 로드 타임아웃(초) |
| `--format` / `--quality` | `png` / `85` | `png`, `jpeg`, `webp` (스크롤 캡쳐 시 webp는 무손실, 16383px 이하만 가능) |
| `--desktop-width/height` | `1440` / `900` | |
| `--mobile-width/height/scale` | `390` / `844` / `2` | |
| `--headful` | | 브라우저 창을 띄워 실행 (디버깅) |

전체 옵션은 `page-screen-capturer --help`를 참고하세요.

## 제약 사항

- 크롤러는 GET 링크만 방문하지만, GET 요청에 부수 효과가 있는 링크(예: `/items/1/delete`)가 있다면 `--exclude`로 제외하세요.
- `body` 대신 내부 스크롤 컨테이너(`height: 100vh; overflow: auto`)를 쓰는 레이아웃은 뷰포트 높이만 캡쳐됩니다.
- `onclick` 등 JavaScript로만 이동하는 링크는 발견하지 못합니다. 이런 페이지는 `--seed`로 추가하세요.
