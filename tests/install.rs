mod common;
use common::*;
use praeco::install::{install, package_command, resolve_escalation, InstallOptions, Mode};
use praeco::progress::Event;
use praeco::{ErrorCode, InstallKind};
use serde_json::json;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Rig {
    tmp: tempfile::TempDir,
    server: FakeServer,
    bin: PathBuf,
    home: PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Rig { tmp, server: FakeServer::start(), bin, home }
}

impl Rig {
    fn opts(&self, version: &str, exe: &Path) -> InstallOptions {
        let mut o = InstallOptions::new(self.server.source(), version, exe);
        o.env = env_with(&self.bin, &self.home);
        o
    }

    /// An AppImage install: returns options and the path of the old file.
    fn appimage(&self, version: &str) -> (InstallOptions, PathBuf) {
        let img = self.home.join("Fairfax.AppImage");
        write_exec(&img, "OLD IMAGE");
        let mut o = self.opts(version, &img);
        o.env.appimage = Some(img.clone());
        o.env.appdir = Some(self.home.clone());
        (o, img)
    }

    fn requested(&self, path: &str) -> usize {
        self.server.requests_for(path).len()
    }
}

fn events_of(o: &InstallOptions) -> (Vec<Event>, praeco::Result<praeco::Installed>) {
    let mut ev = Vec::new();
    let r = install(o, &mut |e| ev.push(e.clone()));
    (ev, r)
}

const IMG: &str = "fairfax-x86_64.AppImage";

#[test]
fn appimage_is_replaced_atomically_in_place_with_progress() {
    let r = rig();
    let content = vec![7u8; 300_000];
    let rel = r.server.publish_one("v1.0.5", IMG, &content);
    r.server.publish_releases(vec![rel]);
    let (o, img) = r.appimage("1.0.5");
    std::fs::set_permissions(&img, std::fs::Permissions::from_mode(0o750)).unwrap();

    let (ev, res) = events_of(&o);
    let done = res.unwrap();
    assert_eq!((done.version.as_str(), done.kind), ("1.0.5", InstallKind::AppImage));
    assert_eq!(std::fs::read(&img).unwrap(), content);
    assert_eq!(std::fs::metadata(&img).unwrap().permissions().mode() & 0o777, 0o750);
    let names: Vec<_> = std::fs::read_dir(&r.home).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["Fairfax.AppImage"], "no temp files left behind");

    let kinds: Vec<&str> = ev
        .iter()
        .map(|e| match e {
            Event::Resolving { .. } => "resolving",
            Event::Downloading { .. } => "downloading",
            Event::Verifying { .. } => "verifying",
            Event::Installing { .. } => "installing",
            Event::Installed { .. } => "installed",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds.first(), Some(&"resolving"));
    assert_eq!(&kinds[kinds.len() - 3..], ["verifying", "installing", "installed"]);
    assert!(kinds.contains(&"downloading"));
    let last_dl = ev.iter().rev().find_map(|e| if let Event::Downloading { bytes, total, .. } = e { Some((*bytes, *total)) } else { None }).unwrap();
    assert_eq!(last_dl, (300_000, Some(300_000)));
}

#[test]
fn sha256sums_alone_is_enough() {
    let r = rig();
    r.server.serve_asset("v1.0.5", IMG, b"NEW");
    r.server.serve_asset("v1.0.5", "SHA256SUMS", format!("{}  other\n{}  {IMG}\n", "0".repeat(64), sha256_hex(b"NEW")).as_bytes());
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG, "SHA256SUMS"])]);
    let (o, img) = r.appimage("v1.0.5");
    install(&o, &mut |_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&img).unwrap(), "NEW");
}

