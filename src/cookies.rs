//! 인증 세션 주입을 위한 쿠키 파싱.
//!
//! 지원 입력:
//! - 인라인: `name=value`, `a=1; b=2` (브라우저 `Cookie` 헤더 형식)
//! - JSON 배열: 브라우저 확장(EditThisCookie, Cookie-Editor 등) / CDP / Puppeteer export
//! - JSON 객체: Playwright `storageState` (`{"cookies": [...]}`)
//! - Netscape `cookies.txt` (curl, yt-dlp, 각종 확장에서 export)

use std::path::Path;

use anyhow::{Context, Result, bail};
use chromiumoxide::cdp::browser_protocol::network::{CookieParam, CookieSameSite, TimeSinceEpoch};
use serde_json::Value;
use url::Url;

/// `name=value; name2=value2` 형식의 문자열을 쿠키 목록으로 변환.
///
/// `domain`이 주어지면 해당 도메인 쿠키로, 아니면 `root`의 호스트 전용 쿠키로 설정된다.
pub fn parse_inline(raw: &str, root: &Url, domain: Option<&str>) -> Result<Vec<CookieParam>> {
    let mut out = Vec::new();
    for pair in raw.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair
            .split_once('=')
            .with_context(|| format!("쿠키 형식이 잘못되었습니다 (name=value 필요): {pair:?}"))?;
        let name = name.trim();
        if name.is_empty() {
            bail!("쿠키 이름이 비어 있습니다: {pair:?}");
        }
        let mut cookie = CookieParam::new(name, value.trim());
        cookie.path = Some("/".into());
        match domain {
            Some(d) => cookie.domain = Some(d.to_string()),
            None => cookie.url = Some(origin_url(root)),
        }
        out.push(cookie);
    }
    Ok(out)
}

/// 쿠키 파일을 읽어 형식을 자동 판별해 파싱.
pub fn parse_file(path: &Path, root: &Url) -> Result<Vec<CookieParam>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("쿠키 파일을 읽을 수 없습니다: {}", path.display()))?;
    parse_file_content(&content, root)
        .with_context(|| format!("쿠키 파일 파싱 실패: {}", path.display()))
}

fn parse_file_content(content: &str, root: &Url) -> Result<Vec<CookieParam>> {
    let trimmed = content.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        let json: Value = serde_json::from_str(trimmed).context("JSON 파싱 실패")?;
        let items = match &json {
            Value::Array(items) => items,
            Value::Object(obj) => match obj.get("cookies") {
                Some(Value::Array(items)) => items,
                _ => bail!("JSON 객체에 \"cookies\" 배열이 없습니다"),
            },
            _ => unreachable!(),
        };
        items
            .iter()
            .enumerate()
            .map(|(i, item)| parse_json_cookie(item, root).with_context(|| format!("쿠키 #{i}")))
            .collect()
    } else {
        parse_netscape(content)
    }
}

fn parse_json_cookie(item: &Value, root: &Url) -> Result<CookieParam> {
    let obj = item.as_object().context("쿠키 항목이 객체가 아닙니다")?;
    let str_field = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
    let bool_field = |key: &str| obj.get(key).and_then(Value::as_bool);

    let name = str_field("name").context("name 필드가 없습니다")?;
    let value = str_field("value").unwrap_or_default();
    let mut cookie = CookieParam::new(name, value);

    cookie.domain = str_field("domain").filter(|d| !d.is_empty());
    cookie.path = Some(str_field("path").unwrap_or_else(|| "/".into()));
    cookie.url = str_field("url");
    if cookie.domain.is_none() && cookie.url.is_none() {
        cookie.url = Some(origin_url(root));
    }
    // EditThisCookie 등은 host-only 쿠키를 hostOnly=true + domain으로 표현한다.
    // CDP는 domain을 주면 도메인 쿠키로 만들기 때문에 url로 바꿔서 host-only를 유지한다.
    if bool_field("hostOnly") == Some(true)
        && let Some(domain) = cookie.domain.take()
    {
        let scheme = if bool_field("secure") == Some(true) {
            "https"
        } else {
            root.scheme()
        };
        cookie.url = Some(format!("{scheme}://{}/", domain.trim_start_matches('.')));
    }

    cookie.secure = bool_field("secure");
    cookie.http_only = bool_field("httpOnly");
    cookie.same_site = str_field("sameSite").and_then(|s| parse_same_site(&s));

    let expires = obj
        .get("expires")
        .or_else(|| obj.get("expirationDate"))
        .and_then(Value::as_f64);
    if let Some(exp) = expires.filter(|e| *e > 0.0) {
        cookie.expires = Some(TimeSinceEpoch::new(exp));
    }
    Ok(cookie)
}

fn parse_same_site(s: &str) -> Option<CookieSameSite> {
    match s.to_ascii_lowercase().as_str() {
        "strict" => Some(CookieSameSite::Strict),
        "lax" => Some(CookieSameSite::Lax),
        "none" | "no_restriction" => Some(CookieSameSite::None),
        _ => None,
    }
}

