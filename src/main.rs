mod capture;
mod cli;
mod cookies;
mod crawler;
mod scope;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chromiumoxide::browser::{Browser, BrowserConfig};
use clap::Parser;
use futures::StreamExt;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

use chromiumoxide::cdp::browser_protocol::network::CookieParam;

use crate::capture::{CaptureOptions, DeviceProfile, Tab};
use crate::cli::{Args, DeviceKind};
use crate::crawler::{CrawlLimits, Crawler, Manifest, Summary, build_seeds};
use crate::scope::Scope;

static STOP: AtomicBool = AtomicBool::new(false);

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("page_screen_capturer=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match run(Args::parse()).await {
        Ok(summary) if summary.captured > 0 => ExitCode::SUCCESS,
        Ok(_) => {
            warn!("캡쳐된 페이지가 없습니다");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<Summary> {
    let root = parse_root(&args.url)?;
    // 브라우저를 띄우기 전에 필터/쿠키 입력 오류를 먼저 검증한다
    prepare(&args, &root)?;

    let mut devices = args.devices.clone();
    devices.dedup();
    if devices.is_empty() {
        bail!("--devices 에 최소 한 개의 디바이스가 필요합니다");
    }

    let user_data_dir = std::env::temp_dir().join(format!(
        "page-screen-capturer-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ));
    let mut config = BrowserConfig::builder();
    config = if args.headful {
        config.with_head()
    } else {
        config.new_headless_mode()
    };
    config = config
        .viewport(None)
        .window_size(args.desktop_width, args.desktop_height)
        .user_data_dir(&user_data_dir)
        .request_timeout(Duration::from_secs(args.timeout + 60))
        .arg("--hide-scrollbars")
        .arg("--mute-audio");
    if let Some(chrome) = &args.chrome {
        config = config.chrome_executable(chrome);
    }
    if args.strict_https {
        config = config.respect_https_errors();
    }
    let config = config.build().map_err(|e| anyhow::anyhow!(e))?;

    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .context("Chrome 실행 실패 (--chrome 으로 실행 파일 경로를 지정할 수 있습니다)")?;
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                tracing::debug!("CDP handler: {e}");
            }
        }
    });

    let result = crawl(&browser, &args, root, &devices).await;

    if let Err(e) = browser.close().await {
        tracing::debug!("브라우저 종료 실패: {e}");
    }
    browser.wait().await.ok();
    handler_task.abort();
    std::fs::remove_dir_all(&user_data_dir).ok();
    result
}

/// 루트 URL 기준으로 크롤링 범위, 시작 페이지, 주입할 쿠키를 만든다.
fn prepare(args: &Args, root: &Url) -> Result<(Scope, Vec<crawler::Job>, Vec<CookieParam>)> {
    let scope = Scope::new(
        root,
        &args.include,
        &args.exclude,
        !args.no_default_excludes,
        args.keep_query,
        args.stay_under_root,
    )?;
    let seeds = build_seeds(&scope, root, &args.seeds)?;
    if seeds.is_empty() {
        bail!("크롤링할 시작 페이지가 없습니다 (루트가 exclude 패턴에 해당하는지 확인하세요)");
    }
    let cookies = load_cookies(args, root)?;
    Ok((scope, seeds, cookies))
}

