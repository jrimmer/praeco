mod common;
use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn praeco() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_praeco"));
    c.env_remove("APPIMAGE").env_remove("APPDIR").env_remove("PRAECO_ESCALATION_CMD").env_remove("PRAECO_TOKEN").env_remove("TERMINAL").env_remove("PRAECO_TRUSTED_DIRS");
    c
}

fn json_lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}"))).collect()
}

fn wait_for(path: &Path) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn base_args<'a>(s: &'a FakeServer, sub: &'a str) -> Vec<&'a str> {
    vec![sub, "--forgejo", s.base.as_str(), "o/r"]
}

#[test]
fn check_json() {
    let s = FakeServer::start();
    s.publish_releases(vec![
        s.release("v1.0.5", "- fix\n\n## Install\nsteps", &[]),
        s.release("v1.0.4", "- older", &[]),
        s.release("v1.0.3", "- oldest", &[]),
    ]);
    let mut args = base_args(&s, "check");
    args.extend(["--current", "v1.0.3", "--no-cache", "--json"]);
    let out = praeco().args(&args).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lines = json_lines(&out);
    assert_eq!(lines.len(), 1);
    let v = &lines[0];
    assert_eq!(v["ok"], true);
    assert_eq!(v["update_available"], true);
    assert_eq!(v["current"], "1.0.3");
    assert_eq!(v["latest"], "1.0.5");
    assert_eq!(v["releases"][0]["version"], "1.0.5");
    assert_eq!(v["releases"][0]["notes"], "- fix");
    assert_eq!(v["releases"][1]["version"], "1.0.4");
    assert!(v.get("install").is_none());
}

#[test]
fn check_with_exe_reports_the_install_kind_and_asset() {
    let s = FakeServer::start();
    s.publish_releases(vec![s.release("v1.0.5", "x", &["fairfax-x86_64.AppImage", "fairfax-amd64.deb"])]);
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("Fairfax.AppImage");
    write_exec(&img, "x");
    let mut args = base_args(&s, "check");
    args.extend(["--current", "1.0.4", "--no-cache", "--json", "--exe", "/tmp/.mount_x/usr/bin/fairfax", "--appimage", img.to_str().unwrap(), "--appdir", "/tmp/.mount_x"]);
    let out = praeco().args(&args).output().unwrap();
    let v = &json_lines(&out)[0];
    assert_eq!(v["install"]["kind"], "appimage");
    assert_eq!(v["install"]["supported"], true);
    assert_eq!(v["install"]["asset"]["name"], "fairfax-x86_64.AppImage");
    assert_eq!(v["install"]["target"], img.to_str().unwrap());
}

