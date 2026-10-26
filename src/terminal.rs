//! Terminal mode: praeco starts itself inside the user's terminal to do the
//! install with `sudo`, and relays the worker's progress as ordinary events.
//!
//! Parent and worker talk through an events file in a private temp directory:
//! the worker appends one JSON event per line (its first line is `hello` with
//! its pid, so the parent can tell if the window is closed early); the parent
//! tails the file.

use crate::detect::{self, Env};
use crate::error::{Error, ErrorCode, Result};
use crate::install::{InstallOptions, Installed};
use crate::progress::Event;
use crate::restart::is_running;
use crate::source::Host;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub name: String,
    pub program: PathBuf,
    /// Arguments placed before the command to run.
    pub prefix: Vec<String>,
}

/// Known terminals and the arguments that make them run a command.
const KNOWN: [(&str, &[&str]); 13] = [
    ("xdg-terminal-exec", &[]),
    ("foot", &[]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("ghostty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("gnome-terminal", &["--wait", "--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("terminator", &["-x"]),
    ("x-terminal-emulator", &["-e"]),
    ("urxvt", &["-e"]),
    ("xterm", &["-e"]),
];

/// Terminals whose `-e` takes one string that they split again (so a path with
/// spaces would break) and that have no argv form: never used.
const NO_ARGV_FORM: [&str; 2] = ["tilix", "lxterminal"];

fn known_prefix(name: &str) -> Vec<String> {
    KNOWN.iter().find(|(n, _)| *n == name).map(|(_, p)| p.iter().map(|s| s.to_string()).collect()).unwrap_or_else(|| vec!["-e".into()])
}

/// Does the program (after resolving symlinks and alternatives such as
/// `x-terminal-emulator`) turn out to be a terminal without an argv form?
fn argv_safe(program: &std::path::Path) -> bool {
    let real = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let base = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let given = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    !NO_ARGV_FORM.contains(&base.as_str()) && !NO_ARGV_FORM.contains(&given.as_str())
}

fn known_terminal(env: &Env) -> Option<Terminal> {
    KNOWN.iter().skip(1).find_map(|(n, p)| {
        env.which(n)
            .filter(|program| argv_safe(program))
            .map(|program| Terminal { name: (*n).into(), program, prefix: p.iter().map(|s| s.to_string()).collect() })
    })
}

/// `xdg-terminal-exec` first, then `$TERMINAL` (may carry its own arguments),
/// then the common terminals in the order above. Terminals without an argv
/// form are refused even behind a symlink.
pub fn find_terminal(env: &Env) -> Option<Terminal> {
    if let Some(p) = env.which("xdg-terminal-exec") {
        return Some(Terminal { name: "xdg-terminal-exec".into(), program: p, prefix: Vec::new() });
    }
    if let Some(t) = &env.terminal {
        let mut parts = t.split_whitespace();
        if let Some(first) = parts.next() {
            let program = if first.contains('/') { Some(PathBuf::from(first)) } else { env.which(first) };
            if let Some(program) = program {
                if !argv_safe(&program) {
                    return known_terminal(env);
                }
                let base = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut prefix: Vec<String> = parts.map(str::to_string).collect();
                prefix.extend(known_prefix(&base));
                return Some(Terminal { name: base, program, prefix });
            }
        }
    }
    known_terminal(env)
}

/// Command line for the worker, mirroring `opts`.
pub fn worker_args(opts: &InstallOptions, events_file: &std::path::Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["install".into()];
    match &opts.source.host {
        Host::GitHub { api_base } => {
            a.extend(["--github".into(), format!("{}/{}", opts.source.owner, opts.source.repo).into()]);
            if api_base != crate::source::GITHUB_API {
                a.extend(["--github-api-base".into(), api_base.into()]);
            }
        }
        Host::Forgejo { base } => {
            a.extend(["--forgejo".into(), base.into(), format!("{}/{}", opts.source.owner, opts.source.repo).into()]);
        }
    }
    a.extend(["--version".into(), (&opts.version).into(), "--exe".into(), opts.exe.clone().into()]);
    if opts.include_prereleases {
        a.push("--prereleases".into());
    }
    if let Some(p) = &opts.env.appimage {
        a.extend(["--appimage".into(), p.clone().into()]);
    }
    if let Some(p) = &opts.env.appdir {
        a.extend(["--appdir".into(), p.clone().into()]);
    }
    for kind in crate::detect::InstallKind::ALL {
        for g in opts.patterns.for_kind(kind) {
            a.extend(["--asset-pattern".into(), format!("{kind}={g}").into()]);
        }
    }
    a.extend(["--mode".into(), "terminal-inner".into(), "--events-file".into(), events_file.into()]);
    a
}

struct Tail {
    path: PathBuf,
    pos: u64,
    partial: String,
}

impl Tail {
    fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let Ok(mut f) = std::fs::File::open(&self.path) else { return out };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return out;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return out;
        }
        self.pos += buf.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&buf));
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            if let Ok(ev) = serde_json::from_str::<Event>(line.trim()) {
                out.push(ev);
            }
        }
        out
    }
}

