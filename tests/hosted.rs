mod common;
use common::*;
use praeco::hook::{ensure_app_hook, ensure_omarchy_hook, remove_app_hook, HookOutcome, RemoveOutcome};
use praeco::registry::{App, SourceSpec};
use praeco::updateall::{update_hosted, Options, Status, UpdateMode};
use std::path::{Path, PathBuf};
use std::process::Command;

fn hosted(name: &str, prog: &Path, args: &[&str]) -> App {
    let mut cmd = vec![prog.to_string_lossy().into_owned()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    App {
        name: name.into(),
        source: SourceSpec::Github { repo: "you/myapp".into(), api_base: None },
        exe: prog.to_path_buf(),
        appimage: false,
        version_cmd: vec![],
        update_cmd: Some(cmd),
        needs_terminal: false,
        prereleases: false,
        asset_patterns: vec![],
    }
}

struct Rig {
    tmp: tempfile::TempDir,
    home: PathBuf,
    env: praeco::detect::Env,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = env_with(&tmp.path().join("bin"), &home);
    Rig { tmp, home, env }
}

fn os_release(r: &Rig, id: &str) -> PathBuf {
    let p = r.tmp.path().join("os-release");
    std::fs::write(&p, format!("ID={id}\n")).unwrap();
    p
}

#[test]
fn an_app_that_updates_itself_needs_an_update_command_not_a_version_command() {
    let ok = hosted("babbage", Path::new("/opt/babbage/Babbage.AppImage"), &["--update-now"]);
    assert!(ok.validate().is_ok());
    assert!(ok.hosted());
    let mut a = ok.clone();
    a.update_cmd = Some(vec![]);
    assert!(a.validate().is_err(), "empty update command");
    a.update_cmd = Some(vec!["babbage".into()]);
    assert!(a.validate().is_err(), "relative update command");
    a.update_cmd = Some(vec!["/opt/b".into(), "--x\n; rm -rf ~".into()]);
    assert!(a.validate().is_err(), "a newline cannot be smuggled into the hook");
    let mut plain = ok.clone();
    plain.update_cmd = None;
    assert!(plain.validate().is_err(), "without an update command a version command is required");
    // Round trip: neither optional field is written when unused.
    let json = serde_json::to_value(&ok).unwrap();
    assert!(json.get("version_cmd").is_none() && json.get("needs_terminal").is_none(), "{json}");
    assert_eq!(json["update_cmd"][1], "--update-now");
    assert_eq!(serde_json::from_value::<App>(json).unwrap(), ok);
}

#[test]
fn the_app_hook_is_its_own_file_and_leaves_the_shared_hook_alone() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    let app = hosted("babbage", Path::new("/opt/babbage/Babbage.AppImage"), &["--update-now", "it's"]);
    let hooks = r.home.join(".config/omarchy/hooks/post-update.d");

    let HookOutcome::Installed { path } = ensure_app_hook(&r.env, &app, &osr).unwrap() else { panic!("expected Installed") };
    assert_eq!(path, hooks.join("50-praeco-babbage"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("cmd=('/opt/babbage/Babbage.AppImage' '--update-now' 'it'\\''s')"), "{text}");
    assert!(text.contains("setsid -w") && text.contains("</dev/null"), "no terminal for a command that needs no root: {text}");
    assert!(!hooks.join("50-praeco").exists(), "the shared hook is not touched");
    assert_eq!(ensure_app_hook(&r.env, &app, &osr).unwrap(), HookOutcome::Current { path: path.clone() });

    let mut root = app.clone();
    root.needs_terminal = true;
    assert!(matches!(ensure_app_hook(&r.env, &root, &osr).unwrap(), HookOutcome::Installed { .. }), "rewritten");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("setsid"), "an update that may need sudo keeps the terminal: {text}");

    std::fs::write(&path, "#!/bin/bash\necho mine\n").unwrap();
    assert!(matches!(ensure_app_hook(&r.env, &app, &osr).unwrap(), HookOutcome::Skipped { .. }), "a foreign file is left alone");
    assert_eq!(remove_app_hook(&r.env, "babbage").unwrap().clone(), RemoveOutcome::Kept { reason: format!("{} was not written by praeco, so it was left alone", path.display()) });
    assert!(path.exists());
}

#[test]
fn the_app_hook_is_refused_elsewhere_and_for_programs_that_may_vanish() {
    let r = rig();
    let app = hosted("babbage", Path::new("/opt/babbage/Babbage.AppImage"), &["--update-now"]);
    assert_eq!(ensure_app_hook(&r.env, &app, &os_release(&r, "debian")).unwrap(), HookOutcome::NotApplicable);
    let osr = os_release(&r, "omarchy");
    for temp in ["/tmp/.mount_Babbage1/usr/bin/babbage", "/tmp/x/babbage"] {
        let HookOutcome::Skipped { reason } = ensure_app_hook(&r.env, &hosted("babbage", Path::new(temp), &[]), &osr).unwrap() else { panic!("{temp}") };
        assert!(reason.contains("may not exist"), "{reason}");
    }
    let mut plain = app.clone();
    plain.update_cmd = None;
    assert!(matches!(ensure_app_hook(&r.env, &plain, &osr).unwrap(), HookOutcome::Skipped { .. }));
    assert!(!r.home.join(".config/omarchy").exists(), "nothing written");
    assert_eq!(remove_app_hook(&r.env, "../x").unwrap(), RemoveOutcome::NotPresent, "a name cannot escape the hooks folder");
    // The shared hook still works beside an app hook.
    let shared = ensure_omarchy_hook(&r.env, Path::new("/usr/local/bin/praeco"), &osr).unwrap();
    assert!(matches!(shared, HookOutcome::Installed { .. }));
}