async fn crawl(
    browser: &Browser,
    args: &Args,
    mut root: Url,
    devices: &[DeviceKind],
) -> Result<Summary> {
    let desktop_ua = match &args.desktop_user_agent {
        Some(ua) => ua.clone(),
        None => browser
            .user_agent()
            .await?
            .replace("HeadlessChrome", "Chrome"),
    };
    let profiles: Vec<DeviceProfile> = devices
        .iter()
        .map(|&kind| match kind {
            DeviceKind::Desktop => DeviceProfile {
                kind,
                width: args.desktop_width,
                height: args.desktop_height,
                scale: 1.0,
                mobile: false,
                user_agent: Some(desktop_ua.clone()),
            },
            DeviceKind::Mobile => DeviceProfile {
                kind,
                width: args.mobile_width,
                height: args.mobile_height,
                scale: args.mobile_scale,
                mobile: true,
                user_agent: Some(args.mobile_user_agent.clone()),
            },
        })
        .collect();

    let opts = CaptureOptions {
        timeout: Duration::from_secs(args.timeout),
        wait: Duration::from_millis(args.wait_ms),
        scroll: !args.no_scroll,
        disable_animations: !args.keep_animations,
        format: args.format,
        quality: args.quality,
        max_height: args.max_height,
    };

    if let Some(canonical) = resolve_canonical_root(browser, &profiles[0], &root, &opts).await {
        info!("루트가 {canonical} 로 리다이렉트되어 이 origin을 크롤링 범위로 사용합니다");
        root = canonical;
    }
    let (scope, seeds, cookies) = prepare(args, &root)?;
    if !cookies.is_empty() {
        report_cookies(&cookies, &root);
        browser
            .set_cookies(cookies)
            .await
            .context("쿠키 주입 실패")?;
    }

    tokio::fs::create_dir_all(&args.out)
        .await
        .with_context(|| format!("출력 디렉터리 생성 실패: {}", args.out.display()))?;

    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            warn!(
                "중단 요청을 받았습니다. 진행 중인 페이지만 마무리합니다 (한 번 더 누르면 즉시 종료)"
            );
            STOP.store(true, Ordering::Relaxed);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });

    info!(
        "{} 크롤링 시작 (devices: {}, max-depth: {}, max-pages: {}, concurrency: {})",
        root,
        devices
            .iter()
            .map(|d| d.name())
            .collect::<Vec<_>>()
            .join(","),
        args.max_depth,
        args.max_pages,
        args.concurrency
    );
    let crawler = Crawler::new(
        browser,
        scope,
        profiles,
        opts,
        args.out.clone(),
        args.capture_errors,
        &STOP,
    );
    let limits = CrawlLimits {
        max_depth: args.max_depth,
        max_pages: args.max_pages,
        concurrency: args.concurrency,
    };
    let records = crawler.run(seeds, &limits).await;

    if let Some(first) = records.first()
        && let Some(final_url) = &first.final_url
        && Url::parse(final_url).ok().map(|u| u.path().to_string()) != Some(root.path().to_string())
    {
        warn!(
            "루트 페이지가 {final_url} 로 리다이렉트되었습니다. 로그인 페이지라면 쿠키가 만료되었거나 도메인이 맞지 않을 수 있습니다"
        );
    }

    let summary = Summary::from_records(&records);
    let manifest = Manifest {
        root: root.as_str(),
        generated_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        devices: devices.iter().map(|d| d.name()).collect(),
        summary: Summary::from_records(&records),
        pages: &records,
    };
    let manifest_path = args.out.join("manifest.json");
    tokio::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .await
        .with_context(|| format!("manifest 저장 실패: {}", manifest_path.display()))?;

    info!(
        "완료: 방문 {} / 캡쳐 {} (스크린샷 {}장) / 건너뜀 {} / 오류 {} → {}",
        summary.visited,
        summary.captured,
        summary.screenshots,
        summary.skipped,
        summary.failed,
        manifest_path.display()
    );
    Ok(summary)
}

/// 루트가 `http→https`, `www` 추가/제거 같은 정규화 리다이렉트를 하면 최종 origin을 돌려준다.
///
/// 쿠키 없이 확인하며, 다른 도메인(SSO 로그인 페이지 등)으로의 리다이렉트는 무시한다.
async fn resolve_canonical_root(
    browser: &Browser,
    profile: &DeviceProfile,
    root: &Url,
    opts: &CaptureOptions,
) -> Option<Url> {
    let tab = Tab::open(browser, profile).await.ok()?;
    let resolved = tab.resolve(root.as_str(), opts.timeout).await;
    tab.close().await;
    let final_url = Url::parse(&resolved.ok()?).ok()?;
    if final_url.origin() == root.origin() {
        return None;
    }
    let bare = |u: &Url| {
        u.host_str()
            .map(|h| h.trim_start_matches("www.").to_string())
    };
    if bare(&final_url) != bare(root) {
        return None;
    }
    let mut canonical = root.clone();
    canonical.set_scheme(final_url.scheme()).ok()?;
    canonical.set_host(final_url.host_str()).ok()?;
    canonical.set_port(final_url.port()).ok()?;
    Some(canonical)
}

fn parse_root(raw: &str) -> Result<Url> {
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        // 로컬 개발 서버는 보통 TLS가 없으므로 http, 그 외에는 https를 기본으로 한다
        let host = raw.split(['/', ':']).next().unwrap_or_default();
        let local = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok();
        format!("{}://{raw}", if local { "http" } else { "https" })
    };
    let url = Url::parse(&with_scheme).with_context(|| format!("잘못된 URL: {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("http(s) URL이 필요합니다: {raw}");
    }
    Ok(url)
}

fn load_cookies(args: &Args, root: &Url) -> Result<Vec<CookieParam>> {
    let mut all = Vec::new();
    for raw in &args.cookies {
        all.extend(cookies::parse_inline(
            raw,
            root,
            args.cookie_domain.as_deref(),
        )?);
    }
    if let Some(path) = &args.cookie_file {
        all.extend(cookies::parse_file(path, root)?);
    }
    Ok(all)
}

fn report_cookies(all: &[CookieParam], root: &Url) {
    let (applicable, other): (Vec<_>, Vec<_>) =
        all.iter().partition(|c| cookies::applies_to(c, root));
    info!(
        "쿠키 {}개 주입: {}",
        all.len(),
        all.iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if applicable.is_empty() {
        warn!(
            "{} 호스트에 전송될 쿠키가 없습니다. 쿠키의 domain을 확인하세요",
            root.host_str().unwrap_or_default()
        );
    } else if !other.is_empty() {
        info!(
            "이 중 {}개는 다른 도메인 쿠키입니다 (그대로 설정됨)",
            other.len()
        );
    }
}
