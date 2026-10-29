mod common;
use common::*;
use praeco::hook::{ensure_omarchy_hook, is_omarchy, remove_omarchy_hook, HookOutcome, RemoveOutcome};
use praeco::registry::{self, App, SourceSpec};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn app(name: &str, exe: &Path) -> App {
    App {
        name: name.into(),
        source: SourceSpec::Github { repo: "you/myapp".into(), api_base: None },
        exe: exe.to_path_buf(),
        appimage: false,
        version_cmd: vec![exe.to_string_lossy().into_owned(), "--version".into()],
        update_cmd: None,
        needs_terminal: false,
        prereleases: false,
        asset_patterns: vec![],
    }
}

struct Rig {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    env: praeco::detect::Env,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = env_with(&tmp.path().join("bin"), &home);
    Rig { _tmp: tmp, home, env }
}

#[test]
fn validation_rejects_unusable_registrations() {
    let ok = app("my-app_1.x", Path::new("/usr/bin/myapp"));
    assert!(ok.validate().is_ok());
    for name in ["", ".hidden", "-x", "a/b", "a b", &"x".repeat(65)] {
        assert!(app(name, Path::new("/usr/bin/myapp")).validate().is_err(), "name {name:?}");
    }
    assert!(app("a", Path::new("relative/myapp")).validate().is_err(), "relative exe");
    let mut a = ok.clone();
    a.version_cmd = vec![];
    assert!(a.validate().is_err(), "empty version command");
    a.version_cmd = vec!["myapp".into(), "--version".into()];
    assert!(a.validate().is_err(), "relative version command");
    let mut a = ok.clone();
    a.asset_patterns = vec!["nonsense".into()];
    assert!(a.validate().is_err(), "bad asset pattern");
    a.asset_patterns = vec!["deb=myapp_*_{arch}.deb".into()];
    assert!(a.validate().is_ok());
    let mut a = ok;
    a.source = SourceSpec::Forgejo { base: "http://example.com".into(), repo: "o/r".into() };
    assert!(a.validate().is_err(), "plain http is only for loopback");
}

#[test]
fn save_then_load_round_trips_and_user_replaces_system() {
    let r = rig();
    let a = app("myapp", Path::new("/usr/bin/myapp"));
    let file = registry::save(&r.env, &a).unwrap();
    assert_eq!(file, r.home.join(".config/praeco/apps.d/myapp.json"));
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
    let loaded = registry::load(&r.env);
    assert_eq!(loaded.apps, vec![a.clone()]);
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);

    let mut changed = a.clone();
    changed.prereleases = true;
    registry::save(&r.env, &changed).unwrap();
    assert_eq!(registry::load(&r.env).apps, vec![changed], "saving again replaces the file");
}

