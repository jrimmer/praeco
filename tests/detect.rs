mod common;
use common::*;
use praeco::detect::{detect, polkit_agent_running, InstallKind};
use std::path::Path;

struct Rig {
    _tmp: tempfile::TempDir,
    bin: std::path::PathBuf,
    home: std::path::PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Rig { _tmp: tmp, bin, home }
}

const SYSTEM_EXE: &str = "/usr/bin/praeco-test-nonexistent-app";

#[test]
fn pacman_owner() {
    let r = rig();
    shim(&r.bin, "pacman", 0);
    shim(&r.bin, "dpkg", 1);
    shim(&r.bin, "rpm", 1);
    let d = detect(&env_with(&r.bin, &r.home), Path::new(SYSTEM_EXE));
    assert_eq!(d.kind, Some(InstallKind::Pacman));
    assert!(d.detail.unwrap().contains("myapp"));
    assert_eq!(std::fs::read_to_string(r.bin.join("pacman.log")).unwrap().trim(), format!("-Qo {SYSTEM_EXE}"));
}

#[test]
fn deb_owner() {
    let r = rig();
    shim(&r.bin, "pacman", 1);
    shim(&r.bin, "dpkg", 0);
    let d = detect(&env_with(&r.bin, &r.home), Path::new(SYSTEM_EXE));
    assert_eq!(d.kind, Some(InstallKind::Deb));
    assert_eq!(std::fs::read_to_string(r.bin.join("dpkg.log")).unwrap().trim(), format!("-S {SYSTEM_EXE}"));
}

#[test]
fn rpm_owner() {
    let r = rig();
    shim(&r.bin, "rpm", 0);
    let d = detect(&env_with(&r.bin, &r.home), Path::new(SYSTEM_EXE));
    assert_eq!(d.kind, Some(InstallKind::Rpm));
    assert_eq!(std::fs::read_to_string(r.bin.join("rpm.log")).unwrap().trim(), format!("-qf {SYSTEM_EXE}"));
}

#[test]
fn appimage_wins_over_package_ownership() {
    let r = rig();
    shim(&r.bin, "pacman", 0);
    let img = r.home.join("Apps/MyApp.AppImage");
    write_exec(&img, "#!/bin/sh\n");
    let mut env = env_with(&r.bin, &r.home);
    env.appimage = Some(img.clone());
    env.appdir = Some("/tmp/.mount_abc".into());
    let d = detect(&env, Path::new("/tmp/.mount_abc/usr/bin/myapp"));
    assert_eq!(d.kind, Some(InstallKind::AppImage));
    assert_eq!(d.target, img);

    // APPDIR must contain the executable.
    env.appdir = Some("/tmp/.mount_other".into());
    assert_eq!(detect(&env, Path::new("/tmp/.mount_abc/usr/bin/myapp")).kind, Some(InstallKind::Pacman));

    // A stray APPIMAGE without APPDIR says nothing about this app.
    env.appdir = None;
    assert_eq!(detect(&env, Path::new(SYSTEM_EXE)).kind, Some(InstallKind::Pacman));
    assert_eq!(detect(&env, Path::new("/tmp/.mount_abc/usr/bin/myapp")).kind, Some(InstallKind::Pacman));

    // A stale APPIMAGE pointing at nothing is ignored.
    env.appdir = Some("/tmp/.mount_abc".into());
    env.appimage = Some(r.home.join("gone.AppImage"));
    assert_eq!(detect(&env, Path::new("/tmp/.mount_abc/usr/bin/myapp")).kind, Some(InstallKind::Pacman));
}

#[test]
fn tarball_under_home() {
    let r = rig();
    shim(&r.bin, "pacman", 1);
    let exe = r.home.join(".local/opt/myapp/bin/myapp");
    write_exec(&exe, "#!/bin/sh\n");
    let d = detect(&env_with(&r.bin, &r.home), &exe);
    assert_eq!(d.kind, Some(InstallKind::Tarball));
    assert_eq!(d.target, std::fs::canonicalize(&exe).unwrap());
}

#[test]
fn source_build_is_unsupported_with_a_reason() {
    let r = rig();
    shim(&r.bin, "pacman", 1);
    shim(&r.bin, "dpkg", 1);
    shim(&r.bin, "rpm", 1);
    let d = detect(&env_with(&r.bin, &r.home), Path::new(SYSTEM_EXE));
    assert_eq!(d.kind, None);
    assert!(d.reason.contains("source build"), "{}", d.reason);
    // No package managers at all behaves the same.
    let empty = r.bin.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(detect(&env_with(&empty, &r.home), Path::new(SYSTEM_EXE)).kind, None);
}

#[test]
fn polkit_agent_probe() {
    let r = rig();
    let proc = r.bin.join("proc");
    let mut env = env_with(&r.bin, &r.home);
    env.proc_root = proc.clone();
    let mk = |pid: &str, comm: &str| {
        std::fs::create_dir_all(proc.join(pid)).unwrap();
        std::fs::write(proc.join(pid).join("comm"), format!("{comm}\n")).unwrap();
    };
    assert!(!polkit_agent_running(&env), "no proc dir");
    mk("1", "systemd");
    mk("200", "polkitd");
    mk("201", "bash");
    std::fs::create_dir_all(proc.join("self")).unwrap();
    std::fs::write(proc.join("self/comm"), "polkit-gnome-au\n").unwrap();
    assert!(!polkit_agent_running(&env), "polkitd is the daemon, not an agent; non-numeric entries are ignored");
    mk("300", "hyprpolkitagent");
    assert!(polkit_agent_running(&env));
    std::fs::remove_dir_all(proc.join("300")).unwrap();
    mk("301", "gnome-shell");
    assert!(polkit_agent_running(&env));
}
