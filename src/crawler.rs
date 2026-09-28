//! 너비 우선(BFS) 크롤링과 디바이스별 캡쳐 오케스트레이션.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use chromiumoxide::Browser;
use futures::StreamExt;
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use serde::Serialize;
use tracing::{debug, info, warn};
use url::Url;

use crate::capture::{CaptureOptions, DeviceProfile, PageInfo, Tab};
use crate::scope::{Scope, Target, key_to_rel_dir};

/// manifest.json에 기록되는 페이지별 결과.
#[derive(Debug, Clone, Serialize)]
pub struct PageRecord {
    pub url: String,
    pub key: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 출력 디렉터리 기준 상대 경로
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// 디바이스 이름 → 스크린샷 파일 (출력 디렉터리 기준 상대 경로)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub screenshots: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub target: Target,
    pub depth: usize,
    pub referrer: Option<String>,
    /// false면 링크 탐색만 하고 캡쳐하지 않는다 (필터에 걸린 루트/seed)
    pub capture: bool,
}

pub struct CrawlLimits {
    pub max_depth: usize,
    pub max_pages: usize,
    pub concurrency: usize,
}

pub struct Crawler<'a> {
    pub browser: &'a Browser,
    pub scope: Scope,
    pub profiles: Vec<DeviceProfile>,
    pub opts: CaptureOptions,
    pub out_dir: PathBuf,
    pub capture_errors: bool,
    pub stop: &'a AtomicBool,
    state: Mutex<State>,
    progress: AtomicUsize,
}

#[derive(Default)]
struct State {
    /// 캡쳐했거나 캡쳐 중인 최종 페이지 키 (리다이렉트 중복 방지)
    claimed: HashSet<String>,
    /// 대소문자 무시 디렉터리 이름 (macOS/Windows 파일시스템 충돌 방지)
    used_dirs: HashSet<String>,
}

impl<'a> Crawler<'a> {
    pub fn new(
        browser: &'a Browser,
        scope: Scope,
        profiles: Vec<DeviceProfile>,
        opts: CaptureOptions,
        out_dir: PathBuf,
        capture_errors: bool,
        stop: &'a AtomicBool,
    ) -> Self {
        assert!(!profiles.is_empty(), "최소 한 개의 디바이스가 필요합니다");
        Self {
            browser,
            scope,
            profiles,
            opts,
            out_dir,
            capture_errors,
            stop,
            state: Mutex::default(),
            progress: AtomicUsize::new(0),
        }
    }

    /// 작업 큐 기반으로 최대 `concurrency`개의 페이지를 동시에 처리한다.
    ///
    /// 깊이(depth) 단위로 기다리지 않고, 한 페이지가 끝나는 즉시 발견한 링크를 큐에 넣고
    /// 빈 슬롯에 다음 페이지를 투입한다. 큐는 FIFO라서 대체로 얕은 페이지부터 처리된다.
    pub async fn run(&self, seeds: Vec<Job>, limits: &CrawlLimits) -> Vec<PageRecord> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<Job> = seeds
            .into_iter()
            .filter(|j| seen.insert(j.target.key.clone()))
            .collect();
        let mut in_flight = FuturesUnordered::new();
        let mut records = Vec::new();
        let mut launched = 0;
        let concurrency = limits.concurrency.max(1);

        loop {
            while in_flight.len() < concurrency
                && launched < limits.max_pages
                && !self.stop.load(Ordering::Relaxed)
            {
                let Some(job) = queue.pop_front() else { break };
                launched += 1;
                in_flight.push(self.process(job, limits.max_pages));
            }
            let Some((record, links)) = in_flight.next().await else {
                break;
            };

            if record.depth < limits.max_depth {
                let referrer = record
                    .final_url
                    .clone()
                    .unwrap_or_else(|| record.url.clone());
                for link in links {
                    let Ok(url) = Url::parse(&link) else { continue };
                    let Some(target) = self.scope.target(&url) else {
                        continue;
                    };
                    if !self.scope.is_allowed(&target.key) || !seen.insert(target.key.clone()) {
                        continue;
                    }
                    queue.push_back(Job {
                        target,
                        depth: record.depth + 1,
                        referrer: Some(referrer.clone()),
                        capture: true,
                    });
                }
            }
            records.push(record);
        }

