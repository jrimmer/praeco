//! Release model and parsing. GitHub and Forgejo/Gitea return the same shape
//! for the fields praeco uses, so one parser serves both.

use crate::error::{Error, ErrorCode, Result};
use crate::version;
use semver::Version;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub name: Option<String>,
    pub body: String,
    pub prerelease: bool,
    pub html_url: Option<String>,
    pub published_at: Option<String>,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    html_url: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

/// Parse an API response. Drafts are always dropped; prereleases (by flag or
/// by a semver pre-release part) only when `include_prereleases`. Releases
/// whose tag is not a version are skipped.
pub fn parse_releases(json: &[u8], include_prereleases: bool) -> Result<Vec<Release>> {
    let raw: Vec<RawRelease> = serde_json::from_slice(json)
        .map_err(|e| Error::new(ErrorCode::Api, format!("The release server sent something praeco could not read: {e}")))?;
    let mut out = Vec::new();
    for r in raw {
        if r.draft {
            continue;
        }
        let Some(v) = version::parse(&r.tag_name) else { continue };
        let pre = r.prerelease || !v.pre.is_empty();
        if pre && !include_prereleases {
            continue;
        }
        let assets = r
            .assets
            .into_iter()
            .filter_map(|a| Some(Asset { name: a.name, url: a.browser_download_url?, size: a.size.unwrap_or(0) }))
            .collect();
        out.push(Release {
            tag: r.tag_name,
            version: v,
            name: r.name.filter(|n| !n.trim().is_empty()),
            body: r.body.unwrap_or_default(),
            prerelease: pre,
            html_url: r.html_url,
            published_at: r.published_at,
            assets,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"tag_name":"v1.0.5","name":"Fairfax 1.0.5","body":"notes","draft":false,"prerelease":false,
       "html_url":"https://x/r/1.0.5","published_at":"2026-10-01T00:00:00Z",
       "assets":[{"name":"a.deb","browser_download_url":"https://x/a.deb","size":3},{"name":"nourl"}]},
      {"tag_name":"v1.1.0-rc1","draft":false,"prerelease":true,"assets":[]},
      {"tag_name":"v9.9.9","draft":true,"assets":[]},
      {"tag_name":"nightly","draft":false,"prerelease":false,"body":null,"assets":[]},
      {"tag_name":"1.0.4","name":null,"body":null,"published_at":null,"assets":[]}
    ]"#;

    #[test]
    fn filters_drafts_prereleases_and_junk() {
        let r = parse_releases(SAMPLE.as_bytes(), false).unwrap();
        let tags: Vec<_> = r.iter().map(|r| r.tag.as_str()).collect();
        assert_eq!(tags, ["v1.0.5", "1.0.4"]);
        assert_eq!(r[0].assets.len(), 1);
        assert_eq!(r[0].assets[0].size, 3);
        assert_eq!(r[1].body, "");
    }

    #[test]
    fn prereleases_on_request() {
        let r = parse_releases(SAMPLE.as_bytes(), true).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r.iter().any(|r| r.tag == "v1.1.0-rc1" && r.prerelease));
    }

    #[test]
    fn garbage_is_an_api_error() {
        assert_eq!(parse_releases(b"<html>", false).unwrap_err().code, ErrorCode::Api);
        assert_eq!(parse_releases(b"{\"message\":\"x\"}", false).unwrap_err().code, ErrorCode::Api);
    }
}
