//! 크롤링 범위 판단, URL 정규화, 경로 → 저장 디렉터리 매핑.

use std::path::PathBuf;

use anyhow::{Context, Result};
use regex::Regex;
use url::Url;

/// 로그아웃 등 인증 세션을 끊을 수 있는 경로. `--no-default-excludes`로 끌 수 있다.
pub const DEFAULT_EXCLUDES: &[&str] = &[r"(?i)(^|[/_.-])(log|sign)[-_]?(out|off)([/_.?-]|$)"];

/// 이미지/문서/바이너리 등 HTML 페이지가 아닌 것으로 간주할 확장자.
const NON_HTML_EXTENSIONS: &[&str] = &[
    "7z", "apk", "atom", "avi", "bin", "bmp", "css", "csv", "dmg", "doc", "docx", "eot", "exe",
    "gif", "gz", "ico", "iso", "jpeg", "jpg", "js", "json", "m4a", "map", "mjs", "mov", "mp3",
    "mp4", "msi", "ogg", "otf", "pdf", "png", "ppt", "pptx", "rar", "rss", "svg", "tar", "tgz",
    "ttf", "txt", "wasm", "wav", "webm", "webp", "woff", "woff2", "xls", "xlsx", "xml", "zip",
];

/// 크롤링 대상 URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// 실제로 이동할 URL (fragment 제거, `keep_query`가 아니면 쿼리 제거)
    pub url: Url,
    /// 중복 판단 키 (예: `/docs/intro`, `/search?q=a`)
    pub key: String,
}

pub struct Scope {
    origin: url::Origin,
    root_prefix: Option<String>,
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    keep_query: bool,
}

impl Scope {
    pub fn new(
        root: &Url,
        include: &[String],
        exclude: &[String],
        default_excludes: bool,
        keep_query: bool,
        stay_under_root: bool,
    ) -> Result<Self> {
        let compile = |p: &String| Regex::new(p).with_context(|| format!("잘못된 정규식: {p}"));
        let include = include.iter().map(compile).collect::<Result<Vec<_>>>()?;
        let mut exclude = exclude.iter().map(compile).collect::<Result<Vec<_>>>()?;
        if default_excludes {
            exclude.extend(DEFAULT_EXCLUDES.iter().map(|p| Regex::new(p).unwrap()));
        }
        let root_prefix = stay_under_root
            .then(|| path_key(root.path()))
            .filter(|p| p != "/");
        Ok(Self {
            origin: root.origin(),
            root_prefix,
            include,
            exclude,
            keep_query,
        })
    }

    /// 같은 origin의 http(s) HTML 페이지로 보이면 정규화된 `Target`을 돌려준다.
    /// include/exclude 필터는 여기서 적용하지 않는다 (`is_allowed` 참고).
    pub fn target(&self, url: &Url) -> Option<Target> {
        if !matches!(url.scheme(), "http" | "https") || url.origin() != self.origin {
            return None;
        }
        if looks_like_file(url.path()) {
            return None;
        }
        let mut nav = url.clone();
        nav.set_fragment(None);
        if !self.keep_query || nav.query() == Some("") {
            nav.set_query(None);
        }
        let key = match nav.query() {
            Some(q) => format!("{}?{q}", path_key(nav.path())),
            None => path_key(nav.path()),
        };
        if let Some(prefix) = &self.root_prefix {
            let path = path_key(nav.path());
            if path != *prefix && !path.starts_with(&format!("{prefix}/")) {
                return None;
            }
        }
        Some(Target { url: nav, key })
    }

    /// include/exclude 필터 통과 여부 (키 기준).
    pub fn is_allowed(&self, key: &str) -> bool {
        if self.exclude.iter().any(|r| r.is_match(key)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|r| r.is_match(key))
    }

    /// exclude 필터에만 걸리는지 (루트/seed처럼 탐색은 해야 하는 경우 판단용).
    pub fn is_excluded(&self, key: &str) -> bool {
        self.exclude.iter().any(|r| r.is_match(key))
    }
}

/// 경로의 끝 슬래시를 제거해 `/about/`과 `/about`을 같은 페이지로 취급한다.
fn path_key(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.into()
    }
}

fn looks_like_file(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            NON_HTML_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

/// 페이지 키를 출력 디렉터리 기준 상대 경로로 변환한다.
///
/// - `/` → `_root`
/// - `/docs/getting-started` → `docs/getting-started`
/// - `/검색` → `검색` (percent-decoding)
/// - `/search?q=a b` → `search__q=a_b` (`--keep-query` 사용 시)
pub fn key_to_rel_dir(key: &str) -> PathBuf {
    let (path, query) = match key.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (key, None),
    };
    let mut segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| sanitize_segment(&percent_decode(s, false)))
        .collect();
    if segments.is_empty() {
        segments.push("_root".into());
    }
    if let Some(q) = query {
        let last = segments.last_mut().unwrap();
        *last = sanitize_segment(&format!("{last}__{}", percent_decode(q, true)));
    }
    segments.iter().collect()
}

fn percent_decode(s: &str, plus_as_space: bool) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' if plus_as_space => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const MAX_SEGMENT_BYTES: usize = 100;

