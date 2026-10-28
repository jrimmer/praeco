mod common;
use common::*;
use praeco::error::{Error, ErrorCode};
use praeco::install::Mode;
use praeco::updateall::{attempts, first_that_works, UpdateMode};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn native(name: &str) -> String {
    match std::env::consts::ARCH {
        "x86_64" => name.to_string(),
        "aarch64" => name.replace("x86_64", "aarch64").replace("amd64", "arm64"),
        arch => name.replace("x86_64", arch),
    }
}

/// A fake home, so nothing here touches the real config, and a fake os-release.
struct Home {
    tmp: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        Home { tmp }
    }
    fn dir(&self) -> PathBuf {
        self.tmp.path().join("home")
    }
    fn os_release(&self, id: &str) -> PathBuf {
        let p = self.tmp.path().join("os-release");
        std::fs::write(&p, format!("ID={id}\n")).unwrap();
        p
    }
    fn praeco(&self, os_id: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_praeco"));
        c.env_remove("APPIMAGE").env_remove("APPDIR").env_remove("PRAECO_ESCALATION_CMD").env_remove("PRAECO_TOKEN").env_remove("TERMINAL").env_remove("PRAECO_TRUSTED_DIRS");
        c.env("HOME", self.dir()).env_remove("XDG_CONFIG_HOME").env("XDG_CACHE_HOME", self.tmp.path().join("cache"));
        c.env("PRAECO_OS_RELEASE", self.os_release(os_id)).env("PRAECO_SYSTEM_APPS_DIR", self.tmp.path().join("no-system-apps"));
        c.stdin(Stdio::null());
        c
    }
    fn registry_file(&self, name: &str) -> PathBuf {
        self.dir().join(format!(".config/praeco/apps.d/{name}.json"))
    }
    fn script_app(&self, name: &str, version: &str) -> PathBuf {
        let p = self.dir().join("bin").join(name);
        write_exec(&p, &format!("#!/bin/sh\necho \"{name} {version}\"\n"));
        p
    }
}

fn json(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("bad JSON {text:?}: {e}; stderr: {}", String::from_utf8_lossy(&out.stderr)))
}

