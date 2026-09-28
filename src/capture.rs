//! 브라우저 탭 생성, 디바이스 에뮬레이션, 페이지 로드 대기, 전체 페이지 스크린샷.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chromiumoxide::cdp::browser_protocol::emulation::{
    ScreenOrientation, ScreenOrientationType, SetDeviceMetricsOverrideParams,
    SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, Viewport,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::{Browser, Page};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, RgbaImage};
use serde::Deserialize;

use crate::cli::{DeviceKind, ImageFormat};

#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub kind: DeviceKind,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub mobile: bool,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub timeout: Duration,
    pub wait: Duration,
    pub scroll: bool,
    pub disable_animations: bool,
    pub format: ImageFormat,
    pub quality: u8,
    pub max_height: u32,
}

/// 페이지 로드 후 수집한 정보.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub url: String,
    /// Navigation Timing의 응답 코드 (브라우저가 제공하지 않으면 0)
    pub status: u16,
    pub content_type: String,
    pub title: String,
    pub visibility: String,
    pub links: Vec<String>,
}

impl PageInfo {
    pub fn is_html(&self) -> bool {
        matches!(
            self.content_type.as_str(),
            "text/html" | "application/xhtml+xml"
        )
    }
}

/// 디바이스 에뮬레이션이 적용된 탭. 사용 후 반드시 `close()` 해야 한다.
pub struct Tab {
    page: Page,
}

impl Tab {
    pub async fn open(browser: &Browser, profile: &DeviceProfile) -> Result<Self> {
        // 같은 창의 탭은 활성 탭 하나만 visible 상태라서, 나머지 탭은 lazy 이미지 로딩과
        // requestAnimationFrame이 멈춘다. 동시에 여러 페이지를 캡쳐하므로 탭마다 새 창을 연다.
        let target = CreateTargetParams::builder()
            .url("about:blank")
            .new_window(true)
            .background(false)
            .build()
            .map_err(|e| anyhow!(e))?;
        let page = browser.new_page(target).await.context("새 탭 생성 실패")?;
        let tab = Self { page };
        if let Err(e) = tab.emulate(profile).await {
            tab.close().await;
            return Err(e);
        }
        Ok(tab)
    }

    async fn emulate(&self, profile: &DeviceProfile) -> Result<()> {
        let orientation = if profile.width > profile.height {
            ScreenOrientation::new(ScreenOrientationType::LandscapePrimary, 90)
        } else {
            ScreenOrientation::new(ScreenOrientationType::PortraitPrimary, 0)
        };
        let metrics = SetDeviceMetricsOverrideParams::builder()
            .width(profile.width)
            .height(profile.height)
            .device_scale_factor(profile.scale)
            .mobile(profile.mobile)
            .screen_width(profile.width)
            .screen_height(profile.height)
            .screen_orientation(orientation)
            .build()
            .map_err(|e| anyhow!(e))?;
        self.page
            .execute(metrics)
            .await
            .context("디바이스 에뮬레이션 실패")?;

        if profile.mobile {
            let touch = SetTouchEmulationEnabledParams::builder()
                .enabled(true)
                .max_touch_points(5)
                .build()
                .map_err(|e| anyhow!(e))?;
            self.page
                .execute(touch)
                .await
                .context("터치 에뮬레이션 실패")?;
        }
        if let Some(ua) = &profile.user_agent {
            self.page
                .set_user_agent(SetUserAgentOverrideParams::new(ua.clone()))
                .await
                .context("User-Agent 설정 실패")?;
        }
        Ok(())
    }