        if launched >= limits.max_pages && !queue.is_empty() {
            info!(
                "최대 페이지 수({})에 도달해 {}개 페이지는 방문하지 않았습니다",
                limits.max_pages,
                queue.len()
            );
        }
        // 동시 처리로 완료 순서가 섞이므로 manifest는 깊이 → 경로 순으로 정렬한다
        records.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.key.cmp(&b.key)));
        records
    }

    /// 한 페이지를 모든 디바이스에서 동시에 로드하고 캡쳐한다.
    async fn process(&self, job: Job, max_pages: usize) -> (PageRecord, Vec<String>) {
        let mut rec = PageRecord {
            url: job.target.url.to_string(),
            key: job.target.key.clone(),
            depth: job.depth,
            referrer: job.referrer.clone(),
            final_url: None,
            status: None,
            title: None,
            dir: None,
            screenshots: BTreeMap::new(),
            skipped: None,
            errors: Vec::new(),
        };
        let mut links = Vec::new();
        if self.stop.load(Ordering::Relaxed) {
            rec.skipped = Some("중단됨".into());
            return (rec, links);
        }
        let n = self.progress.fetch_add(1, Ordering::Relaxed) + 1;

        // 1. 모든 디바이스 탭을 동시에 열고 로드
        let loads = join_all(
            self.profiles
                .iter()
                .map(|profile| self.open_and_load(profile, job.target.url.as_str())),
        )
        .await;
        let mut tabs: Vec<(&DeviceProfile, Tab, PageInfo)> = Vec::new();
        for (profile, result) in self.profiles.iter().zip(loads) {
            match result {
                Ok((tab, info)) => {
                    links.extend(info.links.iter().cloned());
                    tabs.push((profile, tab, info));
                }
                Err(e) => rec.errors.push(format!("{}: {e:#}", profile.kind.name())),
            }
        }

        // 2. 첫 번째로 로드에 성공한 디바이스 기준으로 캡쳐 여부와 저장 위치 결정
        let Some(page) = tabs.first().map(|(_, _, info)| info.clone()) else {
            warn!(
                "[{n}/{max_pages}] {} 실패: {}",
                job.target.key,
                rec.errors.join(" / ")
            );
            return (rec, links);
        };
        rec.final_url = Some(page.url.clone());
        rec.status = (page.status != 0).then_some(page.status);
        rec.title = Some(page.title.clone()).filter(|t| !t.is_empty());

        let dir = match self.decide(&job, &page) {
            Ok(dir) => dir,
            Err(reason) => {
                info!("[{n}/{max_pages}] {} 건너뜀: {reason}", job.target.key);
                rec.skipped = Some(reason);
                join_all(tabs.into_iter().map(|(_, tab, _)| tab.close())).await;
                return (rec, links);
            }
        };
        rec.dir = Some(rel_string(&dir));

        // 3. 디바이스별 캡쳐도 동시에 수행
        let final_key = self.key_of(&page.url);
        let shots = join_all(tabs.into_iter().map(|(profile, tab, info)| {
            let dir = &dir;
            let same_page = self.key_of(&info.url) == final_key;
            async move {
                let result = if same_page {
                    self.shoot(&tab, profile, dir).await
                } else {
                    Err(anyhow::anyhow!("다른 페이지로 리다이렉트됨: {}", info.url))
                };
                tab.close().await;
                (profile.kind.name(), result)
            }
        }))
        .await;
        for (name, result) in shots {
            match result {
                Ok(file) => {
                    rec.screenshots.insert(name.into(), file);
                }
                Err(e) => rec.errors.push(format!("{name}: {e:#}")),
            }
        }

        let status = rec.status.map_or("-".into(), |s| s.to_string());
        let devices: Vec<&str> = rec.screenshots.keys().map(String::as_str).collect();
        if rec.errors.is_empty() {
            info!(
                "[{n}/{max_pages}] {} ({status}) → {} [{}]",
                job.target.key,
                rec.dir.as_deref().unwrap_or_default(),
                devices.join(",")
            );
        } else {
            warn!(
                "[{n}/{max_pages}] {} ({status}) 일부 실패: {}",
                job.target.key,
                rec.errors.join(" / ")
            );
        }
        (rec, links)
    }

    fn key_of(&self, url: &str) -> Option<String> {
        Url::parse(url)
            .ok()
            .and_then(|u| self.scope.target(&u))
            .map(|t| t.key)
    }

    async fn open_and_load(&self, profile: &DeviceProfile, url: &str) -> Result<(Tab, PageInfo)> {
        let started = Instant::now();
        let tab = Tab::open(self.browser, profile).await?;
        let opened = started.elapsed();
        match tab.load(url, &self.opts).await {
            Ok(info) => {
                debug!(
                    "{url} [{}] 탭 열기 {}ms, 로드 {}ms ({})",
                    profile.kind.name(),
                    opened.as_millis(),
                    (started.elapsed() - opened).as_millis(),
                    info.visibility
                );
                Ok((tab, info))
            }
            Err(e) => {
                tab.close().await;
                Err(e)
            }
        }
    }

    /// 캡쳐 여부를 결정하고, 캡쳐할 경우 저장 디렉터리(상대 경로)를 할당한다.
    fn decide(&self, job: &Job, page: &PageInfo) -> std::result::Result<PathBuf, String> {
        if !page.is_html() {
            return Err(format!("HTML 문서가 아님 ({})", page.content_type));
        }
        let final_url = Url::parse(&page.url).map_err(|e| format!("최종 URL 파싱 실패: {e}"))?;
        let Some(final_target) = self.scope.target(&final_url) else {
            return Err(format!("범위 밖으로 리다이렉트됨: {}", page.url));
        };
        if !job.capture {
            return Err("필터에 의해 캡쳐 제외 (링크 탐색만 수행)".into());
        }
        if final_target.key != job.target.key && !self.scope.is_allowed(&final_target.key) {
            return Err(format!(
                "리다이렉트된 경로가 필터에 의해 제외됨: {}",
                final_target.key
            ));
        }
        if page.status >= 400 && !self.capture_errors {
            return Err(format!("HTTP {}", page.status));
        }

        let mut state = self.state.lock().unwrap();
        if !state.claimed.insert(final_target.key.clone()) {
            return Err(format!(
                "이미 캡쳐된 페이지로 리다이렉트됨: {}",
                final_target.key
            ));
        }
        let base = key_to_rel_dir(&final_target.key);
        let mut rel = base.clone();
        let mut suffix = 2;
        while !state.used_dirs.insert(rel.to_string_lossy().to_lowercase()) {
            let mut name = base.file_name().unwrap_or_default().to_os_string();
            name.push(format!("__{suffix}"));
            rel = base.with_file_name(name);
            suffix += 1;
        }
        Ok(rel)
    }

    /// 스크린샷을 저장하고 출력 디렉터리 기준 상대 경로를 돌려준다.
    async fn shoot(&self, tab: &Tab, profile: &DeviceProfile, rel_dir: &Path) -> Result<String> {
        let file = rel_dir.join(format!(
            "{}.{}",
            profile.kind.name(),
            self.opts.format.extension()
        ));
        let started = Instant::now();
        let bytes = tab.screenshot(profile, &self.opts).await?;
        debug!(
            "{} [{}] 캡쳐 {}ms",
            rel_dir.display(),
            profile.kind.name(),
            started.elapsed().as_millis()
        );
        let abs = self.out_dir.join(&file);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, bytes).await?;
        Ok(rel_string(&file))
    }
}

