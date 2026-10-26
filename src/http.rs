//! Blocking HTTPS client (ureq + rustls). Redirects are followed by hand so
//! every hop can be held to the HTTPS-only rule, and credentials never leave
//! the host they were meant for.

use crate::error::{Error, ErrorCode, Result};
use crate::source::scheme_allowed;
use std::io::Read;
use std::time::Duration;
use url::Url;

pub struct Http {
    agent: ureq::Agent,
    /// Plain HTTP to loopback is allowed (only for loopback sources).
    loopback_http: bool,
}

impl Default for Http {
    fn default() -> Http {
        Http::new()
    }
}

impl Http {
    /// Strict client: HTTPS only, on every hop.
    pub fn new() -> Http {
        Http::build(false)
    }

    /// Client for `source`: strict, except that a source that is itself a
    /// loopback HTTP address may use loopback HTTP (tests, local mirrors).
    pub fn for_source(source: &crate::source::Source) -> Http {
        Http::build(source.loopback_http())
    }

    /// May a request (or redirect hop) go to `url`?
    pub fn hop_allowed(&self, url: &Url) -> bool {
        scheme_allowed(url, self.loopback_http)
    }

    fn build(loopback_http: bool) -> Http {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .user_agent(concat!("praeco/", env!("CARGO_PKG_VERSION")))
            .try_proxy_from_env(true)
            .build();
        Http { agent, loopback_http }
    }

    /// GET with manual redirects. Returns the final response whatever its
    /// status; the caller decides what a 404 or 403 means. `auth` is sent only
    /// to the original host.
    pub fn get(&self, url: &Url, accept: &str, auth: Option<&str>, extra: &[(&str, &str)]) -> Result<ureq::Response> {
        let mut cur = url.clone();
        for _ in 0..6 {
            if !self.hop_allowed(&cur) {
                return Err(Error::new(ErrorCode::Network, format!("Refusing to use an insecure connection to {}", cur.host_str().unwrap_or("?"))));
            }
            let mut req = self.agent.get(cur.as_str()).set("Accept", accept);
            if let Some(a) = auth {
                if cur.host_str() == url.host_str() && cur.port_or_known_default() == url.port_or_known_default() {
                    req = req.set("Authorization", a);
                }
            }
            for (k, v) in extra {
                req = req.set(k, v);
            }
            let resp = match req.call() {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(ureq::Error::Transport(t)) => {
                    return Err(Error::new(
                        ErrorCode::Network,
                        format!("Could not reach {}. Check the internet connection. ({t})", cur.host_str().unwrap_or("the server")),
                    ));
                }
            };
            if matches!(resp.status(), 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = resp.header("Location") {
                    cur = cur
                        .join(loc)
                        .map_err(|e| Error::new(ErrorCode::Network, format!("The server sent a bad redirect: {e}")))?;
                    continue;
                }
            }
            return Ok(resp);
        }
        Err(Error::new(ErrorCode::Network, "The server redirected too many times"))
    }
}

/// Read a whole body, refusing anything larger than `max` bytes.
pub fn read_body(resp: ureq::Response, max: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::new(ErrorCode::Network, format!("The connection dropped while reading the response: {e}")))?;
    if buf.len() as u64 > max {
        return Err(Error::new(ErrorCode::Api, "The server's response was unexpectedly large"));
    }
    Ok(buf)
}