    /// URL로 이동하고, 로딩/lazy-load/폰트 대기 후 페이지 정보를 돌려준다.
    pub async fn load(&self, url: &str, opts: &CaptureOptions) -> Result<PageInfo> {
        tokio::time::timeout(opts.timeout, self.page.goto(url))
            .await
            .map_err(|_| anyhow!("페이지 로드 타임아웃 ({}s)", opts.timeout.as_secs()))?
            .with_context(|| format!("페이지 이동 실패: {url}"))?;

        // HTML이 아닌 응답(이미지, JSON 등)은 준비 작업 없이 바로 정보만 반환
        let info = self.info().await?;
        if !info.is_html() {
            return Ok(info);
        }

        if opts.disable_animations {
            self.eval(DISABLE_ANIMATIONS_JS).await.ok();
        }
        if opts.scroll {
            let js = SCROLL_JS.replace("__MAX_HEIGHT__", &opts.max_height.to_string());
            if let Err(e) = self.eval(&js).await {
                tracing::debug!("스크롤 실패 (무시): {e:#}");
            }
        }
        self.eval(SETTLE_JS).await.ok();
        tokio::time::sleep(opts.wait).await;

        // 클라이언트 사이드 리다이렉트 등으로 컨텍스트가 바뀌었을 수 있으니 한 번 재시도
        match self.info().await {
            Ok(info) => Ok(info),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.info().await
            }
        }
    }

    /// 이동만 하고 리다이렉트가 끝난 최종 URL을 돌려준다 (이미지/폰트 대기 없음).
    pub async fn resolve(&self, url: &str, timeout: Duration) -> Result<String> {
        tokio::time::timeout(timeout, self.page.goto(url))
            .await
            .map_err(|_| anyhow!("페이지 로드 타임아웃 ({}s)", timeout.as_secs()))??;
        Ok(self.page.url().await?.unwrap_or_else(|| url.to_string()))
    }

    async fn info(&self) -> Result<PageInfo> {
        self.page
            .evaluate_expression(INFO_JS)
            .await
            .context("페이지 정보 수집 실패")?
            .into_value()
            .context("페이지 정보 역직렬화 실패")
    }

    async fn eval(&self, js: &str) -> Result<()> {
        self.page.evaluate_expression(js).await?;
        Ok(())
    }

    /// 전체 페이지 스크린샷.
    ///
    /// - 모바일: 뷰포트 단위로 끝까지 스크롤하면서 실제 스크롤 위치에서 캡쳐한 뒤 이어 붙인다.
    /// - 데스크탑: 한 번에 캡쳐하되, 결과 이미지가 GPU 텍스처 한계를 넘으면 모바일과 같은 방식을 쓴다.
    ///
    /// Chrome은 `captureBeyondViewport`로 한 번에 그릴 수 있는 높이가 텍스처 한계(16384 device px)로
    /// 제한되어, 그보다 긴 페이지는 위쪽 내용이 반복된 이미지가 나온다. device scale factor가 큰
    /// 모바일에서 특히 쉽게 발생한다.
    pub async fn screenshot(
        &self,
        profile: &DeviceProfile,
        opts: &CaptureOptions,
    ) -> Result<Vec<u8>> {
        let dims: PageDims = self
            .page
            .evaluate_expression(DIMENSIONS_JS)
            .await
            .context("페이지 크기 조회 실패")?
            .into_value()
            .context("페이지 크기 역직렬화 실패")?;
        let height = dims
            .height
            .max(dims.viewport_height)
            .min(f64::from(opts.max_height));

        if profile.mobile || height * profile.scale > MAX_SINGLE_CAPTURE_PX {
            self.screenshot_scrolling(profile, &dims, height, opts)
                .await
        } else {
            self.screenshot_single(dims.width.max(dims.viewport_width), height, opts)
                .await
        }
    }

    /// `captureBeyondViewport`로 문서 전체를 한 번에 캡쳐.
    ///
    /// chromiumoxide의 `full_page` 옵션은 디바이스 메트릭을 mobile=false, scale=1로 덮어쓰므로 쓰지 않는다.
    async fn screenshot_single(
        &self,
        width: f64,
        height: f64,
        opts: &CaptureOptions,
    ) -> Result<Vec<u8>> {
        let format = match opts.format {
            ImageFormat::Png => CaptureScreenshotFormat::Png,
            ImageFormat::Jpeg => CaptureScreenshotFormat::Jpeg,
            ImageFormat::Webp => CaptureScreenshotFormat::Webp,
        };
        let mut params = CaptureScreenshotParams::builder()
            .format(format)
            .capture_beyond_viewport(true)
            .from_surface(true)
            .clip(Viewport {
                x: 0.0,
                y: 0.0,
                width: width.ceil(),
                height: height.ceil(),
                scale: 1.0,
            });
        if opts.format != ImageFormat::Png {
            params = params.quality(i64::from(opts.quality.min(100)));
        }
        self.capture(params.build()).await
    }

    /// 맨 위부터 뷰포트 높이만큼씩 끝까지 스크롤하며 보이는 화면을 캡쳐해 하나로 이어 붙인다.
    async fn screenshot_scrolling(
        &self,
        profile: &DeviceProfile,
        dims: &PageDims,
        height: f64,
        opts: &CaptureOptions,
    ) -> Result<Vec<u8>> {
        let step = dims.viewport_height.max(1.0);
        // 스크롤 중에 페이지가 길어질 수 있으므로(무한 스크롤 등) 여유를 둔다
        let max_segments = (height.max(f64::from(opts.max_height)) / step).ceil() as usize + 2;
        let viewport_shot = CaptureScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .from_surface(true)
            .build();

        // 브라우저 작업(스크롤, 캡쳐)만 여기서 하고, CPU를 쓰는 디코딩/합성/인코딩은 블로킹 스레드에서 한다
        let limit = f64::from(opts.max_height);
        // 이어 붙일 캔버스는 첫 화면(스크롤 0)의 레이아웃을 기준 좌표로 쓴다.
        // 스크롤하면 헤더가 줄어드는 등 콘텐츠가 위아래로 밀리는 페이지가 있어서,
        // 조각마다 콘텐츠 이동량(shift)을 재서 그만큼 보정한 위치에 붙인다.
        let mut segments: Vec<(f64, Vec<u8>)> = Vec::new();
        let mut canvas_y: f64 = 0.0; // 다음 조각이 채워야 할 캔버스 위치
        let mut shift = 0.0; // 첫 화면 대비 콘텐츠 이동량 (위로 밀리면 음수)
        let mut realigned = false;
        for _ in 0..max_segments {
            let target_y = (canvas_y + shift).max(0.0);
            let pos: ScrollPosition = self
                .page
                .evaluate_expression(SCROLL_TO_JS.replace("__Y__", &target_y.to_string()))
                .await
                .context("스크롤 실패")?
                .into_value()
                .context("스크롤 위치 역직렬화 실패")?;
            let is_first = segments.is_empty();
            if let Some(measured) = self.measure_shift().await {
                shift = measured;
            }
            let dest = pos.y - shift;
            // 콘텐츠가 위로 밀려서 이전 조각과의 사이에 빈 틈이 생기면, 이동량을 반영해 다시 스크롤한다
            if !is_first && dest > canvas_y + 1.0 && !realigned {
                tracing::debug!("콘텐츠 이동 {shift}px 감지, 위치 보정 후 다시 캡쳐");
                realigned = true;
                continue;
            }
            realigned = false;

            // 문서 끝에 닿았거나(목표 위치까지 스크롤되지 않음) 최대 높이를 넘으면 마지막 조각이다
            let is_last =
                pos.y + 1.0 >= pos.max_y || pos.y + 1.0 < target_y || dest + step >= limit;
            let phase = match (is_first, is_last) {
                (true, true) => "only",
                (true, false) => "first",
                (false, true) => "last",
                (false, false) => "middle",
            };
            if let Err(e) = self
                .page
                .evaluate_expression(FIXED_PHASE_JS.replace("__PHASE__", phase))
                .await
            {
                tracing::debug!("fixed 요소 처리 실패 (무시): {e}");
            }
            tracing::debug!("조각 scrollY={} shift={shift} → y={dest} ({phase})", pos.y);
            segments.push((dest, self.capture(viewport_shot.clone()).await?));
            canvas_y = dest + step;
            if is_last {
                break;
            }
        }
        let bottom = canvas_y;

        let scale = profile.scale;
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || {
            let canvas = stitch(&segments, (bottom.min(limit) * scale).ceil() as u32, scale)?;
            encode(DynamicImage::ImageRgba8(canvas), &opts)
        })
        .await
        .context("이미지 처리 작업 실패")?
    }

    /// 첫 호출에서 기준 위치를 기록하고, 이후에는 현재 화면에 보이는 요소들이
    /// 기준 대비 얼마나 이동했는지(중앙값)를 돌려준다. 잴 수 없으면 `None`.
    async fn measure_shift(&self) -> Option<f64> {
        match self.page.evaluate_expression(MEASURE_SHIFT_JS).await {
            Ok(result) => result.into_value::<Option<f64>>().ok().flatten(),
            Err(e) => {
                tracing::debug!("콘텐츠 이동량 측정 실패 (무시): {e}");
                None
            }
        }
    }

    async fn capture(&self, params: CaptureScreenshotParams) -> Result<Vec<u8>> {
        let res = self.page.execute(params).await.context("스크린샷 실패")?;
        let data: &str = res.result.data.as_ref();
        BASE64.decode(data).context("스크린샷 디코딩 실패")
    }

    pub async fn close(self) {
        if let Err(e) = self.page.close().await {
            tracing::debug!("탭 닫기 실패: {e}");
        }
    }
}