#[test]
fn bad_missing_or_conflicting_checksums_fail_closed() {
    let r = rig();
    let (o, img) = r.appimage("1.0.5");

    // Wrong sidecar.
    r.server.serve_asset("v1.0.5", IMG, b"NEW");
    r.server.serve_asset("v1.0.5", &format!("{IMG}.sha256"), format!("{}  {IMG}\n", sha256_hex(b"something else")).as_bytes());
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG, &format!("{IMG}.sha256")])]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::ChecksumMismatch);

    // No checksum files at all.
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG])]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::ChecksumMissing);

    // Checksum files exist but have no entry for the asset.
    r.server.serve_asset("v1.0.5", "SHA256SUMS", format!("{}  unrelated\n", sha256_hex(b"NEW")).as_bytes());
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG, "SHA256SUMS"])]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::ChecksumMissing);

    // Sidecar right, SHA256SUMS wrong: disagreement is a failure.
    r.server.serve_asset("v1.0.5", &format!("{IMG}.sha256"), sha256_hex(b"NEW").as_bytes());
    r.server.serve_asset("v1.0.5", "SHA256SUMS", format!("{}  {IMG}\n", sha256_hex(b"tampered")).as_bytes());
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG, &format!("{IMG}.sha256"), "SHA256SUMS"])]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::ChecksumMismatch);

    // The listed checksum file cannot be fetched.
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &[IMG, "SHA256SUMS", "x.sha256"])]);
    r.server.route(&FakeServer::asset_path("v1.0.5", "SHA256SUMS"), 404, &[], "");
    assert!(install(&o, &mut |_| {}).is_err());

    assert_eq!(std::fs::read_to_string(&img).unwrap(), "OLD IMAGE", "nothing unverified was ever installed");
}

#[test]
fn only_the_repos_own_release_assets_are_downloaded() {
    let r = rig();
    // The listing points at another repository's path on the same host, and at another host.
    let evil_path = "/evil/other/releases/download/v1.0.5/fairfax-x86_64.AppImage";
    r.server.route(evil_path, 200, &[], "EVIL");
    let mut rel = r.server.release("v1.0.5", "", &[IMG]);
    rel["assets"][0]["browser_download_url"] = json!(format!("{}{evil_path}", r.server.base));
    r.server.publish_releases(vec![rel.clone()]);
    let (o, img) = r.appimage("1.0.5");
    let e = install(&o, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::Download);
    assert_eq!(r.requested(evil_path), 0, "the foreign URL was never requested");

    rel["assets"][0]["browser_download_url"] = json!("https://evil.example.com/o/r/releases/download/v1.0.5/x.AppImage");
    r.server.publish_releases(vec![rel.clone()]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::Download);

    rel["assets"][0]["browser_download_url"] = json!("http://example.com/o/r/releases/download/v1.0.5/x.AppImage");
    r.server.publish_releases(vec![rel]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::Download);
    assert_eq!(std::fs::read_to_string(&img).unwrap(), "OLD IMAGE");
}

#[test]
fn a_corrupted_download_is_rejected() {
    let r = rig();
    let rel = r.server.publish_one("v1.0.5", IMG, b"0123456789");
    r.server.publish_releases(vec![rel]);
    // The served bytes differ from what the checksum was made for.
    r.server.serve_asset("v1.0.5", IMG, b"01234");
    let (o, img) = r.appimage("1.0.5");
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::ChecksumMismatch);
    assert_eq!(std::fs::read_to_string(&img).unwrap(), "OLD IMAGE");
}

#[test]
fn resolution_errors() {
    let r = rig();
    let mut pre = r.server.release("v1.1.0-rc1", "", &["x-x86_64.deb"]);
    pre["prerelease"] = json!(true);
    r.server.publish_releases(vec![r.server.release("v1.0.5", "", &["fairfax-amd64.deb"]), pre]);
    let (o, _) = r.appimage("1.0.9");
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::NoRelease);
    let (o, _) = r.appimage("1.0.5");
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::NoAsset, "release has a deb but no AppImage");
    let (o, _) = r.appimage("garbage");
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::InvalidVersion);
    let (mut o, _) = r.appimage("1.1.0-rc1");
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::NoRelease, "prerelease needs the flag");
    o.include_prereleases = true;
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::NoAsset, "flag lets it through to asset selection");
}