#[test]
fn check_errors_are_json_and_exit_1() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let base = format!("http://127.0.0.1:{port}");
    let out = praeco().args(["check", "--forgejo", &base, "o/r", "--current", "1.0.0", "--no-cache", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = &json_lines(&out)[0];
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "network");
    assert!(v["error"]["message"].as_str().unwrap().len() > 10);

    // Without --json: plain text on stderr, nothing on stdout.
    let out = praeco().args(["check", "--forgejo", &base, "o/r", "--current", "1.0.0", "--no-cache"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("praeco: "));
}

#[test]
fn usage_errors_exit_2() {
    for args in [vec!["check", "--current", "1.0.0"], vec!["bogus"], vec!["check", "--github", "nope", "--current", "1"], vec![]] {
        let out = praeco().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    let out = praeco().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("EXIT CODES"));
}

#[test]
fn detect_command() {
    let tmp = tempfile::tempdir().unwrap();
    let out = praeco().env("PATH", tmp.path()).env("PRAECO_TRUSTED_DIRS", tmp.path()).env("HOME", tmp.path()).args(["detect", "--exe", "/usr/bin/praeco-test-nonexistent"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = &json_lines(&out)[0];
    assert_eq!(v["kind"], "unsupported");
    assert_eq!(v["supported"], false);
    assert!(v["reason"].as_str().unwrap().contains("source build"));
}

struct Install {
    tmp: tempfile::TempDir,
    server: FakeServer,
    img: PathBuf,
}

fn appimage_install(content: &[u8]) -> Install {
    let tmp = tempfile::tempdir().unwrap();
    let server = FakeServer::start();
    let rel = server.publish_one("v1.0.5", "fairfax-x86_64.AppImage", content);
    server.publish_releases(vec![rel]);
    let img = tmp.path().join("Fairfax.AppImage");
    write_exec(&img, "OLD");
    Install { tmp, server, img }
}

#[test]
fn install_appimage_then_restart_prints_json_lines() {
    if std::env::consts::ARCH != "x86_64" {
        return;
    }
    let i = appimage_install(b"NEW APPIMAGE");
    let marker = i.tmp.path().join("restarted");
    let old = Command::new("sleep").arg("60").spawn().unwrap();
    let mut old = old;
    let script = format!("echo \"$APPIMAGE|$APPDIR\" > '{}'", marker.display());
    let out = praeco()
        .args(base_args(&i.server, "install"))
        .args(["--version", "1.0.5", "--exe", "/tmp/.mount_x/usr/bin/fairfax", "--appimage", i.img.to_str().unwrap()])
        .args(["--restart-pid", &old.id().to_string(), "--restart", "--", "sh", "-c", &script])
        .env("APPIMAGE", "/stale/old.AppImage")
        .env("APPDIR", "/tmp/.mount_x")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let ev = json_lines(&out);
    let names: Vec<&str> = ev.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(names.first(), Some(&"resolving"));
    assert!(names.contains(&"downloading") && names.contains(&"verifying") && names.contains(&"installing") && names.contains(&"installed"));
    assert_eq!(&names[names.len() - 3..], ["restarting", "restarting", "done"]);
    let done = ev.last().unwrap();
    assert_eq!(done["version"], "1.0.5");
    assert_eq!(done["restarted"], true);
    assert_eq!(std::fs::read_to_string(&i.img).unwrap(), "NEW APPIMAGE");
    assert!(wait_for(&marker));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "|", "stale AppImage variables are not passed on");
    old.wait().unwrap();
}

#[test]
fn install_failures_are_events_with_exit_codes() {
    let i = appimage_install(b"NEW");
    // checksum mismatch -> exit 1, error event last
    i.server.serve_asset("v1.0.5", "fairfax-x86_64.AppImage.sha256", "0".repeat(64).as_bytes());
    let out = praeco()
        .args(base_args(&i.server, "install"))
        .args(["--version", "1.0.5", "--exe", "/tmp/.mount_x/usr/bin/fairfax", "--appdir", "/tmp/.mount_x", "--appimage", i.img.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let ev = json_lines(&out);
    let last = ev.last().unwrap();
    assert_eq!(last["event"], "error");
    assert_eq!(last["code"], "checksum_mismatch");
    assert_eq!(std::fs::read_to_string(&i.img).unwrap(), "OLD");

    // unsupported -> exit 4
    let tmp = tempfile::tempdir().unwrap();
    let out = praeco()
        .env("PATH", tmp.path())
        .env("PRAECO_TRUSTED_DIRS", tmp.path())
        .env("HOME", tmp.path())
        .args(base_args(&i.server, "install"))
        .args(["--version", "1.0.5", "--exe", "/usr/bin/praeco-test-nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(json_lines(&out).last().unwrap()["code"], "unsupported");

    // usage -> exit 2, still JSON on stdout
    let out = praeco().args(base_args(&i.server, "install")).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_lines(&out)[0]["error"]["code"], "usage");
}

#[test]
fn escalation_command_env_var_is_used_for_packages() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    shim(&bin, "pacman", 0);
    let rec = tmp.path().join("rec");
    let esc = escalator(&tmp.path().join("esc"), "recorder", &rec, 0, "");
    let s = FakeServer::start();
    let rel = s.publish_one("v1.0.5", "fairfax-x86_64.pkg.tar.zst", b"PKG");
    s.publish_releases(vec![rel]);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out = praeco()
        .env("PATH", &path)
        .env("PRAECO_TRUSTED_DIRS", &bin)
        .env("PRAECO_ESCALATION_CMD", &esc)
        .args(base_args(&s, "install"))
        .args(["--version", "1.0.5", "--exe", "/usr/bin/praeco-test-nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let argv = recorded(&rec);
    assert_eq!(argv[0], bin.join("pacman").to_str().unwrap());
    assert_eq!(&argv[1..3], ["-U", "--noconfirm"]);
    assert_eq!(std::fs::read_to_string(format!("{}.file", rec.display())).unwrap(), "PKG");
    let ev = json_lines(&out);
    assert_eq!(ev.last().unwrap()["event"], "done");
    assert_eq!(ev.last().unwrap()["restarted"], false);
}

/// A terminal that just runs its command in the foreground.
fn fake_terminal(dir: &Path) {
    write_exec(&dir.join("xdg-terminal-exec"), "#!/bin/sh\nexec \"$@\" </dev/null\n");
}

#[test]
fn terminal_mode_runs_the_worker_in_a_terminal_and_relays_events() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_terminal(&bin);
    shim(&bin, "pacman", 0);
    let rec = tmp.path().join("rec");
    let esc = escalator(&tmp.path().join("esc"), "recorder", &rec, 0, "");
    let s = FakeServer::start();
    let rel = s.publish_one("v1.0.5", "fairfax-x86_64.pkg.tar.zst", b"PKG");
    s.publish_releases(vec![rel]);
    let marker = tmp.path().join("restarted");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out = praeco()
        .env("PATH", &path)
        .env("PRAECO_TRUSTED_DIRS", &bin)
        .env("PRAECO_ESCALATION_CMD", &esc)
        .args(base_args(&s, "install"))
        .args(["--version", "1.0.5", "--exe", "/usr/bin/praeco-test-nonexistent", "--mode", "terminal"])
        .args(["--restart", "--", "sh", "-c", &format!("echo up > '{}'", marker.display())])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let ev = json_lines(&out);
    let names: Vec<&str> = ev.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(names[0], "terminal_opened");
    assert!(names.contains(&"resolving") && names.contains(&"verifying") && names.contains(&"installing") && names.contains(&"installed"));
    assert!(!names.contains(&"hello"), "internal events are not relayed: {names:?}");
    assert_eq!(*names.last().unwrap(), "done");
    assert_eq!(&recorded(&rec)[1..3], ["-U", "--noconfirm"]);
    assert!(wait_for(&marker), "the parent restarts the app after the terminal worker finishes");
}

#[test]
fn terminal_mode_reports_a_failed_install() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_terminal(&bin);
    shim(&bin, "pacman", 0);
    let rec = tmp.path().join("rec");
    let esc = escalator(&tmp.path().join("esc"), "recorder", &rec, 1, "error: could not lock database");
    let s = FakeServer::start();
    let rel = s.publish_one("v1.0.5", "fairfax-x86_64.pkg.tar.zst", b"PKG");
    s.publish_releases(vec![rel]);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out = praeco()
        .env("PATH", &path)
        .env("PRAECO_TRUSTED_DIRS", &bin)
        .env("PRAECO_ESCALATION_CMD", &esc)
        .args(base_args(&s, "install"))
        .args(["--version", "1.0.5", "--exe", "/usr/bin/praeco-test-nonexistent", "--mode", "terminal"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let last = json_lines(&out).pop().unwrap();
    assert_eq!(last["event"], "error");
    assert_eq!(last["code"], "install_failed");
    assert!(last["message"].as_str().unwrap().contains("exit status 1"));
}

#[test]
fn terminal_mode_without_a_terminal_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    shim(&bin, "pacman", 0);
    let s = FakeServer::start();
    s.publish_releases(vec![]);
    let out = praeco()
        .env("PATH", &bin)
        .env("PRAECO_TRUSTED_DIRS", &bin)
        .args(base_args(&s, "install"))
        .args(["--version", "1.0.5", "--exe", "/usr/bin/praeco-test-nonexistent", "--mode", "terminal"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(json_lines(&out).last().unwrap()["message"].as_str().unwrap().contains("TERMINAL"));
}

// ---- --detach, relocation out of an AppImage mount, paths with spaces ----

fn detach_args<'a>(i: &'a Install, extra: &[&'a str]) -> Vec<String> {
    let mut v: Vec<String> = base_args(&i.server, "install").iter().map(|s| s.to_string()).collect();
    for a in ["--version", "1.0.5", "--exe", "/tmp/.mount_x/usr/bin/fairfax", "--appdir", "/tmp/.mount_x", "--appimage", i.img.to_str().unwrap()] {
        v.push(a.to_string());
    }
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn wait_for_event(path: &Path, event: &str) -> Vec<Value> {
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(&format!("\"event\":\"{event}\"")) {
            return text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("no {event} event in {}: {:?}", path.display(), std::fs::read_to_string(path));
}

#[test]
fn detach_returns_at_once_and_the_update_survives_in_the_progress_file() {
    use std::os::unix::fs::PermissionsExt;
    let i = appimage_install(b"NEW APPIMAGE");
    let progress = i.tmp.path().join("progress.jsonl");
    let marker = i.tmp.path().join("restarted");
    let old = Command::new("sleep").arg("60").spawn().unwrap();
    let mut old = old;
    let pid = old.id().to_string();
    let script = format!("echo \"$0\" > '{}'", marker.display());
    let args = detach_args(&i, &["--progress-file", progress.to_str().unwrap(), "--detach", "--restart-pid", &pid, "--restart", "--", "sh", "-c", &script, "--detach"]);
    let started = Instant::now();
    let out = praeco().args(&args).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(started.elapsed() < Duration::from_secs(5), "the caller is not kept waiting");
    let lines = json_lines(&out);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"], "detached");
    assert_eq!(lines[0]["progress_file"], progress.to_str().unwrap());

    let ev = wait_for_event(&progress, "done");
    assert_eq!(ev[0]["event"], "resolving");
    assert_eq!(ev.last().unwrap()["event"], "done");
    assert_eq!(ev.last().unwrap()["restarted"], true);
    assert_eq!(std::fs::metadata(&progress).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&i.img).unwrap(), "NEW APPIMAGE");
    assert!(wait_for(&marker), "the detached run restarted the app");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "--detach", "arguments after --restart reach the app untouched");
    old.wait().unwrap();
}

#[test]
fn detach_without_a_progress_file_uses_a_private_directory_and_prints_its_path() {
    use std::os::unix::fs::PermissionsExt;
    let i = appimage_install(b"NEW");
    let out = praeco().args(detach_args(&i, &["--detach"])).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let path = PathBuf::from(json_lines(&out)[0]["progress_file"].as_str().unwrap());
    let dir = path.parent().unwrap().to_path_buf();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    wait_for_event(&path, "done");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn detach_refuses_an_existing_file_and_a_shared_directory() {
    use std::os::unix::fs::PermissionsExt;
    let i = appimage_install(b"NEW");
    let existing = i.tmp.path().join("exists.jsonl");
    std::fs::write(&existing, "precious").unwrap();
    let out = praeco().args(detach_args(&i, &["--detach", "--progress-file", existing.to_str().unwrap()])).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "precious", "an existing file (or symlink target) is never touched");

    let link = i.tmp.path().join("link.jsonl");
    std::os::unix::fs::symlink(&existing, &link).unwrap();
    let out = praeco().args(detach_args(&i, &["--detach", "--progress-file", link.to_str().unwrap()])).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "precious");

    let shared = i.tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = praeco().args(detach_args(&i, &["--detach", "--progress-file", shared.join("p.jsonl").to_str().unwrap()])).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!shared.join("p.jsonl").exists());
    assert_eq!(std::fs::read_to_string(&i.img).unwrap(), "OLD", "nothing was installed");
}

#[test]
fn praeco_inside_an_appimage_mount_runs_from_a_private_copy_when_it_will_outlive_the_app() {
    let i = appimage_install(b"NEW");
    let mount = PathBuf::from(format!("/tmp/.mount_praecotest{}", std::process::id()));
    std::fs::create_dir_all(mount.join("usr/bin")).unwrap();
    let inside = mount.join("usr/bin/praeco");
    std::fs::copy(env!("CARGO_BIN_EXE_praeco"), &inside).unwrap();
    let marker = i.tmp.path().join("who");
    let script = format!("readlink /proc/$PPID/exe > '{}'", marker.display());
    let mut args = base_args(&i.server, "install").iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for a in ["--version", "1.0.5", "--exe", "/tmp/.mount_x/usr/bin/fairfax", "--appdir", "/tmp/.mount_x", "--appimage", i.img.to_str().unwrap(), "--restart", "--", "sh", "-c", &script] {
        args.push(a.to_string());
    }
    let mut c = Command::new(&inside);
    c.args(&args).env_remove("APPIMAGE").env_remove("PRAECO_SELF_COPY_DIR");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(json_lines(&out).last().unwrap()["event"], "done");
    assert!(wait_for(&marker));
    let who = std::fs::read_to_string(&marker).unwrap();
    assert!(who.contains("praeco-self-") && !who.contains(".mount_"), "praeco restarted the app from a copy outside the mount: {who}");
    let _ = std::fs::remove_dir_all(&mount);
    // The copy removes itself when it is done.
    let dir = Path::new(who.trim().trim_end_matches(" (deleted)")).parent().unwrap().to_path_buf();
    let end = Instant::now() + Duration::from_secs(5);
    while dir.exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!dir.exists(), "{} was left behind", dir.display());
}

#[test]
fn terminal_mode_works_when_praeco_lives_in_a_path_with_spaces() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_terminal(&bin);
    let spaced = tmp.path().join("dir with  spaces");
    std::fs::create_dir_all(&spaced).unwrap();
    let copy = spaced.join("praeco");
    std::fs::copy(env!("CARGO_BIN_EXE_praeco"), &copy).unwrap();
    let i = appimage_install(b"NEW");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let mut args = detach_args(&i, &[]);
    args.extend(["--mode".to_string(), "terminal".to_string()]);
    let out = Command::new(&copy).args(&args).env("PATH", &path).env_remove("APPIMAGE").stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let names: Vec<String> = json_lines(&out).iter().map(|e| e["event"].as_str().unwrap().to_string()).collect();
    assert_eq!(names.first().map(String::as_str), Some("terminal_opened"));
    assert_eq!(names.last().map(String::as_str), Some("done"));
    assert_eq!(std::fs::read_to_string(&i.img).unwrap(), "NEW");
}