/// 이보다 높은 이미지는 한 번에 캡쳐하지 않는다 (Chrome GPU 텍스처 한계 16384px에 여유를 둔 값).
const MAX_SINGLE_CAPTURE_PX: f64 = 16000.0;

/// WebP 포맷이 지원하는 최대 크기.
const WEBP_MAX_DIMENSION: u32 = 16383;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageDims {
    width: f64,
    height: f64,
    viewport_width: f64,
    viewport_height: f64,
}

/// (스크롤 위치(CSS px), PNG) 조각들을 세로로 이어 붙인다.
fn stitch(segments: &[(f64, Vec<u8>)], height: u32, scale: f64) -> Result<RgbaImage> {
    let mut canvas: Option<RgbaImage> = None;
    for (scrolled, png) in segments {
        let shot = image::load_from_memory(png)
            .context("캡쳐 이미지 디코딩 실패")?
            .to_rgba8();
        let canvas = canvas.get_or_insert_with(|| RgbaImage::new(shot.width(), height));
        image::imageops::replace(canvas, &shot, 0, (scrolled * scale).round() as i64);
    }
    canvas.context("캡쳐된 화면이 없습니다")
}

fn encode(image: DynamicImage, opts: &CaptureOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match opts.format {
        ImageFormat::Png => image.write_with_encoder(PngEncoder::new(&mut out))?,
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(image.to_rgb8()).write_with_encoder(
            JpegEncoder::new_with_quality(&mut out, opts.quality.min(100)),
        )?,
        ImageFormat::Webp => {
            if image.height() > WEBP_MAX_DIMENSION {
                bail!(
                    "이미지 높이 {}px가 WebP 최대 크기({WEBP_MAX_DIMENSION}px)를 넘습니다. --format png 또는 jpeg를 사용하세요",
                    image.height()
                );
            }
            image.write_with_encoder(WebPEncoder::new_lossless(&mut out))?
        }
    }
    Ok(out)
}