#[test]
fn source_builds_are_unsupported_and_nothing_is_fetched() {
    let r = rig();
    r.server.publish_releases(vec![]);
    let o = r.opts("1.0.5", Path::new("/opt/praeco-test-nonexistent/fairfax"));
    let (ev, res) = events_of(&o);
    let e = res.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unsupported);
    assert!(e.message.contains("source build"));
    assert_eq!(ev.len(), 1);
    assert_eq!(r.requested(FakeServer::releases_path()), 0);
}

#[test]
fn tarball_install_end_to_end() {
    let r = rig();
    let root = r.home.join(".local/opt/fairfax");
    write_exec(&root.join("bin/fairfax"), "old");
    std::fs::write(root.join("settings.keep"), "mine").unwrap();
    let name = "fairfax-linux-x86_64.tar.gz";
    let archive = tgz(&[("fairfax-linux-x86_64/bin/fairfax", b"new binary", 0o755), ("fairfax-linux-x86_64/share/a.txt", b"data", 0o644)]);
    let rel = r.server.publish_one("v1.0.5", name, &archive);
    r.server.publish_releases(vec![rel]);
    let o = r.opts("1.0.5", &root.join("bin/fairfax"));
    let done = install(&o, &mut |_| {}).unwrap();
    assert_eq!(done.kind, InstallKind::Tarball);
    assert_eq!(std::fs::read_to_string(root.join("bin/fairfax")).unwrap(), "new binary");
    assert_eq!(std::fs::metadata(root.join("bin/fairfax")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::read_to_string(root.join("share/a.txt")).unwrap(), "data");
    assert_eq!(std::fs::read_to_string(root.join("settings.keep")).unwrap(), "mine");
}

// ---- privileged path: prove the exact commands without running them ----

struct PkgRig {
    r: Rig,
    rec: PathBuf,
    exe: PathBuf,
    esc: PathBuf,
}

/// A rig where `exe` is owned by a package of `kind` (via a shim), the release
/// has the matching asset, and escalation is a recorder exiting with `code`.
fn pkg_rig(kind: InstallKind, asset: &str, shims: &[&str], code: i32, stderr: &str) -> PkgRig {
    let r = rig();
    for s in shims {
        shim(&r.bin, s, 0);
    }
    let rec = r.tmp.path().join("rec");
    let exe = PathBuf::from("/usr/bin/praeco-test-nonexistent-app");
    let rel = r.server.publish_one("v1.0.5", asset, format!("PACKAGE for {kind}").as_bytes());
    r.server.publish_releases(vec![rel]);
    let esc = escalator(&r.tmp.path().join("esc"), "recorder", &rec, code, stderr);
    PkgRig { r, rec, exe, esc }
}

impl PkgRig {
    fn opts(&self) -> InstallOptions {
        let mut o = self.r.opts("1.0.5", &self.exe);
        o.escalation = Some(vec![self.esc.clone().into_os_string()]);
        o
    }

    fn argv(&self) -> Vec<String> {
        recorded(&self.rec)
    }
}

fn assert_installs_file(p: &PkgRig, kind: InstallKind) {
    let argv = p.argv();
    let file = argv.last().unwrap();
    assert!(file.starts_with("/") && file.contains("/praeco-"), "{file}");
    let copy = std::fs::read_to_string(format!("{}.file", p.rec.display())).unwrap();
    assert_eq!(copy, format!("PACKAGE for {kind}"), "the verified bytes were what the package manager got");
}

#[test]
fn pacman_command() {
    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 0, "");
    let done = install(&p.opts(), &mut |_| {}).unwrap();
    assert_eq!(done.kind, InstallKind::Pacman);
    let argv = p.argv();
    assert_eq!(argv[0], p.r.bin.join("pacman").to_str().unwrap());
    assert_eq!(&argv[1..3], ["-U", "--noconfirm"]);
    assert!(argv[3].ends_with("/fairfax-x86_64.pkg.tar.zst"));
    assert_eq!(argv.len(), 4);
    assert_installs_file(&p, InstallKind::Pacman);
    // praeco itself only ever ran the ownership query; the install went to the escalator.
    let log = std::fs::read_to_string(p.r.bin.join("pacman.log")).unwrap();
    assert!(log.lines().all(|l| l.starts_with("-Qo")), "{log}");
}

