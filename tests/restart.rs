use praeco::restart::{is_running, launch_detached, restart, RestartSpec};
use praeco::ErrorCode;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
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

#[test]
fn quits_the_old_process_then_starts_the_new_one() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("started");
    let mut old = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(is_running(old.id()));
    let script = format!("echo started > '{}'", marker.display());
    let mut events = Vec::new();
    let spec = RestartSpec::new(Some(old.id()), argv(&["sh", "-c", &script]));
    restart(&spec, &mut |e| events.push(e.clone())).unwrap();
    assert!(wait_for(&marker), "new version was not started");
    assert!(!is_running(old.id()));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(old.wait().unwrap().signal(), Some(15), "the old process got SIGTERM");
    assert_eq!(events.len(), 2);
}

#[test]
fn a_process_that_will_not_quit_is_reported_and_nothing_is_started() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("started");
    let mut stubborn = Command::new("sh").args(["-c", "trap '' TERM; while :; do sleep 1; done"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let script = format!("echo started > '{}'", marker.display());
    let mut spec = RestartSpec::new(Some(stubborn.id()), argv(&["sh", "-c", &script]));
    spec.timeout = Duration::from_secs(1);
    let e = restart(&spec, &mut |_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::RestartFailed);
    assert!(e.message.contains("did not close within 1 seconds"), "{}", e.message);
    assert!(!marker.exists());
    stubborn.kill().unwrap();
    stubborn.wait().unwrap();
}

#[test]
fn relaunch_runs_in_its_own_session() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("stat");
    launch_detached(&argv(&["sh", "-c", &format!("cat /proc/$$/stat > '{}'", out.display())])).unwrap();
    assert!(wait_for(&out));
    let stat = std::fs::read_to_string(&out).unwrap();
    let rest: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    // After the name: state, ppid, pgrp, session.
    let (pgrp, session): (u32, u32) = (rest[2].parse().unwrap(), rest[3].parse().unwrap());
    let pid: u32 = stat.split(' ').next().unwrap().parse().unwrap();
    assert_eq!(session, pid, "the relaunched app is a session leader");
    assert_eq!(pgrp, pid);
    assert_ne!(session, unsafe_sid());
}

fn unsafe_sid() -> u32 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let rest: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    rest[3].parse().unwrap()
}

#[test]
fn without_a_pid_it_just_launches() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("m");
    let spec = RestartSpec::new(None, argv(&["sh", "-c", &format!("echo hi > '{}'", marker.display())]));
    restart(&spec, &mut |_| {}).unwrap();
    assert!(wait_for(&marker));
}

#[test]
fn bad_requests_are_refused() {
    let me = std::process::id();
    for pid in [0u32, 1, me] {
        let e = restart(&RestartSpec::new(Some(pid), argv(&["true"])), &mut |_| {}).unwrap_err();
        assert_eq!(e.code, ErrorCode::RestartFailed, "pid {pid}");
    }
    assert_eq!(restart(&RestartSpec::new(None, vec![]), &mut |_| {}).unwrap_err().code, ErrorCode::RestartFailed);
    let e = launch_detached(&argv(&["/nonexistent/praeco-test-app"])).unwrap_err();
    assert_eq!(e.code, ErrorCode::RestartFailed);
    let e = launch_detached(&argv(&["sh", "-c", "exit 3"])).unwrap_err();
    assert_eq!(e.code, ErrorCode::RestartFailed);
}