fn sanitize_segment(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | ' ' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows에서 문제가 되는 끝의 점/공백 제거, `.`/`..` 방지
    while out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.len() > MAX_SEGMENT_BYTES {
        let hash = format!("{:016x}", fnv1a(s.as_bytes()));
        let mut cut = MAX_SEGMENT_BYTES - hash.len() - 1;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('~');
        out.push_str(&hash);
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(keep_query: bool) -> Scope {
        let root = Url::parse("https://example.com/").unwrap();
        Scope::new(&root, &[], &[], true, keep_query, false).unwrap()
    }

    fn t(s: &Scope, url: &str) -> Option<String> {
        s.target(&Url::parse(url).unwrap()).map(|t| t.key)
    }

    #[test]
    fn same_origin_only() {
        let s = scope(false);
        assert_eq!(t(&s, "https://example.com/a"), Some("/a".into()));
        assert_eq!(t(&s, "http://example.com/a"), None);
        assert_eq!(t(&s, "https://sub.example.com/a"), None);
        assert_eq!(t(&s, "https://example.com:8443/a"), None);
        assert_eq!(t(&s, "mailto:a@example.com"), None);
    }

    #[test]
    fn normalizes_fragment_query_and_trailing_slash() {
        let s = scope(false);
        assert_eq!(t(&s, "https://example.com/a/?x=1#top"), Some("/a".into()));
        assert_eq!(t(&s, "https://example.com/#x"), Some("/".into()));
        let target = s
            .target(&Url::parse("https://example.com/a/?x=1#top").unwrap())
            .unwrap();
        // 이동할 URL은 끝 슬래시를 유지한다 (서버마다 처리가 다르므로)
        assert_eq!(target.url.as_str(), "https://example.com/a/");

        let s = scope(true);
        assert_eq!(
            t(&s, "https://example.com/a/?x=1#top"),
            Some("/a?x=1".into())
        );
        assert_eq!(t(&s, "https://example.com/a?"), Some("/a".into()));
    }

    #[test]
    fn skips_files() {
        let s = scope(false);
        assert_eq!(t(&s, "https://example.com/doc.PDF"), None);
        assert_eq!(t(&s, "https://example.com/img/logo.png"), None);
        assert_eq!(
            t(&s, "https://example.com/page.html"),
            Some("/page.html".into())
        );
        assert_eq!(
            t(&s, "https://example.com/v1.2/guide"),
            Some("/v1.2/guide".into())
        );
        assert_eq!(
            t(&s, "https://example.com/.well-known"),
            Some("/.well-known".into())
        );
    }

    #[test]
    fn default_excludes_logout() {
        let s = scope(false);
        for k in [
            "/logout",
            "/auth/log-out",
            "/signout?next=/",
            "/user/sign_off",
            "/api/logout/",
        ] {
            assert!(!s.is_allowed(k), "{k} should be excluded");
        }
        for k in ["/catalog-offers", "/blog/outage", "/login", "/"] {
            assert!(s.is_allowed(k), "{k} should be allowed");
        }
    }

    #[test]
    fn include_and_exclude() {
        let root = Url::parse("https://example.com/").unwrap();
        let s = Scope::new(
            &root,
            &["^/docs".into()],
            &["/internal".into()],
            false,
            false,
            false,
        )
        .unwrap();
        assert!(s.is_allowed("/docs/a"));
        assert!(!s.is_allowed("/blog"));
        assert!(!s.is_allowed("/docs/internal/x"));
        assert!(s.is_allowed("/docs/logout"));
    }

    #[test]
    fn stay_under_root() {
        let root = Url::parse("https://example.com/docs/").unwrap();
        let s = Scope::new(&root, &[], &[], true, false, true).unwrap();
        assert_eq!(t(&s, "https://example.com/docs"), Some("/docs".into()));
        assert_eq!(t(&s, "https://example.com/docs/a"), Some("/docs/a".into()));
        assert_eq!(t(&s, "https://example.com/docsx"), None);
        assert_eq!(t(&s, "https://example.com/"), None);
    }

    #[test]
    fn rel_dir_mapping() {
        assert_eq!(key_to_rel_dir("/"), PathBuf::from("_root"));
        assert_eq!(key_to_rel_dir("/docs/intro"), PathBuf::from("docs/intro"));
        assert_eq!(key_to_rel_dir("/%EA%B2%80%EC%83%89"), PathBuf::from("검색"));
        assert_eq!(key_to_rel_dir("/a/../b"), PathBuf::from("a/_/b"));
        assert_eq!(
            key_to_rel_dir("/search?q=a+b&x=1"),
            PathBuf::from("search__q=a_b&x=1")
        );
        assert_eq!(key_to_rel_dir("/?tab=2"), PathBuf::from("_root__tab=2"));
        assert_eq!(key_to_rel_dir("/a:b|c"), PathBuf::from("a_b_c"));
    }

    #[test]
    fn long_segment_is_truncated_with_hash() {
        let long = format!("/{}", "가".repeat(80));
        let dir = key_to_rel_dir(&long);
        let name = dir.to_str().unwrap();
        assert!(name.len() <= MAX_SEGMENT_BYTES);
        assert!(name.contains('~'));
        assert_ne!(key_to_rel_dir(&format!("{long}x")), dir);
    }
}