/// Run the install in the user's terminal and relay its progress. Returns
/// once the worker reports `installed` or fails.
pub fn install_via_terminal(opts: &InstallOptions, emit: &mut dyn FnMut(&Event)) -> Result<Installed> {
    let det = detect::detect(&opts.env, &opts.exe);
    if det.kind.is_none() {
        return Err(Error::new(ErrorCode::Unsupported, det.reason));
    }
    let term = find_terminal(&opts.env).ok_or_else(|| {
        Error::new(ErrorCode::InstallFailed, "No terminal program was found. Set the TERMINAL environment variable to your terminal, or install with your package manager.")
    })?;
    let me = std::env::current_exe().map_err(|e| Error::new(ErrorCode::Io, format!("Cannot locate praeco itself: {e}")))?;
    let dir = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).prefix("praeco-term-").tempdir()?;
    let events = dir.path().join("events.jsonl");
    std::fs::File::create(&events)?;

    let mut cmd = Command::new(&term.program);
    cmd.args(&term.prefix).arg(&me).args(worker_args(opts, &events)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut launcher = cmd.spawn().map_err(|e| Error::new(ErrorCode::InstallFailed, format!("Could not open {}: {e}", term.name)))?;
    emit(&Event::TerminalOpened { terminal: term.name.clone() });

    let mut tail = Tail { path: events, pos: 0, partial: String::new() };
    let mut worker: Option<u32> = None;
    let mut launcher_gone_at: Option<Instant> = None;
    let handle = |ev: Event, worker: &mut Option<u32>, emit: &mut dyn FnMut(&Event)| -> Option<Result<Installed>> {
        match ev {
            Event::Hello { pid } => {
                *worker = Some(pid);
                None
            }
            Event::Error { code, message } => Some(Err(Error::new(ErrorCode::from_code(&code), message))),
            Event::NeedsTerminal { message } => Some(Err(Error::new(ErrorCode::InstallFailed, message))),
            Event::Installed { version, kind } => {
                emit(&Event::Installed { version: version.clone(), kind });
                Some(Ok(Installed { version, kind }))
            }
            other => {
                emit(&other);
                None
            }
        }
    };
    loop {
        for ev in tail.drain() {
            if let Some(r) = handle(ev, &mut worker, emit) {
                return r;
            }
        }
        let closed = |what: &str| Error::new(ErrorCode::TerminalClosed, format!("The terminal was closed {what}. Check whether the update was installed."));
        match worker {
            Some(pid) if !is_running(pid) => {
                for ev in tail.drain() {
                    if let Some(r) = handle(ev, &mut worker, emit) {
                        return r;
                    }
                }
                return Err(closed("before the update finished"));
            }
            Some(_) => {}
            None => {
                if let Ok(Some(status)) = launcher.try_wait() {
                    let since = *launcher_gone_at.get_or_insert_with(Instant::now);
                    // Terminal servers (gnome-terminal) return at once; give the window time to appear.
                    if !status.success() || since.elapsed() > Duration::from_secs(20) {
                        return Err(Error::new(ErrorCode::InstallFailed, format!("{} did not start the update. Try installing from a terminal yourself.", term.name)));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;
    use std::os::unix::fs::PermissionsExt;

    fn exe(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn env(dir: &std::path::Path, terminal: Option<&str>) -> Env {
        let mut e = Env::from_process();
        e.path = dir.as_os_str().to_os_string();
        e.terminal = terminal.map(str::to_string);
        e
    }

    #[test]
    fn terminals_without_an_argv_form_are_never_used() {
        let d = tempfile::tempdir().unwrap();
        exe(d.path(), "tilix");
        exe(d.path(), "lxterminal");
        assert_eq!(find_terminal(&env(d.path(), None)), None, "not even from the built-in list");
        assert_eq!(find_terminal(&env(d.path(), Some("tilix"))), None, "nor from $TERMINAL");
        exe(d.path(), "foot");
        assert_eq!(find_terminal(&env(d.path(), Some("tilix"))).unwrap().name, "foot");
    }

    #[test]
    fn symlinks_to_unsafe_terminals_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = exe(d.path(), "lxterminal");
        std::os::unix::fs::symlink(&real, d.path().join("x-terminal-emulator")).unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("myterm")).unwrap();
        assert_eq!(find_terminal(&env(d.path(), None)), None, "x-terminal-emulator -> lxterminal");
        assert_eq!(find_terminal(&env(d.path(), Some("myterm"))), None, "$TERMINAL symlink -> lxterminal");
        exe(d.path(), "xterm");
        assert_eq!(find_terminal(&env(d.path(), Some("myterm"))).unwrap().name, "xterm");
    }

    #[test]
    fn terminal_command_keeps_paths_with_spaces_as_single_arguments() {
        let d = tempfile::tempdir().unwrap();
        exe(d.path(), "alacritty");
        let t = find_terminal(&env(d.path(), None)).unwrap();
        assert_eq!(t.prefix, ["-e"]);
        let mut o = crate::install::InstallOptions::new(Source::github("o/r").unwrap(), "1.0.5", "/home/u/My Apps/fairfax");
        o.env.appimage = Some("/home/u/My Apps/Fairfax.AppImage".into());
        let args = worker_args(&o, std::path::Path::new("/run/user/1000/dir with space/events.jsonl"));
        assert!(args.iter().any(|a| a == "/home/u/My Apps/fairfax"));
        assert!(args.iter().any(|a| a == "/home/u/My Apps/Fairfax.AppImage"));
        assert!(args.iter().any(|a| a == "/run/user/1000/dir with space/events.jsonl"));
    }
}