#[test]
fn deb_command_prefers_apt_get_and_falls_back_to_dpkg() {
    let p = pkg_rig(InstallKind::Deb, "fairfax-amd64.deb", &["dpkg", "apt-get"], 0, "");
    install(&p.opts(), &mut |_| {}).unwrap();
    let argv = p.argv();
    assert_eq!(argv[0], p.r.bin.join("apt-get").to_str().unwrap());
    assert_eq!(&argv[1..3], ["install", "-y"]);
    assert!(argv[3].ends_with("/fairfax-amd64.deb"));
    assert_installs_file(&p, InstallKind::Deb);

    let p = pkg_rig(InstallKind::Deb, "fairfax-amd64.deb", &["dpkg"], 0, "");
    install(&p.opts(), &mut |_| {}).unwrap();
    let argv = p.argv();
    assert_eq!(argv[0], p.r.bin.join("dpkg").to_str().unwrap());
    assert_eq!(argv[1], "-i");
    assert_eq!(argv.len(), 3);
}

#[test]
fn rpm_command_prefers_dnf_and_falls_back_to_rpm() {
    let p = pkg_rig(InstallKind::Rpm, "fairfax-x86_64.rpm", &["rpm", "dnf"], 0, "");
    install(&p.opts(), &mut |_| {}).unwrap();
    let argv = p.argv();
    assert_eq!(argv[0], p.r.bin.join("dnf").to_str().unwrap());
    assert_eq!(&argv[1..3], ["install", "-y"]);
    assert_installs_file(&p, InstallKind::Rpm);

    let p = pkg_rig(InstallKind::Rpm, "fairfax-x86_64.rpm", &["rpm"], 0, "");
    install(&p.opts(), &mut |_| {}).unwrap();
    let argv = p.argv();
    assert_eq!(argv[0], p.r.bin.join("rpm").to_str().unwrap());
    assert_eq!(argv[1], "-U");
    assert_eq!(argv.len(), 3);
}

#[test]
fn package_command_is_pure() {
    let r = rig();
    shim(&r.bin, "pacman", 0);
    let env = env_with(&r.bin, &r.home);
    let c = package_command(InstallKind::Pacman, Path::new("/tmp/x.pkg.tar.zst"), &env).unwrap();
    assert_eq!(c.len(), 4);
    assert!(package_command(InstallKind::Deb, Path::new("/tmp/x.deb"), &env).is_err(), "no apt-get or dpkg on this PATH");
    assert!(package_command(InstallKind::AppImage, Path::new("/tmp/x"), &env).is_err());
}

#[test]
fn escalation_failures_map_to_clear_errors() {
    // pkexec-style exit codes are only interpreted when the escalator is pkexec.
    for (code, want) in [(126, ErrorCode::AuthCancelled), (127, ErrorCode::AuthFailed)] {
        let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], code, "");
        let pk = escalator(&p.r.tmp.path().join("pk"), "pkexec", &p.rec, code, "");
        let mut o = p.opts();
        o.escalation = Some(vec![pk.into_os_string()]);
        let (ev, res) = events_of(&o);
        assert_eq!(res.unwrap_err().code, want, "exit {code}");
        assert!(!ev.iter().any(|e| matches!(e, Event::Installed { .. })));
    }
    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 126, "");
    let e = install(&p.opts(), &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::InstallFailed, "126 from a non-pkexec escalator is a plain failure");

    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 1, "error: failed to commit transaction (conflicting files)");
    let e = install(&p.opts(), &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::InstallFailed);
    assert!(e.message.contains("conflicting files"), "{}", e.message);
}

