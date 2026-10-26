mod common;
use common::*;
use praeco::check::{check, CheckOptions};
use praeco::source::Source;
use praeco::ErrorCode;
use serde_json::json;

fn opts(server: &FakeServer, current: &str) -> CheckOptions {
    let mut o = CheckOptions::new(server.source(), current);
    o.cache_dir = None;
    o
}

fn fairfax_releases(s: &FakeServer) -> Vec<serde_json::Value> {
    let body = |n: &str| format!("### Changes in {n}\n- something\n\n## Install\n\nsudo pacman -U fairfax.pkg.tar.zst\n");
    let mut draft = s.release("v1.0.7", "draft", &[]);
    draft["draft"] = json!(true);
    let mut pre = s.release("v1.1.0-rc1", "rc", &[]);
    pre["prerelease"] = json!(true);
    vec![
        s.release("v1.0.3", &body("1.0.3"), &[]),
        s.release("v1.0.6", &body("1.0.6"), &[]),
        s.release("v1.0.4", &body("1.0.4"), &[]),
        s.release("v1.0.5", &body("1.0.5"), &[]),
        draft,
        pre,
    ]
}

#[test]
fn newer_releases_with_notes_newest_first() {
    let s = FakeServer::start();
    s.publish_releases(fairfax_releases(&s));
    let r = check(&opts(&s, "v1.0.4")).unwrap();
    assert!(r.update_available);
    assert_eq!(r.latest.as_deref(), Some("1.0.6"));
    assert_eq!(r.current, "1.0.4");
    let versions: Vec<_> = r.releases.iter().map(|x| x.version.as_str()).collect();
    assert_eq!(versions, ["1.0.6", "1.0.5"]);
    assert_eq!(r.releases[0].notes, "### Changes in 1.0.6\n- something");
    assert!(!r.releases.iter().any(|x| x.notes.contains("pacman")));
    assert_eq!(r.releases[0].tag, "v1.0.6");
    assert!(r.releases[0].url.as_deref().unwrap().ends_with("/releases/tag/v1.0.6"));
}

#[test]
fn prereleases_only_when_asked() {
    let s = FakeServer::start();
    s.publish_releases(fairfax_releases(&s));
    let mut o = opts(&s, "1.0.6");
    assert!(!check(&o).unwrap().update_available);
    o.include_prereleases = true;
    let r = check(&o).unwrap();
    assert_eq!(r.latest.as_deref(), Some("1.1.0-rc1"));
    assert!(r.releases[0].prerelease);
}

#[test]
fn custom_marker() {
    let s = FakeServer::start();
    s.publish_releases(vec![s.release("v2.0.0", "Notes here\n\n## How to update\nsteps", &[])]);
    let mut o = opts(&s, "1.0.0");
    o.marker = "## How to update".into();
    assert_eq!(check(&o).unwrap().releases[0].notes, "Notes here");
}

#[test]
fn up_to_date() {
    let s = FakeServer::start();
    s.publish_releases(fairfax_releases(&s));
    let r = check(&opts(&s, "1.0.6")).unwrap();
    assert!(!r.update_available && r.releases.is_empty());
    assert_eq!(r.latest.as_deref(), Some("1.0.6"));
}

#[test]
fn errors_are_clean_results() {
    let s = FakeServer::start();
    // Nothing published: 404.
    assert_eq!(check(&opts(&s, "1.0.0")).unwrap_err().code, ErrorCode::NoRelease);

    s.route(FakeServer::releases_path(), 500, &[], "boom");
    assert_eq!(check(&opts(&s, "1.0.0")).unwrap_err().code, ErrorCode::Api);

    s.route(FakeServer::releases_path(), 200, &[], "<html>not json</html>");
    assert_eq!(check(&opts(&s, "1.0.0")).unwrap_err().code, ErrorCode::Api);

    s.route(FakeServer::releases_path(), 403, &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", "9999999999")], "limit");
    let e = check(&opts(&s, "1.0.0")).unwrap_err();
    assert_eq!(e.code, ErrorCode::RateLimited);
    assert!(e.message.contains("Try again"), "{}", e.message);

    s.route(FakeServer::releases_path(), 403, &[], "nope");
    assert_eq!(check(&opts(&s, "1.0.0")).unwrap_err().code, ErrorCode::Api);

    assert_eq!(check(&opts(&s, "not-a-version")).unwrap_err().code, ErrorCode::InvalidVersion);
}

#[test]
fn offline_is_a_network_error_not_a_panic() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let src = Source::forgejo(&format!("http://127.0.0.1:{port}"), "o/r").unwrap();
    let mut o = CheckOptions::new(src, "1.0.0");
    o.cache_dir = None;
    let e = check(&o).unwrap_err();
    assert_eq!(e.code, ErrorCode::Network);
    assert!(e.message.contains("internet"), "{}", e.message);
}

#[test]
fn etag_cache_uses_conditional_requests() {
    let s = FakeServer::start();
    let json = serde_json::to_vec(&fairfax_releases(&s)).unwrap();
    s.route(FakeServer::releases_path(), 200, &[("ETag", "\"abc123\"")], json);
    let cache = tempfile::tempdir().unwrap();
    let mut o = opts(&s, "1.0.4");
    o.cache_dir = Some(cache.path().to_path_buf());

    let first = check(&o).unwrap();
    let second = check(&o).unwrap();
    assert_eq!(first, second);
    assert!(second.update_available);

    let reqs = s.requests_for(FakeServer::releases_path());
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].header("if-none-match"), None);
    assert_eq!(reqs[1].header("if-none-match"), Some("\"abc123\""));
}

#[test]
fn token_goes_to_the_api_with_the_right_scheme() {
    let s = FakeServer::start();
    s.publish_releases(vec![]);
    let mut o = opts(&s, "1.0.0");
    o.token = Some("sekrit".into());
    check(&o).unwrap();
    let reqs = s.requests_for(FakeServer::releases_path());
    assert_eq!(reqs[0].header("authorization"), Some("token sekrit"));

    // GitHub-shaped API
    s.route("/repos/o/r/releases", 200, &[], "[]");
    let mut g = CheckOptions::new(Source::github_with_api_base("o/r", &s.base).unwrap(), "1.0.0");
    g.cache_dir = None;
    g.token = Some("sekrit".into());
    check(&g).unwrap();
    let reqs = s.requests_for("/repos/o/r/releases");
    assert_eq!(reqs[0].header("authorization"), Some("Bearer sekrit"));
    assert_eq!(reqs[0].header("accept"), Some("application/vnd.github+json"));

    // No token by default.
    let o = opts(&s, "1.0.0");
    check(&o).unwrap();
    assert_eq!(s.requests_for(FakeServer::releases_path()).last().unwrap().header("authorization"), None);
}