fn rel_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// manifest.json 구조.
#[derive(Serialize)]
pub struct Manifest<'a> {
    pub root: &'a str,
    pub generated_at_unix: u64,
    pub devices: Vec<&'static str>,
    pub summary: Summary,
    pub pages: &'a [PageRecord],
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub visited: usize,
    pub captured: usize,
    pub skipped: usize,
    pub failed: usize,
    pub screenshots: usize,
}

impl Summary {
    pub fn from_records(records: &[PageRecord]) -> Self {
        let mut s = Summary {
            visited: records.len(),
            ..Default::default()
        };
        for r in records {
            s.screenshots += r.screenshots.len();
            if !r.screenshots.is_empty() {
                s.captured += 1;
            } else if r.skipped.is_some() {
                s.skipped += 1;
            }
            if !r.errors.is_empty() {
                s.failed += 1;
            }
        }
        s
    }
}

/// 같은 키가 여러 번 들어오지 않도록 seed 목록을 만든다.
pub fn build_seeds(scope: &Scope, root: &Url, extra: &[String]) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |url: Url, label: &str| -> Result<()> {
        let target = scope
            .target(&url)
            .ok_or_else(|| anyhow::anyhow!("{label} URL이 크롤링 범위 밖입니다: {url}"))?;
        if scope.is_excluded(&target.key) {
            warn!("{label} {}는 exclude 패턴에 해당해 제외됩니다", target.key);
            return Ok(());
        }
        if seen.insert(target.key.clone()) {
            let capture = scope.is_allowed(&target.key);
            jobs.push(Job {
                target,
                depth: 0,
                referrer: None,
                capture,
            });
        }
        Ok(())
    };
    push(root.clone(), "루트")?;
    for s in extra {
        let url = root
            .join(s)
            .map_err(|e| anyhow::anyhow!("잘못된 seed 경로 {s:?}: {e}"))?;
        push(url, "seed")?;
    }
    Ok(jobs)
}