const DIMENSIONS_JS: &str = r#"(() => {
  const doc = document.documentElement;
  const body = document.body;
  return {
    width: Math.max(doc ? doc.scrollWidth : 0, body ? body.scrollWidth : 0),
    height: Math.max(doc ? doc.scrollHeight : 0, body ? body.scrollHeight : 0),
    viewportWidth: window.innerWidth,
    viewportHeight: window.innerHeight,
  };
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScrollPosition {
    y: f64,
    max_y: f64,
}

/// 지정 위치로 즉시 스크롤하고, 화면이 안정될 때까지 기다린 뒤 실제 스크롤 위치를 돌려준다.
///
/// - 화면 안 이미지 로딩 대기 (최대 1초)
/// - 스크롤에 반응하는 스크립트(등장 효과, lazy 렌더링 등)가 DOM을 바꾸는 동안 대기:
///   100ms 동안 변경이 없을 때까지, 최대 1초
const SCROLL_TO_JS: &str = r#"(async () => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  // 백그라운드 탭(--headful)에서는 rAF가 멈출 수 있으므로 타임아웃과 경쟁시킨다
  const frames = () => Promise.race([
    new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))),
    sleep(100),
  ]);
  window.scrollTo({ top: __Y__, left: 0, behavior: 'instant' });
  await frames();

  const pending = Array.from(document.images).filter(img => {
    if (img.complete) return false;
    const r = img.getBoundingClientRect();
    return r.bottom > 0 && r.top < window.innerHeight;
  });
  await Promise.race([
    Promise.all(pending.map(img => new Promise(r => {
      img.addEventListener('load', r, { once: true });
      img.addEventListener('error', r, { once: true });
    }))),
    sleep(1000),
  ]);

  await new Promise(resolve => {
    let quiet = null;
    let limit = null;
    const observer = new MutationObserver(() => {
      clearTimeout(quiet);
      quiet = setTimeout(done, 100);
    });
    function done() {
      observer.disconnect();
      clearTimeout(quiet);
      clearTimeout(limit);
      resolve();
    }
    observer.observe(document.documentElement, {
      subtree: true, childList: true, attributes: true, characterData: true,
    });
    quiet = setTimeout(done, 100);
    limit = setTimeout(done, 1000);
  });
  await frames();

  const doc = document.documentElement;
  const scrollHeight = Math.max(doc ? doc.scrollHeight : 0, document.body ? document.body.scrollHeight : 0);
  return { y: window.scrollY, maxY: Math.max(0, scrollHeight - window.innerHeight) };
})()"#;