#[test]
fn escalation_resolution_and_needs_terminal_detection() {
    let r = rig();
    let proc = r.bin.join("proc");
    std::fs::create_dir_all(proc.join("500")).unwrap();
    std::fs::write(proc.join("500/comm"), "polkit-gnome-au\n").unwrap();
    let pkexec = shim(&r.bin, "pkexec", 0);
    let sudo = shim(&r.bin, "sudo", 0);
    let exe = Path::new("/usr/bin/x");

    let mut o = r.opts("1.0.5", exe);
    o.env.proc_root = proc.clone();
    assert_eq!(resolve_escalation(&o).unwrap(), vec![pkexec.clone().into_os_string()], "dialog: pkexec when an agent runs");

    let mut no_agent = o.clone();
    no_agent.env.proc_root = r.bin.join("empty-proc");
    assert_eq!(resolve_escalation(&no_agent).unwrap_err().code, ErrorCode::NeedsTerminal);

    let mut headless = o.clone();
    headless.env.graphical = false;
    assert_eq!(resolve_escalation(&headless).unwrap_err().code, ErrorCode::NeedsTerminal);

    let mut no_pkexec = o.clone();
    std::fs::remove_file(&pkexec).unwrap();
    assert_eq!(resolve_escalation(&no_pkexec).unwrap_err().code, ErrorCode::NeedsTerminal);

    no_pkexec.mode = Mode::Terminal;
    assert_eq!(resolve_escalation(&no_pkexec).unwrap(), vec![sudo.clone().into_os_string()], "terminal: sudo");
    std::fs::remove_file(&sudo).unwrap();
    assert_eq!(resolve_escalation(&no_pkexec).unwrap_err().code, ErrorCode::InstallFailed);

    let mut root = o.clone();
    root.env.euid = 0;
    assert!(resolve_escalation(&root).unwrap().is_empty(), "root needs no escalation");

    let mut custom = no_agent.clone();
    custom.escalation = Some(vec!["doas".into()]);
    assert_eq!(resolve_escalation(&custom).unwrap(), vec![OsString::from("doas")], "override beats every check");
}

#[test]
fn dialog_mode_without_an_agent_stops_before_downloading() {
    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 0, "");
    let mut o = p.r.opts("1.0.5", &p.exe);
    o.escalation = None;
    // euid 1000, no pkexec on PATH.
    let e = install(&o, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::NeedsTerminal);
    assert_eq!(p.r.requested(&FakeServer::asset_path("v1.0.5", "fairfax-x86_64.pkg.tar.zst")), 0);
    assert!(p.argv().is_empty());
}

// ---- privileged programs come only from trusted directories ----

#[test]
fn privileged_programs_never_come_from_the_users_path() {
    let r = rig();
    let user_bin = r.tmp.path().join("user-bin");
    let trusted = r.tmp.path().join("trusted");
    shim(&user_bin, "pacman", 0);
    shim(&user_bin, "pkexec", 0);
    shim(&user_bin, "sudo", 0);
    let file = Path::new("/tmp/x.pkg.tar.zst");

    // A user-writable directory on PATH with a planted pacman: not used.
    let mut env = env_with(&trusted, &r.home);
    std::fs::create_dir_all(&trusted).unwrap();
    env.path = OsString::from(&user_bin);
    assert!(package_command(InstallKind::Pacman, file, &env).is_err());
    assert!(env.which_trusted("pkexec").is_none() && env.which_trusted("sudo").is_none());

    // The trusted one wins, as an absolute path.
    shim(&trusted, "pacman", 0);
    let c = package_command(InstallKind::Pacman, file, &env).unwrap();
    assert_eq!(c[0], trusted.join("pacman").into_os_string());

    // Empty and relative PATH entries never yield a relative program.
    env.path = OsString::from(format!("/nonexistent-dir:{}:", "relative-bin"));
    assert!(env.which("pacman").is_none());
    env.trusted_dirs = vec![PathBuf::from("relative-bin"), PathBuf::new()];
    assert!(package_command(InstallKind::Pacman, file, &env).is_err());
    assert!(env.which_trusted("pacman").is_none());
}