#[test]
fn bad_files_are_reported_and_skipped_not_fatal() {
    let r = rig();
    let dir = r.home.join(".config/praeco/apps.d");
    registry::save(&r.env, &app("good", Path::new("/usr/bin/good"))).unwrap();
    std::fs::write(dir.join("broken.json"), "{ nope").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored silently").unwrap();
    let mut wrong = app("other", Path::new("/usr/bin/other"));
    wrong.name = "renamed".into();
    std::fs::write(dir.join("other.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
    let writable = dir.join("loose.json");
    std::fs::write(&writable, serde_json::to_vec(&app("loose", Path::new("/usr/bin/loose"))).unwrap()).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o666)).unwrap();

    let loaded = registry::load(&r.env);
    assert_eq!(loaded.apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(loaded.problems.len(), 3, "{:?}", loaded.problems);
    assert!(loaded.problems.iter().any(|p| p.contains("broken.json")));
    assert!(loaded.problems.iter().any(|p| p.contains("does not match the file name")));
    assert!(loaded.problems.iter().any(|p| p.contains("writable by others")));
}

#[test]
fn version_output_parsing() {
    use registry::parse_version_output as p;
    assert_eq!(p("myapp 1.2.3\n").as_deref(), Some("1.2.3"));
    assert_eq!(p("MyApp v2.0 (Qt 6)").as_deref(), Some("2.0"));
    assert_eq!(p("version: 3.1.4-rc1,").as_deref(), Some("3.1.4-rc1"));
    assert_eq!(p("Qt 6 only"), None, "a bare integer is not a version");
    assert_eq!(p(""), None);
}

#[test]
fn installed_version_runs_the_command_detached_from_the_terminal() {
    let r = rig();
    let bin = r.home.join("myapp");
    let sid_file = r.home.join("sid");
    write_exec(&bin, &format!("#!/bin/sh\ncut -d' ' -f6 /proc/self/stat > '{}'\necho \"myapp 1.4.2\"\n", sid_file.display()));
    let a = app("myapp", &bin);
    assert_eq!(registry::installed_version(&a).unwrap(), "1.4.2");
    let theirs: i32 = std::fs::read_to_string(&sid_file).unwrap().trim().parse().unwrap();
    let ours = nix::unistd::getsid(None).unwrap().as_raw();
    assert_ne!(theirs, ours, "the version command has its own session, so it has no terminal to borrow sudo through");
}

#[test]
fn installed_version_failures_are_errors_with_the_app_name() {
    let r = rig();
    let bin = r.home.join("myapp");
    write_exec(&bin, "#!/bin/sh\necho no numbers here\n");
    let e = registry::installed_version(&app("myapp", &bin)).unwrap_err();
    assert!(e.message.contains("myapp") && e.message.contains("no version number"), "{}", e.message);
    write_exec(&bin, "#!/bin/sh\necho 1.0.0\nexit 3\n");
    assert!(registry::installed_version(&app("myapp", &bin)).unwrap_err().message.contains("exited"));
    let missing = app("myapp", &r.home.join("gone"));
    assert!(registry::installed_version(&missing).unwrap_err().message.contains("could not run"));
    write_exec(&bin, "#!/bin/sh\nexec sleep 5\n");
    let started = std::time::Instant::now();
    let e = registry::installed_version_within(&app("myapp", &bin), Duration::from_millis(300)).unwrap_err();
    assert!(e.message.contains("did not answer"), "{}", e.message);
    assert!(started.elapsed() < Duration::from_secs(3));
}

fn os_release(r: &Rig, id: &str) -> PathBuf {
    let p = r.home.join("os-release");
    std::fs::write(&p, format!("NAME=\"X\"\nID={id}\nID_LIKE=arch\n")).unwrap();
    p
}

#[test]
fn omarchy_detection() {
    assert!(is_omarchy("NAME=Omarchy\nID=omarchy\nID_LIKE=arch\n"));
    assert!(is_omarchy("ID=\"omarchy\"\n"));
    assert!(!is_omarchy("ID=arch\n"));
    assert!(!is_omarchy("ID_LIKE=omarchy\n"), "only the system's own ID counts");
    assert!(!is_omarchy("VARIANT_ID=omarchy\n"));
}

#[test]
fn hook_is_installed_once_rewritten_for_a_new_path_and_never_clobbers_a_foreign_file() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    let hook = r.home.join(".config/omarchy/hooks/post-update.d/50-praeco");
    let praeco = Path::new("/usr/local/bin/praeco");

    let HookOutcome::Installed { path } = ensure_omarchy_hook(&r.env, praeco, &osr).unwrap() else { panic!("expected Installed") };
    assert_eq!(path, hook);
    assert_eq!(std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777, 0o755);
    let text = std::fs::read_to_string(&hook).unwrap();
    assert!(text.starts_with("#!/bin/bash\n# praeco-managed"), "{text}");
    assert!(text.contains("'/usr/local/bin/praeco' update-all || true"), "{text}");

    assert_eq!(ensure_omarchy_hook(&r.env, praeco, &osr).unwrap(), HookOutcome::Current { path: hook.clone() });
    let moved = Path::new("/opt/it's here/praeco");
    assert!(matches!(ensure_omarchy_hook(&r.env, moved, &osr).unwrap(), HookOutcome::Installed { .. }));
    assert!(std::fs::read_to_string(&hook).unwrap().contains("'/opt/it'\\''s here/praeco' update-all"), "quotes are escaped");

    std::fs::write(&hook, "#!/bin/bash\necho mine\n").unwrap();
    let HookOutcome::Skipped { reason } = ensure_omarchy_hook(&r.env, praeco, &osr).unwrap() else { panic!("expected Skipped") };
    assert!(reason.contains("left alone"), "{reason}");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "#!/bin/bash\necho mine\n");
}

#[test]
fn hook_is_not_written_elsewhere_or_for_a_vanishing_praeco() {
    let r = rig();
    let hook_dir = r.home.join(".config/omarchy");
    assert_eq!(ensure_omarchy_hook(&r.env, Path::new("/usr/bin/praeco"), &os_release(&r, "debian")).unwrap(), HookOutcome::NotApplicable);
    assert_eq!(ensure_omarchy_hook(&r.env, Path::new("/usr/bin/praeco"), &r.home.join("no-such-file")).unwrap(), HookOutcome::NotApplicable);
    let osr = os_release(&r, "omarchy");
    for temp in ["/tmp/praeco-self-x/praeco", "/tmp/.mount_MyApp1/usr/bin/praeco"] {
        let HookOutcome::Skipped { reason } = ensure_omarchy_hook(&r.env, Path::new(temp), &osr).unwrap() else { panic!("{temp}") };
        assert!(reason.contains("may not exist"), "{reason}");
    }
    assert!(!hook_dir.exists(), "nothing was written");
}