/// 스크롤에 따른 콘텐츠 이동량 측정.
///
/// 첫 호출 시 fixed/sticky가 아닌 요소들의 문서 기준 위치를 기록해 두고, 이후 호출에서는
/// 현재 화면에 보이는 기록된 요소들의 위치 변화량 중앙값을 돌려준다.
const MEASURE_SHIFT_JS: &str = r#"(() => {
  const vh = window.innerHeight;
  const sy = window.scrollY;
  if (!window.__pscProbes) {
    const probes = [];
    const walk = parent => {
      for (const el of parent.children) {
        const pos = getComputedStyle(el).position;
        if (pos === 'fixed' || pos === 'sticky' || pos === '-webkit-sticky') continue;
        const r = el.getBoundingClientRect();
        if (r.height > 0 && r.height <= vh) probes.push({ el, top: r.top + sy });
        walk(el);
      }
    };
    if (document.body) walk(document.body);
    window.__pscProbes = probes;
    return 0;
  }
  const shifts = [];
  for (const p of window.__pscProbes) {
    if (!p.el.isConnected) continue;
    const r = p.el.getBoundingClientRect();
    if (r.height > 0 && r.bottom > 0 && r.top < vh) shifts.push(r.top + sy - p.top);
  }
  if (shifts.length === 0) return null;
  shifts.sort((a, b) => a - b);
  return shifts[Math.floor(shifts.length / 2)];
})()"#;

/// 조각마다 fixed/sticky 요소가 반복되지 않도록 캡쳐 직전에 표시 여부를 조정한다.
///
/// - fixed 요소 중 화면 위쪽에 붙은 것(헤더 등)은 첫 조각에서만,
///   아래쪽에 붙은 것(하단 탭바, 플로팅 버튼 등)은 마지막 조각에서만 보이게 한다.
///   → 이어 붙인 이미지에서 헤더는 맨 위, 탭바는 맨 아래에 한 번씩만 나온다.
/// - sticky 요소는 첫 조각 이후 원래 자리(문서 흐름상 위치)에 머물게 한다.
const FIXED_PHASE_JS: &str = r#"(async () => {
  const phase = '__PHASE__';
  const vh = window.innerHeight;
  for (const el of document.querySelectorAll('body *')) {
    const pos = getComputedStyle(el).position;
    if (pos === 'fixed') {
      if (!el.hasAttribute('data-psc-visibility')) {
        el.setAttribute('data-psc-visibility', el.style.getPropertyValue('visibility'));
      }
      const r = el.getBoundingClientRect();
      const atBottom = (r.top + r.bottom) / 2 > vh / 2;
      const show = phase === 'only'
        || (phase === 'first' && !atBottom)
        || (phase === 'last' && atBottom);
      if (show) {
        const original = el.getAttribute('data-psc-visibility');
        if (original) el.style.setProperty('visibility', original);
        else el.style.removeProperty('visibility');
      } else {
        el.style.setProperty('visibility', 'hidden', 'important');
      }
    } else if ((pos === 'sticky' || pos === '-webkit-sticky') && phase !== 'first' && phase !== 'only') {
      el.style.setProperty('position', 'relative', 'important');
      el.style.setProperty('top', 'auto', 'important');
      el.style.setProperty('bottom', 'auto', 'important');
    }
  }
  await Promise.race([
    new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))),
    new Promise(r => setTimeout(r, 100)),
  ]);
  return true;
})()"#;