#[test]
fn trusted_programs_must_not_be_writable_by_others_or_owned_by_a_user() {
    let r = rig();
    let trusted = r.tmp.path().join("trusted");
    let p = shim(&trusted, "pacman", 0);
    let file = Path::new("/tmp/x.pkg.tar.zst");
    let mut env = env_with(&trusted, &r.home);
    assert!(package_command(InstallKind::Pacman, file, &env).is_ok());
    for mode in [0o775, 0o757, 0o777] {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(package_command(InstallKind::Pacman, file, &env).is_err(), "mode {mode:o}");
    }
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Ownership is enforced outside tests: a file owned by an ordinary user is refused.
    env.check_ownership = true;
    use std::os::unix::fs::MetadataExt;
    let owned_by_root = std::fs::metadata(&p).unwrap().uid() == 0;
    assert_eq!(package_command(InstallKind::Pacman, file, &env).is_ok(), owned_by_root);
}

#[test]
fn package_manager_gets_a_fixed_path_not_the_users() {
    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 0, "");
    let envfile = p.r.tmp.path().join("env.txt");
    let esc = p.r.tmp.path().join("env-esc");
    write_exec(&esc, &format!("#!/bin/sh\n/usr/bin/env > '{}'\nexit 0\n", envfile.display()));
    let mut o = p.opts();
    o.escalation = Some(vec![esc.into_os_string()]);
    install(&o, &mut |_| {}).unwrap();
    let env = std::fs::read_to_string(&envfile).unwrap();
    assert!(env.lines().any(|l| l == "PATH=/usr/sbin:/usr/bin:/sbin:/bin"), "{env}");
    assert!(!env.contains(p.r.bin.to_str().unwrap()), "the user's path never reaches the elevated command: {env}");
}

#[test]
fn package_path_must_be_absolute_and_a_relative_tmpdir_is_ignored() {
    let r = rig();
    shim(&r.bin, "pacman", 0);
    let env = env_with(&r.bin, &r.home);
    let e = package_command(InstallKind::Pacman, Path::new("relative/x.pkg.tar.zst"), &env).unwrap_err();
    assert_eq!(e.code, ErrorCode::InstallFailed);
    use praeco::install::temp_base;
    assert_eq!(temp_base(Some("relative/tmp".into())), PathBuf::from("/tmp"));
    assert_eq!(temp_base(Some("".into())), PathBuf::from("/tmp"));
    assert_eq!(temp_base(None), PathBuf::from("/tmp"));
    assert_eq!(temp_base(Some("/var/tmp".into())), PathBuf::from("/var/tmp"));
}

#[test]
fn forgejo_attachment_urls_work_when_listed() {
    let r = rig();
    let name = "fairfax-x86_64.AppImage";
    r.server.route("/attachments/uuid-1", 200, &[], b"NEW".to_vec());
    r.server.route("/attachments/uuid-2", 200, &[], format!("{}  {name}\n", sha256_hex(b"NEW")).into_bytes());
    let mut rel = r.server.release("v1.0.5", "", &[name, &format!("{name}.sha256")]);
    rel["assets"][0]["browser_download_url"] = json!(format!("{}/attachments/uuid-1", r.server.base));
    rel["assets"][1]["browser_download_url"] = json!(format!("{}/attachments/uuid-2", r.server.base));
    r.server.publish_releases(vec![rel]);
    let (o, img) = r.appimage("1.0.5");
    install(&o, &mut |_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&img).unwrap(), "NEW");
}

#[test]
fn pkexec_127_means_no_agent_only_when_it_says_so() {
    let p = pkg_rig(InstallKind::Pacman, "fairfax-x86_64.pkg.tar.zst", &["pacman"], 127, "Error getting authority: No authentication agent found.");
    let pk = escalator(&p.r.tmp.path().join("pk"), "pkexec", &p.rec, 127, "Error executing command as another user: No authentication agent found.");
    let mut o = p.opts();
    o.escalation = Some(vec![pk.into_os_string()]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::NeedsTerminal);

    let pk = escalator(&p.r.tmp.path().join("pk2"), "pkexec", &p.rec, 127, "Error executing command as another user: Not authorized");
    o.escalation = Some(vec![pk.into_os_string()]);
    assert_eq!(install(&o, &mut |_| {}).unwrap_err().code, ErrorCode::AuthFailed);
}