#[test]
fn remove_deletes_only_the_users_registration() {
    let r = rig();
    registry::save(&r.env, &app("myapp", Path::new("/usr/bin/myapp"))).unwrap();
    registry::save(&r.env, &app("keep", Path::new("/usr/bin/keep"))).unwrap();
    let file = r.home.join(".config/praeco/apps.d/myapp.json");
    assert_eq!(registry::remove(&r.env, "myapp").unwrap(), registry::Removal::Removed(file.clone()));
    assert!(!file.exists());
    assert_eq!(registry::load(&r.env).apps.len(), 1);
    assert_eq!(registry::remove(&r.env, "myapp").unwrap(), registry::Removal::NotRegistered, "removing twice is fine");
    for bad in ["../keep", "a/b", "", ".hidden"] {
        assert!(registry::remove(&r.env, bad).is_err(), "{bad:?}");
    }
    assert_eq!(registry::load(&r.env).apps.len(), 1, "nothing else was touched");
}

/// A stand-in for Omarchy's installer. It writes only where `dest` says: a test must never
/// reach the real home folder.
fn hook_tool(r: &Rig, dest: &Path, exit: i32) -> PathBuf {
    let bin = r.env.trusted_dirs[0].clone();
    let log = bin.join("hook-install.log");
    write_exec(
        &bin.join("omarchy-hook-install"),
        &format!("#!/bin/sh\necho \"$1\" >> '{log}'\nmkdir -p '{d}'\ncp \"$2\" '{d}/'\nchmod 755 '{d}/'\"$(basename \"$2\")\"\necho \"Installed $1 hook: {d}/$(basename \"$2\")\"\nexit {exit}\n", log = log.display(), d = dest.display()),
    );
    log
}

#[test]
fn hook_uses_omarchy_hook_install_when_present_and_follows_the_path_it_reports() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    let moved = r.home.join("moved-hooks/post-update.d");
    let log = hook_tool(&r, &moved, 0);
    let praeco = Path::new("/usr/local/bin/praeco");
    let HookOutcome::Installed { path } = ensure_omarchy_hook(&r.env, praeco, &osr).unwrap() else { panic!("expected Installed") };
    assert_eq!(path, moved.join("50-praeco"), "the path Omarchy's tool reported");
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "post-update");
    assert!(std::fs::read_to_string(&path).unwrap().contains("praeco-managed"));
    assert!(!r.home.join(".config/omarchy").exists(), "no second copy at the default place");
}

#[test]
fn hook_falls_back_to_writing_it_when_the_omarchy_tool_fails_or_wrote_something_else() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    let praeco = Path::new("/usr/local/bin/praeco");
    let default = r.home.join(".config/omarchy/hooks/post-update.d/50-praeco");

    hook_tool(&r, &r.home.join("elsewhere"), 1);
    assert_eq!(ensure_omarchy_hook(&r.env, praeco, &osr).unwrap(), HookOutcome::Installed { path: default.clone() }, "tool failed");
    std::fs::remove_file(&default).unwrap();

    write_exec(&r.env.trusted_dirs[0].join("omarchy-hook-install"), "#!/bin/sh\necho \"Installed post-update hook: /nonexistent/50-praeco\"\n");
    assert_eq!(ensure_omarchy_hook(&r.env, praeco, &osr).unwrap(), HookOutcome::Installed { path: default.clone() }, "tool reported a file that is not there");
    assert!(std::fs::read_to_string(&default).unwrap().contains("'/usr/local/bin/praeco' update-all"));
}

#[test]
fn removing_the_hook_takes_only_what_praeco_wrote() {
    let r = rig();
    let osr = os_release(&r, "omarchy");
    let hook = r.home.join(".config/omarchy/hooks/post-update.d/50-praeco");
    assert_eq!(remove_omarchy_hook(&r.env).unwrap(), RemoveOutcome::NotPresent);
    ensure_omarchy_hook(&r.env, Path::new("/usr/local/bin/praeco"), &osr).unwrap();
    assert_eq!(remove_omarchy_hook(&r.env).unwrap(), RemoveOutcome::Removed { path: hook.clone() });
    assert!(!hook.exists());
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/bash\necho mine\n").unwrap();
    assert!(matches!(remove_omarchy_hook(&r.env).unwrap(), RemoveOutcome::Kept { .. }));
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "#!/bin/bash\necho mine\n", "a foreign file is never deleted");
}
