//! Restart the app after an update: ask it to quit, wait for it to go, start
//! the new version fully detached.

use crate::error::{Error, ErrorCode, Result};
use crate::progress::Event;
use nix::errno::Errno;
use nix::sys::signal::{kill, signal, SigHandler, Signal};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Variables set by an AppImage runtime that describe the *old* mount; the new
/// AppImage sets its own.
const STALE_APPIMAGE_VARS: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

#[derive(Debug, Clone)]
pub struct RestartSpec {
    /// The running app. `None` skips the quit-and-wait step.
    pub pid: Option<u32>,
    /// Command line that starts the new version; `argv[0]` is the program.
    pub argv: Vec<OsString>,
    /// How long to wait for the app to exit after SIGTERM.
    pub timeout: Duration,
}

impl RestartSpec {
    pub fn new(pid: Option<u32>, argv: Vec<OsString>) -> RestartSpec {
        RestartSpec { pid, argv, timeout: Duration::from_secs(30) }
    }
}

fn fail(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::RestartFailed, msg)
}

/// Is `pid` a live process? A zombie (exited, not yet reaped) counts as gone.
pub fn is_running(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else { return false };
    match kill(Pid::from_raw(raw), None) {
        Err(Errno::ESRCH) => return false,
        Err(_) | Ok(()) => {}
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rfind(')').and_then(|i| stat[i + 1..].trim_start().chars().next());
            state != Some('Z') && state != Some('X')
        }
        Err(_) => false,
    }
}

/// Start `argv` in its own session with no terminal and no stdio, so it
/// survives this process and the terminal that started it.
pub fn launch_detached(argv: &[OsString]) -> Result<()> {
    let Some(prog) = argv.first() else { return Err(fail("There is no command to start the new version with")) };
    let mut cmd = Command::new(prog);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for v in STALE_APPIMAGE_VARS {
        cmd.env_remove(v);
    }
    // SAFETY: the closure runs between fork and exec and only calls
    // async-signal-safe functions (setsid, sigaction), allocating nothing.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            // An ignored SIGHUP would be inherited across exec; restore it.
            signal(Signal::SIGHUP, SigHandler::SigDfl).map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| fail(format!("Could not start {}: {e}", prog.to_string_lossy())))?;
    std::thread::sleep(Duration::from_millis(400));
    if let Ok(Some(status)) = child.try_wait() {
        if !status.success() {
            return Err(fail(format!("The new version exited right after starting ({status})")));
        }
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn restart(spec: &RestartSpec, emit: &mut dyn FnMut(&Event)) -> Result<()> {
    if spec.argv.is_empty() {
        return Err(fail("There is no command to start the new version with"));
    }
    if let Some(pid) = spec.pid {
        let raw = i32::try_from(pid).ok().filter(|p| *p > 1 && *p as u32 != std::process::id());
        let Some(raw) = raw else { return Err(fail(format!("{pid} is not a process praeco can restart"))) };
        emit(&Event::Restarting { message: "Closing the app so the new version can start...".into() });
        match kill(Pid::from_raw(raw), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(e) => return Err(fail(format!("Could not ask the app to quit: {e}. It is installed; please restart it yourself."))),
        }
        let deadline = Instant::now() + spec.timeout;
        while is_running(pid) {
            if Instant::now() >= deadline {
                return Err(fail(format!(
                    "The app did not close within {} seconds. The update is installed; close the app and open it again to use it.",
                    spec.timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    emit(&Event::Restarting { message: "Starting the new version...".into() });
    launch_detached(&spec.argv)
}