fn init(h: &Home, os: &str, name: &str, server: &FakeServer, exe_flag: &str, exe: &Path) -> Value {
    let out = h
        .praeco(os)
        .args(["init", "--name", name, "--forgejo", &server.base, "o/r", exe_flag, exe.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    json(&out)
}

#[test]
fn init_registers_the_app_reads_its_version_and_adds_the_omarchy_hook() {
    let h = Home::new();
    let s = FakeServer::start();
    let exe = h.script_app("myapp", "1.0.4");
    let v = init(&h, "omarchy", "myapp", &s, "--exe", &exe);
    assert_eq!(v["ok"], true);
    assert_eq!(v["app"]["name"], "myapp");
    assert_eq!(v["version"], "1.0.4");
    assert_eq!(v["registry_file"], h.registry_file("myapp").to_str().unwrap());
    assert!(h.registry_file("myapp").is_file());
    assert_eq!(v["hook"]["status"], "installed");
    let hook = h.dir().join(".config/omarchy/hooks/post-update.d/50-praeco");
    assert_eq!(v["hook"]["path"], hook.to_str().unwrap());
    let text = std::fs::read_to_string(&hook).unwrap();
    assert!(text.contains(env!("CARGO_BIN_EXE_praeco")) && text.contains("update-all"), "{text}");

    // A second app is registered; the shared hook is already there.
    let exe2 = h.script_app("other", "2.0.0");
    let v = init(&h, "omarchy", "other", &s, "--exe", &exe2);
    assert_eq!(v["hook"]["status"], "current");
}

#[test]
fn init_elsewhere_registers_without_a_hook_and_warns_when_the_version_cannot_be_read() {
    let h = Home::new();
    let s = FakeServer::start();
    let exe = h.script_app("myapp", "1.0.4");
    let v = init(&h, "debian", "myapp", &s, "--exe", &exe);
    assert_eq!(v["hook"]["status"], "not_applicable");
    assert!(!h.dir().join(".config/omarchy").exists());

    let missing = h.dir().join("bin/gone");
    let v = init(&h, "omarchy", "gone", &s, "--exe", &missing);
    assert!(v.get("version").is_none());
    assert!(v["version_error"].as_str().unwrap().contains("gone"), "{v}");

    let out = h.praeco("omarchy").args(["init", "--name", "again", "--forgejo", &s.base, "o/r", "--exe", exe.to_str().unwrap(), "--hook", "none", "--json"]).output().unwrap();
    assert_eq!(json(&out)["hook"]["status"], "not_applicable", "--hook none");
}

#[test]
fn init_for_an_appimage_stores_the_file_not_the_mount() {
    let h = Home::new();
    let s = FakeServer::start();
    let img = h.script_app("MyApp.AppImage", "1.0.4");
    let out = h
        .praeco("debian")
        .args(["init", "--name", "myapp", "--forgejo", &s.base, "o/r", "--exe", "/tmp/.mount_MyApp1/usr/bin/myapp", "--json"])
        .env("APPIMAGE", &img)
        .env("APPDIR", "/tmp/.mount_MyApp1")
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(v["app"]["appimage"], true);
    assert_eq!(v["app"]["exe"], img.to_str().unwrap());
    assert_eq!(v["app"]["version_cmd"][0], img.to_str().unwrap());
    assert_eq!(v["version"], "1.0.4");
}

#[test]
fn init_usage_errors() {
    let h = Home::new();
    let s = FakeServer::start();
    for args in [
        vec!["init", "--forgejo", &s.base, "o/r", "--exe", "/usr/bin/x"],
        vec!["init", "--name", "x", "--exe", "/usr/bin/x"],
        vec!["init", "--name", "x", "--forgejo", &s.base, "o/r"],
        vec!["init", "--name", "../x", "--forgejo", &s.base, "o/r", "--exe", "/usr/bin/x"],
        vec!["init", "--name", "x", "--forgejo", &s.base, "o/r", "--exe", "/usr/bin/x", "--hook", "maybe"],
    ] {
        let out = h.praeco("debian").args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(!h.dir().join(".config/praeco").exists(), "nothing registered");
}

fn appimage_app(h: &Home, name: &str, s: &FakeServer) -> PathBuf {
    let img = h.script_app(&format!("{name}.AppImage"), "1.0.4");
    init(h, "debian", name, s, "--appimage", &img);
    img
}

#[test]
fn update_all_updates_silently_and_a_second_run_finds_nothing_to_do() {
    let h = Home::new();
    let s = FakeServer::start();
    let img = appimage_app(&h, "myapp", &s);
    let new = b"#!/bin/sh\necho \"myapp 1.0.5\"\n";
    s.publish_releases(vec![s.publish_one("v1.0.5", &native("myapp-x86_64.AppImage"), new)]);

    let out = h.praeco("debian").args(["update-all", "--mode", "inline", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["apps"][0]["name"], "myapp");
    assert_eq!(v["apps"][0]["status"], "updated");
    assert_eq!(v["apps"][0]["from"], "1.0.4");
    assert_eq!(v["apps"][0]["to"], "1.0.5");
    assert_eq!(std::fs::read(&img).unwrap(), new);

    let out = h.praeco("debian").args(["update-all"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("myapp: up to date (1.0.5)"), "{text}");
}

#[test]
fn one_app_failing_does_not_stop_the_others() {
    let h = Home::new();
    let good = FakeServer::start();
    let dead = FakeServer::start();
    let dead_base = dead.base.clone();
    drop(dead);
    let img = appimage_app(&h, "bbb", &good);
    let exe = h.script_app("aaa", "1.0.0");
    let out = h.praeco("debian").args(["init", "--name", "aaa", "--forgejo", &dead_base, "o/r", "--exe", exe.to_str().unwrap(), "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    good.publish_releases(vec![good.publish_one("v1.0.5", &native("myapp-x86_64.AppImage"), b"#!/bin/sh\necho \"bbb 1.0.5\"\n")]);

    let out = h.praeco("debian").args(["update-all", "--mode", "inline", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "any failure is exit 1");
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["apps"][0]["name"], "aaa");
    assert_eq!(v["apps"][0]["status"], "failed");
    assert_eq!(v["apps"][0]["error"]["code"], "network");
    assert_eq!(v["apps"][1]["name"], "bbb");
    assert_eq!(v["apps"][1]["status"], "updated");
    assert!(std::fs::read_to_string(&img).unwrap().contains("1.0.5"));
}

#[test]
fn uninstalled_apps_are_skipped_and_unreadable_registrations_are_reported() {
    let h = Home::new();
    let s = FakeServer::start();
    init(&h, "debian", "gone", &s, "--exe", &h.dir().join("bin/gone"));
    std::fs::write(h.dir().join(".config/praeco/apps.d/broken.json"), "{ nope").unwrap();
    let out = h.praeco("debian").args(["update-all", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "an uninstalled app is not a failure");
    let v = json(&out);
    assert_eq!(v["apps"][0]["status"], "not_installed");
    assert_eq!(v["problems"].as_array().unwrap().len(), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("broken.json"));
}

#[test]
fn update_all_with_nothing_registered_says_so() {
    let h = Home::new();
    let out = h.praeco("debian").arg("update-all").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("No apps are registered"));
    let out = h.praeco("debian").args(["update-all", "--json"]).output().unwrap();
    assert_eq!(json(&out), serde_json::json!({"ok": true, "apps": [], "problems": []}));
    let out = h.praeco("debian").args(["update-all", "--mode", "sideways"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_package_install_goes_through_the_chosen_escalation() {
    let h = Home::new();
    let s = FakeServer::start();
    let bin = h.tmp.path().join("shims");
    shim(&bin, "pacman", 0);
    let rec = h.tmp.path().join("rec");
    let sudo = escalator(&bin, "fake-sudo", &rec, 0, "");
    let exe = h.script_app("myapp", "1.0.4");
    init(&h, "debian", "myapp", &s, "--exe", &exe);
    s.publish_releases(vec![s.publish_one("v1.0.5", &native("myapp-x86_64.pkg.tar.zst"), b"PKG")]);

    let out = h
        .praeco("debian")
        .args(["update-all", "--mode", "inline", "--json"])
        .env("PRAECO_TRUSTED_DIRS", &bin)
        .env("PRAECO_ESCALATION_CMD", &sudo)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(json(&out)["apps"][0]["status"], "updated");
    let argv = recorded(&rec);
    assert!(argv.iter().any(|a| a.ends_with("/pacman")) && argv.contains(&"-U".to_string()), "{argv:?}");
}

#[test]
fn how_administrator_rights_are_sought() {
    use UpdateMode::*;
    assert_eq!(attempts(Inline, false), [(Mode::Terminal, true)]);
    assert_eq!(attempts(Auto, true), [(Mode::Terminal, true)], "a terminal in hand: sudo there, no new prompt");
    assert_eq!(attempts(Auto, false), [(Mode::Dialog, false), (Mode::Terminal, false)], "no terminal: the prompt, then a terminal window");
    assert_eq!(attempts(Dialog, true), [(Mode::Dialog, false)]);
    assert_eq!(attempts(Terminal, true), [(Mode::Terminal, false)]);
    assert_eq!(UpdateMode::parse("auto"), Some(Auto));
    assert_eq!(UpdateMode::parse("nope"), None);
}

#[test]
fn only_needs_terminal_falls_through_to_the_next_attempt() {
    let steps = [(Mode::Dialog, false), (Mode::Terminal, false)];
    let mut tried = Vec::new();
    let r = first_that_works(&steps, |m, _| {
        tried.push(m);
        if m == Mode::Dialog { Err(Error::new(ErrorCode::NeedsTerminal, "no agent")) } else { Ok("installed") }
    });
    assert_eq!(r.unwrap(), "installed");
    assert_eq!(tried, [Mode::Dialog, Mode::Terminal]);

    let mut tried = 0;
    let r: Result<(), _> = first_that_works(&steps, |_, _| {
        tried += 1;
        Err(Error::new(ErrorCode::AuthCancelled, "dismissed"))
    });
    assert_eq!(r.unwrap_err().code, ErrorCode::AuthCancelled);
    assert_eq!(tried, 1, "a dismissed prompt is the user's answer, not a reason to open a terminal");

    let r: Result<(), _> = first_that_works(&steps, |_, _| Err(Error::new(ErrorCode::NeedsTerminal, "still none")));
    assert_eq!(r.unwrap_err().code, ErrorCode::NeedsTerminal, "the last attempt's error is reported");
}
