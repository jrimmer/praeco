//! Check for updates: fetch releases, compare versions, collect the notes of
//! every release newer than the running one.

use crate::error::{Error, ErrorCode, Result};
use crate::http::{read_body, Http};
use crate::notes::{strip_install_section, DEFAULT_MARKER};
use crate::release::{parse_releases, Release};
use crate::source::{Host, Source};
use crate::version;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;

const MAX_API_BODY: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub source: Source,
    /// The running app's version, with or without a leading `v`.
    pub current: String,
    /// Install-section marker; an empty string keeps whole release bodies.
    pub marker: String,
    pub include_prereleases: bool,
    /// Optional API token. Sent to the API host only, never to downloads.
    pub token: Option<String>,
    /// Where to keep the ETag cache. `None` disables caching.
    pub cache_dir: Option<PathBuf>,
}

impl CheckOptions {
    pub fn new(source: Source, current: impl Into<String>) -> CheckOptions {
        CheckOptions {
            source,
            current: current.into(),
            marker: DEFAULT_MARKER.to_string(),
            include_prereleases: false,
            token: None,
            cache_dir: default_cache_dir(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: String,
    pub tag: String,
    pub name: Option<String>,
    pub published_at: Option<String>,
    pub url: Option<String>,
    pub prerelease: bool,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CheckResult {
    pub source: String,
    pub current: String,
    /// Highest eligible release version, even when it is not newer.
    pub latest: Option<String>,
    pub update_available: bool,
    /// Every release newer than `current`, newest first.
    pub releases: Vec<ReleaseInfo>,
}

/// `$XDG_CACHE_HOME/praeco`, else `~/.cache/praeco`.
pub fn default_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("praeco"))
}

pub fn parse_current(current: &str) -> Result<Version> {
    version::parse(current).ok_or_else(|| {
        Error::new(ErrorCode::InvalidVersion, format!("'{current}' is not a version number like 1.2.3"))
    })
}

/// Pure comparison step: which of `releases` are newer than `current`.
pub fn evaluate(source_label: &str, releases: &[Release], current: &Version, current_text: &str, marker: &str) -> CheckResult {
    let mut sorted: Vec<&Release> = releases.iter().collect();
    sorted.sort_by(|a, b| b.version.cmp(&a.version));
    let latest = sorted.first().map(|r| r.version.to_string());
    let newer: Vec<ReleaseInfo> = sorted
        .iter()
        .filter(|r| r.version > *current && !version::same(&r.version, current))
        .map(|r| ReleaseInfo {
            version: r.version.to_string(),
            tag: r.tag.clone(),
            name: r.name.clone(),
            published_at: r.published_at.clone(),
            url: r.html_url.clone(),
            prerelease: r.prerelease,
            notes: strip_install_section(&r.body, marker),
        })
        .collect();
    CheckResult {
        source: source_label.to_string(),
        current: current_text.trim().trim_start_matches(['v', 'V']).to_string(),
        latest,
        update_available: !newer.is_empty(),
        releases: newer,
    }
}

pub fn check(opts: &CheckOptions) -> Result<CheckResult> {
    check_with(&Http::new(), opts)
}

pub fn check_with(http: &Http, opts: &CheckOptions) -> Result<CheckResult> {
    check_full(http, opts).map(|(result, _)| result)
}

/// Like `check_with`, also returning the eligible releases (for asset selection).
pub fn check_full(http: &Http, opts: &CheckOptions) -> Result<(CheckResult, Vec<Release>)> {
    let current = parse_current(&opts.current)?;
    let releases = fetch_releases(http, &opts.source, opts.token.as_deref(), opts.cache_dir.as_deref(), opts.include_prereleases)?;
    let result = evaluate(&opts.source.label(), &releases, &current, &opts.current, &opts.marker);
    Ok((result, releases))
}

#[derive(Serialize, Deserialize)]
struct Cache {
    etag: String,
    body: String,
}

fn cache_file(dir: &Path, source: &Source) -> PathBuf {
    let name: String = source.label().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    dir.join(format!("{name}.json"))
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if let Ok(bytes) = serde_json::to_vec(cache) {
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Fetch and parse the release list, using a conditional request when a cached
/// ETag exists (a 304 does not count against GitHub's rate limit).
pub fn fetch_releases(
    http: &Http,
    source: &Source,
    token: Option<&str>,
    cache_dir: Option<&Path>,
    include_prereleases: bool,
) -> Result<Vec<Release>> {
    let url = Url::parse(&source.releases_url()).map_err(|e| Error::new(ErrorCode::Usage, format!("Bad release URL: {e}")))?;
    let cache_path = cache_dir.map(|d| cache_file(d, source));
    let cached = cache_path.as_deref().and_then(read_cache);

    let mut extra: Vec<(&str, &str)> = Vec::new();
    if let Some(c) = &cached {
        extra.push(("If-None-Match", c.etag.as_str()));
    }
    let accept = match source.host {
        Host::GitHub { .. } => {
            extra.push(("X-GitHub-Api-Version", "2022-11-28"));
            "application/vnd.github+json"
        }
        Host::Forgejo { .. } => "application/json",
    };
    let auth = token.map(|t| source.auth_header(t));
    let resp = http.get(&url, accept, auth.as_deref(), &extra)?;

    match resp.status() {
        200 => {
            let etag = resp.header("ETag").map(str::to_string);
            let bytes = read_body(resp, MAX_API_BODY)?;
            let releases = parse_releases(&bytes, include_prereleases)?;
            if let (Some(path), Some(etag), Ok(body)) = (&cache_path, etag, String::from_utf8(bytes)) {
                write_cache(path, &Cache { etag, body });
            }
            Ok(releases)
        }
        304 => match cached {
            Some(c) => parse_releases(c.body.as_bytes(), include_prereleases),
            None => Err(Error::new(ErrorCode::Api, "The server said nothing changed, but there is nothing cached to compare with")),
        },
        _ => Err(api_error(source, &resp)),
    }
}

fn api_error(source: &Source, resp: &ureq::Response) -> Error {
    let status = resp.status();
    let slug = format!("{}/{}", source.owner, source.repo);
    match status {
        404 => Error::new(ErrorCode::NoRelease, format!("Could not find releases for {slug}. Is the repository public and spelled correctly?")),
        401 => Error::new(ErrorCode::Api, "The release server did not accept the access token"),
        403 | 429 => {
            let exhausted = resp.header("X-RateLimit-Remaining") == Some("0") || status == 429 || resp.header("Retry-After").is_some();
            if exhausted {
                let when = resp
                    .header("X-RateLimit-Reset")
                    .and_then(|v| v.parse::<u64>().ok())
                    .and_then(|reset| {
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                        Some(reset.saturating_sub(now).div_ceil(60).max(1))
                    })
                    .map(|m| format!(" Try again in about {m} minute{}.", if m == 1 { "" } else { "s" }))
                    .unwrap_or_else(|| " Try again later.".to_string());
                Error::new(ErrorCode::RateLimited, format!("The release server is limiting requests.{when}"))
            } else {
                Error::new(ErrorCode::Api, "The release server refused the request (HTTP 403)")
            }
        }
        500..=599 => Error::new(ErrorCode::Api, format!("The release server had a problem (HTTP {status}). Try again later.")),
        _ => Error::new(ErrorCode::Api, format!("The release server answered with an unexpected status (HTTP {status})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::parse_releases;

    fn rels() -> Vec<Release> {
        let json = r#"[
          {"tag_name":"v1.0.3","body":"three","assets":[]},
          {"tag_name":"v1.0.10","body":"ten\n\n## Install\nsteps","assets":[]},
          {"tag_name":"v1.0.5","name":"Five","body":"five","assets":[]},
          {"tag_name":"v1.0.4","body":"four","assets":[]}
        ]"#;
        parse_releases(json.as_bytes(), false).unwrap()
    }

    #[test]
    fn every_newer_release_newest_first() {
        let cur = parse_current("v1.0.4").unwrap();
        let r = evaluate("s", &rels(), &cur, "v1.0.4", DEFAULT_MARKER);
        assert!(r.update_available);
        assert_eq!(r.current, "1.0.4");
        assert_eq!(r.latest.as_deref(), Some("1.0.10"));
        let v: Vec<_> = r.releases.iter().map(|r| (r.version.as_str(), r.notes.as_str())).collect();
        assert_eq!(v, [("1.0.10", "ten"), ("1.0.5", "five")]);
    }

    #[test]
    fn up_to_date_and_ahead() {
        let r = evaluate("s", &rels(), &parse_current("1.0.10").unwrap(), "1.0.10", DEFAULT_MARKER);
        assert!(!r.update_available && r.releases.is_empty());
        assert_eq!(r.latest.as_deref(), Some("1.0.10"));
        let r = evaluate("s", &rels(), &parse_current("2.0.0").unwrap(), "2.0.0", DEFAULT_MARKER);
        assert!(!r.update_available);
    }

    #[test]
    fn no_releases() {
        let r = evaluate("s", &[], &parse_current("1.0.0").unwrap(), "1.0.0", DEFAULT_MARKER);
        assert!(!r.update_available && r.latest.is_none());
    }

    #[test]
    fn bad_current_version() {
        assert_eq!(parse_current("abc").unwrap_err().code, ErrorCode::InvalidVersion);
    }
}