/// An app that records how the hook ran it: its arguments, its session, what stdin was.
fn recording_app(dir: &Path) -> (PathBuf, PathBuf) {
    let out = dir.join("ran.txt");
    let app = dir.join("bin/babbage");
    write_exec(&app, &format!("#!/bin/bash\n{{ echo \"args=$*\"; echo \"sid=$(cut -d' ' -f6 /proc/self/stat)\"; echo \"stdin=$(readlink /proc/self/fd/0)\"; }} > '{}'\nexit 7\n", out.display()));
    (app, out)
}

/// Run the hook with a pipe for stdin, so "stdin was closed to /dev/null" can only come from the hook itself
/// (the harness may well have /dev/null as its own stdin).
fn run_hook(path: &Path) -> std::process::Output {
    let child = Command::new("bash")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_hook_script_runs_the_command_detached_and_never_fails_the_update() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    // Not under /tmp: the hook refuses programs there.
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (prog, out) = recording_app(work.path());
    let mut app = hosted("babbage", &prog, &["--update-now", "two words"]);
    let HookOutcome::Installed { path } = ensure_app_hook(&r.env, &app, &osr).unwrap() else { panic!() };

    let result = run_hook(&path);
    assert_eq!(result.status.code(), Some(0), "the app exiting 7 must not fail omarchy update");
    let ran = std::fs::read_to_string(&out).unwrap();
    assert!(ran.contains("args=--update-now two words"), "{ran}");
    let ours = nix::unistd::getsid(None).unwrap().as_raw();
    let theirs: i32 = ran.lines().find_map(|l| l.strip_prefix("sid=")).unwrap().parse().unwrap();
    assert_ne!(theirs, ours, "its own session, so no terminal and no borrowed sudo: {ran}");
    assert!(ran.contains("stdin=/dev/null"), "{ran}");

    app.needs_terminal = true;
    ensure_app_hook(&r.env, &app, &osr).unwrap();
    std::fs::remove_file(&out).unwrap();
    assert_eq!(run_hook(&path).status.code(), Some(0));
    let ran = std::fs::read_to_string(&out).unwrap();
    let theirs: i32 = ran.lines().find_map(|l| l.strip_prefix("sid=")).unwrap().parse().unwrap();
    assert_eq!(theirs, ours, "an update that may need sudo stays in the update's session: {ran}");
    assert!(!ran.contains("stdin=/dev/null"), "{ran}");

    std::fs::remove_file(&prog).unwrap();
    std::fs::remove_file(&out).unwrap();
    let result = run_hook(&path);
    assert_eq!(result.status.code(), Some(0), "an uninstalled app is skipped quietly");
    assert!(result.stdout.is_empty() && result.stderr.is_empty() && !out.exists());
}

#[test]
fn update_hosted_updates_the_app_with_the_version_it_reports() {
    let r = rig();
    let s = FakeServer::start();
    let img = r.tmp.path().join("Babbage.AppImage");
    write_exec(&img, "OLD");
    // The machine's architecture is pinned on both sides, so the test means the same everywhere.
    s.publish_releases(vec![s.publish_one("v1.0.5", "myapp-aarch64.AppImage", b"NEW")]);
    let mut app = hosted("babbage", &img, &["--update-now"]);
    app.appimage = true;
    app.source = SourceSpec::Forgejo { base: s.base.clone(), repo: "o/r".into() };
    let mut env = r.env.clone();
    env.arch = "aarch64".into();
    let opts = Options { mode: UpdateMode::Inline, stdin_is_tty: false, env, token: None, escalation: None };

    let mut log = Vec::new();
    let res = update_hosted(&app, "1.0.4", &opts, &mut |l| log.push(l));
    assert_eq!(res.status, Status::Updated, "{log:?} {res:?}");
    assert_eq!((res.from.as_deref(), res.to.as_deref()), (Some("1.0.4"), Some("1.0.5")));
    assert_eq!(std::fs::read(&img).unwrap(), b"NEW");

    let res = update_hosted(&app, "1.0.5", &opts, &mut |_| {});
    assert_eq!(res.status, Status::UpToDate);
    assert_eq!(std::fs::read(&img).unwrap(), b"NEW", "nothing was reinstalled");

    app.source = SourceSpec::Forgejo { base: "http://127.0.0.1:1".into(), repo: "o/r".into() };
    let res = update_hosted(&app, "1.0.4", &opts, &mut |_| {});
    assert_eq!(res.status, Status::Failed);
    assert_eq!(res.error.unwrap().code, "network");
}

#[test]
fn a_source_converts_to_a_registration_and_back() {
    use praeco::source::Source;
    for source in [
        Source::github("you/myapp").unwrap(),
        Source::github_with_api_base("you/myapp", "https://ghe.example.com/api/v3").unwrap(),
        Source::forgejo("https://code.example.com", "owner/repo").unwrap(),
    ] {
        assert_eq!(SourceSpec::from(&source).to_source().unwrap(), source);
    }
    assert!(matches!(SourceSpec::from(&Source::github("you/myapp").unwrap()), SourceSpec::Github { api_base: None, .. }), "the default API base is not written out");
}
