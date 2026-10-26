//! Release sources (GitHub, Forgejo/Gitea) and the rule for which asset URLs
//! may be downloaded.

use crate::error::{Error, ErrorCode, Result};
use crate::release::Asset;
use url::Url;

pub const GITHUB_API: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    GitHub { api_base: String },
    Forgejo { base: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub host: Host,
    pub owner: String,
    pub repo: String,
}

fn split_slug(slug: &str) -> Result<(String, String)> {
    let bad = || Error::new(ErrorCode::Usage, format!("'{slug}' is not a repository in owner/repo form"));
    let (o, r) = slug.split_once('/').ok_or_else(bad)?;
    let ok = |s: &str| {
        !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !ok(o) || !ok(r) {
        return Err(bad());
    }
    Ok((o.to_string(), r.to_string()))
}

pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// HTTPS always; plain HTTP only to a loopback address, and only when
/// `loopback_http` says the configured source itself is a loopback one (tests,
/// local mirrors). A remote server can therefore never steer praeco to
/// `http://127.0.0.1`.
pub fn scheme_allowed(url: &Url, loopback_http: bool) -> bool {
    url.scheme() == "https" || (loopback_http && url.scheme() == "http" && is_loopback(url))
}

impl Source {
    pub fn github(slug: &str) -> Result<Source> {
        Source::github_with_api_base(slug, GITHUB_API)
    }

    /// GitHub-shaped API at a non-default base (GitHub Enterprise, tests).
    pub fn github_with_api_base(slug: &str, api_base: &str) -> Result<Source> {
        let (owner, repo) = split_slug(slug)?;
        Ok(Source { host: Host::GitHub { api_base: api_base.trim_end_matches('/').to_string() }, owner, repo })
    }

    pub fn forgejo(base: &str, slug: &str) -> Result<Source> {
        let (owner, repo) = split_slug(slug)?;
        let url = Url::parse(base).map_err(|e| Error::new(ErrorCode::Usage, format!("'{base}' is not a valid URL: {e}")))?;
        if !scheme_allowed(&url, true) {
            return Err(Error::new(ErrorCode::Usage, "The Forgejo address must start with https://"));
        }
        Ok(Source { host: Host::Forgejo { base: base.trim_end_matches('/').to_string() }, owner, repo })
    }

    /// Is the configured source itself a plain-HTTP loopback address? Only then
    /// may HTTP to loopback be used at all.
    pub fn loopback_http(&self) -> bool {
        let base = match &self.host {
            Host::GitHub { api_base } => api_base,
            Host::Forgejo { base } => base,
        };
        Url::parse(base).map(|u| u.scheme() == "http" && is_loopback(&u)).unwrap_or(false)
    }

    pub fn releases_url(&self) -> String {
        match &self.host {
            Host::GitHub { api_base } => format!("{api_base}/repos/{}/{}/releases?per_page=100", self.owner, self.repo),
            Host::Forgejo { base } => format!("{base}/api/v1/repos/{}/{}/releases?limit=50", self.owner, self.repo),
        }
    }

    /// Short stable label, e.g. `github:you/myapp`.
    pub fn label(&self) -> String {
        match &self.host {
            Host::GitHub { api_base } if api_base == GITHUB_API => format!("github:{}/{}", self.owner, self.repo),
            Host::GitHub { api_base } => format!("github({api_base}):{}/{}", self.owner, self.repo),
            Host::Forgejo { base } => format!("forgejo({base}):{}/{}", self.owner, self.repo),
        }
    }

    pub fn auth_header(&self, token: &str) -> String {
        match self.host {
            Host::GitHub { .. } => format!("Bearer {token}"),
            Host::Forgejo { .. } => format!("token {token}"),
        }
    }

    /// May this URL be downloaded? It must be HTTPS (or loopback HTTP for a
    /// loopback source), on the source's own host, and under that repository's
    /// release-download path. Forgejo's legacy `/attachments/<id>` URLs are
    /// accepted only when `listed` (the release's own asset list) names that
    /// exact URL.
    pub fn allows_download(&self, url: &Url, listed: &[Asset]) -> bool {
        if !scheme_allowed(url, self.loopback_http()) {
            return false;
        }
        let slug = format!("/{}/{}/releases/download/", self.owner, self.repo).to_lowercase();
        let path = url.path().to_lowercase();
        match &self.host {
            Host::GitHub { api_base } if api_base == GITHUB_API => {
                url.host_str() == Some("github.com") && path.starts_with(&slug)
            }
            Host::GitHub { api_base } => {
                let Ok(b) = Url::parse(api_base) else { return false };
                same_origin(&b, url) && path.starts_with(&slug)
            }
            Host::Forgejo { base } => {
                let Ok(b) = Url::parse(base) else { return false };
                if !same_origin(&b, url) {
                    return false;
                }
                let prefix = b.path().trim_end_matches('/').to_lowercase();
                if path.starts_with(&format!("{prefix}{slug}")) {
                    return true;
                }
                let attach = format!("{prefix}/attachments/");
                let single_segment = url.path().to_lowercase().strip_prefix(&attach).is_some_and(|r| !r.is_empty() && !r.contains('/'));
                single_segment && listed.iter().any(|a| a.url == url.as_str())
            }
        }
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn slug_validation() {
        assert!(Source::github("you/myapp").is_ok());
        for bad in ["myapp", "a/b/c", "../x", "a/..", "a b/c", "/c", "a/"] {
            assert!(Source::github(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls() {
        let g = Source::github("you/myapp").unwrap();
        assert_eq!(g.releases_url(), "https://api.github.com/repos/you/myapp/releases?per_page=100");
        let f = Source::forgejo("https://code.example.com/", "o/r").unwrap();
        assert_eq!(f.releases_url(), "https://code.example.com/api/v1/repos/o/r/releases?limit=50");
        assert!(Source::forgejo("http://code.example.com", "o/r").is_err());
        assert!(Source::forgejo("http://127.0.0.1:3000", "o/r").is_ok());
    }

    #[test]
    fn github_download_rules() {
        let g = Source::github("you/myapp").unwrap();
        let ok = |s: &str| g.allows_download(&u(s), &[]);
        assert!(ok("https://github.com/you/myapp/releases/download/v1/a.deb"));
        assert!(ok("https://github.com/You/MyApp/releases/download/v1/a.deb"));
        assert!(!ok("https://github.com/evil/myapp/releases/download/v1/a.deb"));
        assert!(!ok("https://evil.com/you/myapp/releases/download/v1/a.deb"));
        assert!(!ok("http://github.com/you/myapp/releases/download/v1/a.deb"));
        assert!(!ok("https://github.com/you/myapp/archive/v1.tar.gz"));
    }

    #[test]
    fn forgejo_download_rules() {
        let f = Source::forgejo("https://code.example.com", "o/r").unwrap();
        let ok = |s: &str| f.allows_download(&u(s), &[]);
        assert!(ok("https://code.example.com/o/r/releases/download/v1/a.deb"));
        assert!(!ok("https://code.example.com/x/y/releases/download/v1/a.deb"));
        assert!(!ok("https://other.example.com/o/r/releases/download/v1/a.deb"));
    }

    #[test]
    fn forgejo_attachments_only_when_listed_exactly() {
        let f = Source::forgejo("https://code.example.com", "o/r").unwrap();
        let url = "https://code.example.com/attachments/abc-123";
        let listed = [Asset { name: "a.deb".into(), url: url.into(), size: 1 }];
        assert!(f.allows_download(&u(url), &listed));
        assert!(!f.allows_download(&u(url), &[]), "any /attachments/ URL on the host is not enough");
        assert!(!f.allows_download(&u("https://code.example.com/attachments/other"), &listed));
        assert!(!f.allows_download(&u("https://code.example.com/attachments/abc-123/extra"), &listed));
    }

    #[test]
    fn github_enterprise_prefix_is_anchored() {
        let g = Source::github_with_api_base("o/r", "https://ghe.example.com/api/v3").unwrap();
        assert!(g.allows_download(&u("https://ghe.example.com/o/r/releases/download/v1/a"), &[]));
        assert!(!g.allows_download(&u("https://ghe.example.com/evil/x/o/r/releases/download/v1/a"), &[]));
        assert!(!g.allows_download(&u("https://ghe.example.com/uploads/o/r/releases/download/v1/a"), &[]));
    }

    #[test]
    fn loopback_http_only_for_loopback_sources() {
        let lo = u("http://127.0.0.1:9/x");
        assert!(!scheme_allowed(&lo, false), "a remote source's redirect to loopback is refused");
        assert!(scheme_allowed(&lo, true));
        assert!(!scheme_allowed(&u("http://example.com/"), true));
        assert!(Source::forgejo("http://127.0.0.1:3000", "o/r").unwrap().loopback_http());
        assert!(!Source::forgejo("https://code.example.com", "o/r").unwrap().loopback_http());
        assert!(!Source::github("o/r").unwrap().loopback_http());
        let remote = Source::forgejo("https://code.example.com", "o/r").unwrap();
        assert!(!remote.allows_download(&u("http://127.0.0.1:9/o/r/releases/download/v1/a"), &[]));
    }
}