/// Netscape cookies.txt: `domain \t includeSubdomains \t path \t secure \t expiry \t name \t value`
fn parse_netscape(content: &str) -> Result<Vec<CookieParam>> {
    let mut out = Vec::new();
    for (lineno, line) in content.lines().enumerate() {
        let (line, http_only) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            bail!(
                "{}번째 줄: Netscape 쿠키 형식은 탭으로 구분된 7개 필드가 필요합니다",
                lineno + 1
            );
        }
        let [domain, include_sub, path, secure, expiry, name, value] =
            [0, 1, 2, 3, 4, 5, 6].map(|i| fields[i]);
        let secure = secure.eq_ignore_ascii_case("TRUE");

        let mut cookie = CookieParam::new(name, value);
        cookie.path = Some(if path.is_empty() {
            "/".into()
        } else {
            path.into()
        });
        if include_sub.eq_ignore_ascii_case("TRUE") {
            cookie.domain = Some(domain.into());
        } else {
            let scheme = if secure { "https" } else { "http" };
            cookie.url = Some(format!("{scheme}://{}/", domain.trim_start_matches('.')));
        }
        cookie.secure = Some(secure);
        cookie.http_only = Some(http_only);
        if let Ok(exp) = expiry.parse::<f64>()
            && exp > 0.0
        {
            cookie.expires = Some(TimeSinceEpoch::new(exp));
        }
        out.push(cookie);
    }
    Ok(out)
}

fn origin_url(root: &Url) -> String {
    format!("{}/", root.origin().ascii_serialization())
}

/// 쿠키가 루트 URL의 호스트로 전송될 수 있는지 대략적으로 판단 (경고 용도).
pub fn applies_to(cookie: &CookieParam, root: &Url) -> bool {
    let Some(host) = root.host_str() else {
        return false;
    };
    if let Some(domain) = &cookie.domain {
        let d = domain.trim_start_matches('.');
        return host == d || host.ends_with(&format!(".{d}"));
    }
    if let Some(url) = cookie.url.as_deref().and_then(|u| Url::parse(u).ok()) {
        return url.host_str() == Some(host);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Url {
        Url::parse("https://app.example.com/dashboard").unwrap()
    }

    #[test]
    fn inline_multiple_pairs() {
        let c = parse_inline("session=abc=def; theme=dark ;", &root(), None).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "session");
        assert_eq!(c[0].value, "abc=def");
        assert_eq!(c[0].url.as_deref(), Some("https://app.example.com/"));
        assert_eq!(c[1].name, "theme");
        assert_eq!(c[1].value, "dark");
    }

    #[test]
    fn inline_with_domain() {
        let c = parse_inline("sid=1", &root(), Some(".example.com")).unwrap();
        assert_eq!(c[0].domain.as_deref(), Some(".example.com"));
        assert!(c[0].url.is_none());
        assert!(applies_to(&c[0], &root()));
    }

    #[test]
    fn inline_rejects_missing_eq() {
        assert!(parse_inline("novalue", &root(), None).is_err());
    }

    #[test]
    fn json_array_edit_this_cookie() {
        let json = r#"[
            {"name":"sid","value":"x","domain":".example.com","path":"/","secure":true,
             "httpOnly":true,"sameSite":"no_restriction","expirationDate":1999999999.5,"hostOnly":false},
            {"name":"h","value":"y","domain":"app.example.com","hostOnly":true,"secure":true}
        ]"#;
        let c = parse_file_content(json, &root()).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].domain.as_deref(), Some(".example.com"));
        assert_eq!(c[0].same_site, Some(CookieSameSite::None));
        assert_eq!(c[0].http_only, Some(true));
        assert!(c[0].expires.is_some());
        assert!(c[1].domain.is_none());
        assert_eq!(c[1].url.as_deref(), Some("https://app.example.com/"));
        assert!(c.iter().all(|c| applies_to(c, &root())));
    }

    #[test]
    fn json_playwright_storage_state() {
        let json = r#"{"cookies":[{"name":"a","value":"1","domain":"app.example.com","path":"/",
            "expires":-1,"httpOnly":false,"secure":false,"sameSite":"Lax"}],"origins":[]}"#;
        let c = parse_file_content(json, &root()).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].same_site, Some(CookieSameSite::Lax));
        assert!(c[0].expires.is_none());
    }

    #[test]
    fn json_without_domain_uses_root() {
        let c = parse_file_content(r#"[{"name":"a","value":"1"}]"#, &root()).unwrap();
        assert_eq!(c[0].url.as_deref(), Some("https://app.example.com/"));
    }

    #[test]
    fn netscape_format() {
        let txt = "# Netscape HTTP Cookie File\n\
                   .example.com\tTRUE\t/\tTRUE\t0\tsid\tabc\n\
                   #HttpOnly_app.example.com\tFALSE\t/\tTRUE\t1999999999\ttok\tzzz\n";
        let c = parse_file_content(txt, &root()).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].domain.as_deref(), Some(".example.com"));
        assert!(c[0].expires.is_none());
        assert_eq!(c[1].http_only, Some(true));
        assert_eq!(c[1].url.as_deref(), Some("https://app.example.com/"));
        assert!(c[1].expires.is_some());
    }

    #[test]
    fn applies_to_other_domain_is_false() {
        let c = parse_inline("a=1", &root(), Some("other.com")).unwrap();
        assert!(!applies_to(&c[0], &root()));
    }
}
