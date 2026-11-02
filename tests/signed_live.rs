//! Needs the network: checks the whole signed path against a real signed
//! release (jdx/hk 2.3.0). Run with `cargo test --test signed_live -- --ignored`.
#![cfg(feature = "signed")]

use praeco::signed::{self, Plan};
use praeco::{http::Http, release::Release, Env, Source};

fn hk_release(http: &Http, source: &Source) -> Release {
    praeco::check::fetch_releases(http, source, std::env::var("GITHUB_TOKEN").ok().as_deref(), None, true)
        .unwrap()
        .into_iter()
        .find(|r| r.tag == "v2.3.0")
        .expect("hk v2.3.0 exists")
}

#[test]
#[ignore = "network"]
fn real_signed_release_verifies_and_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = Env::from_process();
    env.home = Some(dir.path().to_path_buf());
    env.config_home = Some(dir.path().join("config"));
    let http = Http::new();
    let source = Source::github("jdx/hk").unwrap();
    let release = hk_release(&http, &source);
    let token = std::env::var("GITHUB_TOKEN").ok();

    let Plan::Signed(signed) = signed::plan(&http, &source, &release, token.as_deref(), &env).unwrap() else {
        panic!("hk 2.3.0 is signed");
    };
    let asset = release.assets.iter().find(|a| a.name == "hk-x86_64-unknown-linux-gnu.tar.gz").expect("asset");
    let file = praeco::verify::download(&http, &source, &release, asset, dir.path(), &mut |_, _| {}).unwrap();
    signed.verify_file(&file).unwrap();
    let store = signed::Store::load(&env).unwrap();
    let rec = store.get("github.com/jdx/hk").expect("remembered");
    assert_eq!(rec.version, "2.3.0");
    assert!(rec.pin.is_some());

    // A different file under a listed name must fail.
    std::fs::write(&file, b"tampered").unwrap();
    assert!(signed.verify_file(&file).is_err());
}