const INFO_JS: &str = r#"(() => {
  const nav = performance.getEntriesByType('navigation')[0];
  const links = Array.from(document.querySelectorAll('a[href], area[href]'))
    .map(a => a.href)
    .filter(h => typeof h === 'string' && h.length > 0);
  return {
    url: location.href,
    status: (nav && nav.responseStatus) || 0,
    contentType: document.contentType || '',
    title: document.title || '',
    visibility: document.visibilityState,
    links: Array.from(new Set(links)),
  };
})()"#;

const DISABLE_ANIMATIONS_JS: &str = r#"(() => {
  const style = document.createElement('style');
  style.setAttribute('data-page-screen-capturer', '');
  style.textContent = `*, *::before, *::after {
    animation-duration: 0s !important; animation-delay: 0s !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0s !important; transition-delay: 0s !important;
    caret-color: transparent !important; scroll-behavior: auto !important;
  }`;
  (document.head || document.documentElement).appendChild(style);
  return true;
})()"#;

/// lazy-load 이미지/컴포넌트를 불러오기 위해 페이지 끝까지 스크롤한 뒤 맨 위로 돌아간다.
const SCROLL_JS: &str = r#"(async () => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const maxHeight = __MAX_HEIGHT__;
  const step = Math.max(window.innerHeight, 400);
  const scrollHeight = () => Math.max(
    document.documentElement ? document.documentElement.scrollHeight : 0,
    document.body ? document.body.scrollHeight : 0);
  // 현재 화면(위아래 한 화면 여유 포함)에 걸친 이미지가 로드될 때까지 최대 1초 대기
  const visibleImagesLoaded = () => {
    const pending = Array.from(document.images).filter(img => {
      if (img.complete) return false;
      const r = img.getBoundingClientRect();
      return r.bottom > -window.innerHeight && r.top < window.innerHeight * 2;
    });
    const loaded = Promise.all(pending.map(img => new Promise(r => {
      img.addEventListener('load', r, { once: true });
      img.addEventListener('error', r, { once: true });
    })));
    return Promise.race([loaded, sleep(1000)]);
  };
  let y = 0;
  for (let i = 0; i < 200; i++) {
    if (y >= scrollHeight() || y >= maxHeight) break;
    y += step;
    window.scrollTo(0, y);
    await sleep(120);
    await visibleImagesLoaded();
  }
  window.scrollTo(0, 0);
  await sleep(150);
  return true;
})()"#;

/// 로딩 중인 이미지와 웹폰트를 최대 5초까지 기다린다.
/// 화면 밖의 `loading="lazy"` 이미지는 스크롤 전까지 로드되지 않으므로 기다리지 않는다.
const SETTLE_JS: &str = r#"(async () => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const inView = img => {
    const r = img.getBoundingClientRect();
    return r.bottom > 0 && r.top < window.innerHeight;
  };
  const images = Array.from(document.images)
    .filter(img => !img.complete && (img.loading !== 'lazy' || inView(img)));
  const imagesLoaded = Promise.all(images.map(img => new Promise(r => {
    img.addEventListener('load', r, { once: true });
    img.addEventListener('error', r, { once: true });
  })));
  const fonts = document.fonts ? document.fonts.ready : Promise.resolve();
  await Promise.race([Promise.all([imagesLoaded, fonts]), sleep(5000)]);
  return true;
})()"#;
