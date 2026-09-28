use std::path::PathBuf;

use clap::builder::TypedValueParser;
use clap::{Parser, ValueEnum};

/// 루트 URL에서 시작해 같은 origin 내에서 접근 가능한 페이지를 크롤링하고,
/// 데스크탑/모바일 전체 페이지 스크린샷을 경로별로 저장합니다.
#[derive(Debug, Clone, Parser)]
#[command(name = "page-screen-capturer", version, about)]
pub struct Args {
    /// 크롤링을 시작할 루트 URL (예: https://example.com/)
    pub url: String,

    /// 스크린샷 저장 디렉터리
    #[arg(short, long, default_value = "captures")]
    pub out: PathBuf,

    /// 쿠키 직접 지정. `name=value` 또는 `a=1; b=2` 형식 (반복 가능)
    #[arg(short = 'c', long = "cookie", value_name = "COOKIE")]
    pub cookies: Vec<String>,

    /// 쿠키 파일 경로. JSON 배열, Playwright storageState, Netscape cookies.txt 지원
    #[arg(long, value_name = "PATH")]
    pub cookie_file: Option<PathBuf>,

    /// `--cookie`로 지정한 쿠키의 domain (예: `.example.com`, 서브도메인 공유 시).
    /// 생략하면 루트 URL의 호스트에만 설정됩니다.
    #[arg(long, value_name = "DOMAIN")]
    pub cookie_domain: Option<String>,

    /// 캡쳐할 디바이스
    #[arg(long, value_delimiter = ',', default_value = "desktop,mobile")]
    pub devices: Vec<DeviceKind>,

    /// 최대 링크 깊이 (루트 = 0)
    #[arg(long, default_value_t = 5)]
    pub max_depth: usize,

    /// 최대 캡쳐 페이지 수
    #[arg(long, default_value_t = 200)]
    pub max_pages: usize,

    /// 동시에 처리할 페이지 수. 페이지마다 디바이스 수만큼 탭을 동시에 열기 때문에
    /// 최대 탭 수는 `concurrency × 디바이스 수`가 된다
    #[arg(short = 'j', long, default_value_t = 4, value_parser = TypedValueParser::map(clap::value_parser!(u16).range(1..=64), usize::from))]
    pub concurrency: usize,

    /// 경로가 이 정규식에 매칭되는 페이지만 캡쳐/탐색 (반복 가능, 하나라도 매칭되면 통과)
    #[arg(long, value_name = "REGEX")]
    pub include: Vec<String>,

    /// 경로가 이 정규식에 매칭되면 제외 (반복 가능)
    #[arg(long, value_name = "REGEX")]
    pub exclude: Vec<String>,

    /// 기본 제외 패턴(logout/signout 등 세션을 끊는 경로)을 사용하지 않음
    #[arg(long)]
    pub no_default_excludes: bool,

    /// 링크로 발견되지 않는 경로를 추가로 크롤링 대상에 넣음 (예: /admin/settings, 반복 가능)
    #[arg(long = "seed", value_name = "PATH")]
    pub seeds: Vec<String>,

    /// 쿼리스트링이 다른 URL을 별개의 페이지로 취급 (기본: 쿼리 무시, 경로 기준으로 중복 제거)
    #[arg(long)]
    pub keep_query: bool,

    /// 루트 URL 경로 하위만 크롤링 (예: 루트가 /docs/ 이면 /docs/** 만)
    #[arg(long)]
    pub stay_under_root: bool,

    /// 페이지 로드 후 캡쳐 전 추가 대기 시간 (ms)
    #[arg(long, default_value_t = 1000)]
    pub wait_ms: u64,

    /// 페이지 로드 타임아웃 (초)
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,

    /// lazy-load 콘텐츠를 불러오기 위한 사전 스크롤을 하지 않음
    #[arg(long)]
    pub no_scroll: bool,

    /// CSS 애니메이션/트랜지션을 비활성화하지 않음
    #[arg(long)]
    pub keep_animations: bool,

    /// 4xx/5xx 응답 페이지도 캡쳐
    #[arg(long)]
    pub capture_errors: bool,

    /// 이미지 포맷
    #[arg(long, value_enum, default_value_t = ImageFormat::Png)]
    pub format: ImageFormat,

    /// jpeg/webp 품질 (0-100)
    #[arg(long, default_value_t = 85)]
    pub quality: u8,

    /// 한 장당 최대 캡쳐 높이 (CSS px). 무한 스크롤 페이지 등에서 이미지가 과도하게 커지는 것을 방지
    #[arg(long, default_value_t = 20000)]
    pub max_height: u32,

    /// 데스크탑 뷰포트 너비
    #[arg(long, default_value_t = 1440)]
    pub desktop_width: u32,

    /// 데스크탑 뷰포트 높이
    #[arg(long, default_value_t = 900)]
    pub desktop_height: u32,

    /// 모바일 뷰포트 너비
    #[arg(long, default_value_t = 390)]
    pub mobile_width: u32,

    /// 모바일 뷰포트 높이
    #[arg(long, default_value_t = 844)]
    pub mobile_height: u32,

    /// 모바일 device scale factor
    #[arg(long, default_value_t = 2.0)]
    pub mobile_scale: f64,

    /// 모바일 User-Agent
    #[arg(
        long,
        default_value = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1"
    )]
    pub mobile_user_agent: String,

    /// 데스크탑 User-Agent (생략 시 Chrome 기본값에서 "Headless" 문자열만 제거)
    #[arg(long)]
    pub desktop_user_agent: Option<String>,

    /// Chrome/Chromium 실행 파일 경로 (생략 시 자동 탐지)
    #[arg(long, value_name = "PATH")]
    pub chrome: Option<PathBuf>,

    /// 브라우저 창을 띄워서 실행 (디버깅용)
    #[arg(long)]
    pub headful: bool,

    /// HTTPS 인증서 오류를 무시하지 않음
    #[arg(long)]
    pub strict_https: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum DeviceKind {
    Desktop,
    Mobile,
}

impl DeviceKind {
    pub fn name(self) -> &'static str {
        match self {
            DeviceKind::Desktop => "desktop",
            DeviceKind::Mobile => "mobile",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
        }
    }
}
