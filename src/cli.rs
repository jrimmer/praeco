//! The `praeco` command line. Flags and output formats are documented in the
//! README and are meant to stay stable.

use crate::assets::AssetPatterns;
use crate::check::{self, CheckOptions, CheckResult};
use crate::detect::{Env, InstallKind};
use crate::error::{Error, ErrorCode, Result};
use crate::hook;
use crate::http::Http;
use crate::install::{self, InstallInfo, InstallOptions, Mode};
use crate::notes::DEFAULT_MARKER;
use crate::progress::Event;
use crate::registry;
use crate::restart::RestartSpec;
use crate::source::Source;
use crate::update;
use crate::updateall;
use serde::Serialize;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub const HELP: &str = "\
praeco - update checks and in-place updates for Linux desktop apps

USAGE
  praeco check   SOURCE --current VERSION [--marker TEXT] [--prereleases] [--exe PATH] [--json]
  praeco install SOURCE --version VERSION --exe PATH [--mode dialog|terminal]
                 [--restart-pid PID] [--restart -- COMMAND [ARGS...]]
  praeco detect  --exe PATH
  praeco init    --name NAME SOURCE (--exe PATH | --appimage PATH) [--version-arg ARG]... [--hook auto|none] [--json]
  praeco update-all [--mode auto|inline|dialog|terminal] [--json]
  praeco --help | --version

SOURCE (one of)
  --github OWNER/REPO
  --forgejo BASE_URL OWNER/REPO       e.g. --forgejo https://code.example.com owner/repo

COMMON OPTIONS
  --prereleases            include prereleases (default: stable releases only)
  --asset-pattern K=GLOB   asset name pattern for install kind K (pacman, deb, rpm,
                           appimage, tarball); repeatable; '{arch}' expands to the
                           machine's architecture names
  --token-env NAME         environment variable holding an API token (default PRAECO_TOKEN)
  --appimage PATH          the AppImage the app runs from (default: $APPIMAGE)
  --appdir PATH            where that AppImage is mounted (default: $APPDIR)
  --github-api-base URL    GitHub-compatible API base (GitHub Enterprise)

check OPTIONS
  --current VERSION        the running app's version (a leading v is fine)
  --marker TEXT            cut release notes at the first line starting with TEXT
                           (default '## Install'; '' keeps whole bodies)
  --exe PATH               also report how that executable is installed and which asset applies
  --no-cache               do not use the ETag cache
  --json                   machine-readable output

install OPTIONS
  --version VERSION        the version to install
  --exe PATH               the running app's executable
  --mode dialog|terminal   dialog (default): pkexec permission prompt; terminal: open a terminal, use sudo
  --restart-pid PID        quit this process (SIGTERM) and wait for it before relaunching
  --restart -- CMD ARGS... command line that starts the new version (must be last)
  --restart-timeout SECS   how long to wait for the app to quit (default 30)
  --detach                 continue in a background process in its own session and return at
                           once; progress goes to --progress-file as JSON lines (see README)
  --progress-file PATH     with --detach: where to write the events. Must not exist; created
                           0600 in a directory only you can write to. Default: a new private
                           temporary directory (its path is printed in the `detached` event).

init OPTIONS
  --name NAME              the app's registry name (letters, digits, . _ -)
  --exe PATH               the app's executable; or --appimage PATH for the .AppImage file
                           (default: $APPIMAGE when set)
  --version-arg ARG        argument that makes the app print its version (repeatable;
                           default: --version)
  --hook auto|none         auto (default): also add praeco to the system's update command
                           where there is one (Omarchy: `omarchy update`)

update-all OPTIONS
  --mode auto|inline|dialog|terminal
                           how to get administrator rights; auto (default): sudo in this
                           terminal if there is one, else the permission prompt, else a
                           terminal window. Never asks whether to update.

EXIT CODES
  0 success (check: also when no update exists)   3 needs_terminal (retry with --mode terminal)
  1 failure (network, API, checksum, install)     4 unsupported install (source build)
  2 usage error                                   5 installed, but the app could not be restarted

install prints one JSON object per line on stdout; check --json prints one object.
";

#[derive(Default)]
struct Args {
    raw: Vec<OsString>,
    detach: bool,
    progress_file: Option<PathBuf>,
    command: String,
    github: Option<String>,
    github_api_base: Option<String>,
    forgejo: Option<(String, String)>,
    current: Option<String>,
    marker: Option<String>,
    prereleases: bool,
    exe: Option<PathBuf>,
    json: bool,
    token_env: Option<String>,
    no_cache: bool,
    patterns: Vec<String>,
    version: Option<String>,
    mode: Option<String>,
    restart_pid: Option<u32>,
    restart: bool,
    restart_argv: Vec<OsString>,
    restart_timeout: Option<u64>,
    events_file: Option<PathBuf>,
    appimage: Option<PathBuf>,
    appdir: Option<PathBuf>,
    name: Option<String>,
    version_args: Vec<String>,
    hook: Option<String>,
}

fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::Usage, msg)
}

fn parse(args: Vec<OsString>) -> Result<Args> {
    let mut a = Args { raw: args.clone(), ..Args::default() };
    let mut it = args.into_iter();
    a.command = it.next().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
    while let Some(raw) = it.next() {
        let Some(flag) = raw.to_str().map(str::to_string) else {
            return Err(usage(format!("Unexpected argument {raw:?}")));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(OsString::from(v))),
            _ => (flag.clone(), None),
        };
        let value = |it: &mut std::vec::IntoIter<OsString>| -> Result<OsString> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| usage(format!("{name} needs a value")))
        };
        let text = |v: OsString| v.to_string_lossy().into_owned();
        match name.as_str() {
            "--github" => a.github = Some(text(value(&mut it)?)),
            "--github-api-base" => a.github_api_base = Some(text(value(&mut it)?)),
            "--forgejo" => {
                let base = text(value(&mut it)?);
                let slug = it.next().map(text).ok_or_else(|| usage("--forgejo needs a base URL and owner/repo"))?;
                a.forgejo = Some((base, slug));
            }
            "--current" => a.current = Some(text(value(&mut it)?)),
            "--marker" => a.marker = Some(text(value(&mut it)?)),
            "--prereleases" => a.prereleases = true,
            "--exe" => a.exe = Some(PathBuf::from(value(&mut it)?)),
            "--json" => a.json = true,
            "--token-env" => a.token_env = Some(text(value(&mut it)?)),
            "--no-cache" => a.no_cache = true,
            "--asset-pattern" => a.patterns.push(text(value(&mut it)?)),
            "--version" if a.command != "--version" => a.version = Some(text(value(&mut it)?)),
            "--mode" => a.mode = Some(text(value(&mut it)?)),
            "--restart-pid" => {
                let v = text(value(&mut it)?);
                a.restart_pid = Some(v.parse().map_err(|_| usage(format!("'{v}' is not a process id")))?);
            }
            "--restart-timeout" => {
                let v = text(value(&mut it)?);
                a.restart_timeout = Some(v.parse().map_err(|_| usage(format!("'{v}' is not a number of seconds")))?);
            }
            "--restart" => {
                a.restart = true;
                match it.next() {
                    Some(sep) if sep == "--" => a.restart_argv = it.by_ref().collect(),
                    _ => return Err(usage("--restart must be followed by -- and the command that starts the app")),
                }
            }
            "--detach" => a.detach = true,
            "--progress-file" => a.progress_file = Some(PathBuf::from(value(&mut it)?)),
            "--events-file" => a.events_file = Some(PathBuf::from(value(&mut it)?)),
            "--appimage" => a.appimage = Some(PathBuf::from(value(&mut it)?)),
            "--appdir" => a.appdir = Some(PathBuf::from(value(&mut it)?)),
            "--name" => a.name = Some(text(value(&mut it)?)),
            "--version-arg" => a.version_args.push(text(value(&mut it)?)),
            "--hook" => a.hook = Some(text(value(&mut it)?)),
            _ => return Err(usage(format!("Unknown option {flag}. Try praeco --help."))),
        }
    }
    Ok(a)
}

impl Args {
    fn source(&self) -> Result<Source> {
        match (&self.github, &self.forgejo) {
            (Some(g), None) => match &self.github_api_base {
                Some(base) => Source::github_with_api_base(g, base),
                None => Source::github(g),
            },
            (None, Some((base, slug))) => Source::forgejo(base, slug),
            (None, None) => Err(usage("Say where releases come from: --github OWNER/REPO or --forgejo BASE_URL OWNER/REPO")),
            _ => Err(usage("Use either --github or --forgejo, not both")),
        }
    }

    fn token(&self) -> Option<String> {
        let name = self.token_env.as_deref().unwrap_or("PRAECO_TOKEN");
        std::env::var(name).ok().filter(|t| !t.trim().is_empty())
    }

    fn patterns(&self) -> Result<AssetPatterns> {
        let mut p = AssetPatterns::default();
        let mut seen: Vec<InstallKind> = Vec::new();
        for spec in &self.patterns {
            p.apply_override(spec, &mut seen).map_err(usage)?;
        }
        Ok(p)
    }

    fn env(&self) -> Env {
        let mut e = Env::from_process();
        if let Some(p) = &self.appimage {
            e.appimage = Some(p.clone());
        }
        if let Some(p) = &self.appdir {
            e.appdir = Some(p.clone());
        }
        e
    }

    fn need<T: Clone>(&self, v: &Option<T>, flag: &str) -> Result<T> {
        v.clone().ok_or_else(|| usage(format!("{flag} is required for praeco {}", self.command)))
    }
}

fn out_line(s: &str) {
    let mut o = std::io::stdout().lock();
    // A closed pipe (the app quit during a restart) must not kill praeco.
    let _ = writeln!(o, "{s}");
    let _ = o.flush();
}

fn error_event(e: &Error) -> Event {
    if e.code == ErrorCode::NeedsTerminal {
        Event::NeedsTerminal { message: e.message.clone() }
    } else {
        Event::Error { code: e.code.as_str().to_string(), message: e.message.clone() }
    }
}

#[derive(Serialize)]
struct CheckOut<'a> {
    ok: bool,
    #[serde(flatten)]
    result: &'a CheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    install: Option<InstallInfo>,
}

#[derive(Serialize)]
struct DetectOut {
    ok: bool,
    #[serde(flatten)]
    install: InstallInfo,
}

fn cmd_check(a: &Args) -> Result<()> {
    let mut opts = CheckOptions::new(a.source()?, a.need(&a.current, "--current")?);
    opts.marker = a.marker.clone().unwrap_or_else(|| DEFAULT_MARKER.to_string());
    opts.include_prereleases = a.prereleases;
    opts.token = a.token();
    if a.no_cache {
        opts.cache_dir = None;
    }
    let (result, releases) = check::check_full(&Http::for_source(&opts.source), &opts)?;
    let install = a.exe.as_ref().map(|exe| {
        let latest = releases.iter().max_by(|x, y| x.version.cmp(&y.version));
        install::describe(&a.env(), exe, latest, &a.patterns().unwrap_or_default())
    });
    if a.json {
        out_line(&serde_json::to_string(&CheckOut { ok: true, result: &result, install }).expect("serialises"));
    } else {
        print_check(&result, install.as_ref());
    }
    Ok(())
}

fn print_check(r: &CheckResult, install: Option<&InstallInfo>) {
    if r.update_available {
        println!("Update available: {} -> {}", r.current, r.latest.as_deref().unwrap_or("?"));
        for rel in &r.releases {
            println!("\n== {} ==\n{}", rel.version, rel.notes);
        }
    } else {
        println!("Up to date ({}).", r.current);
    }
    if let Some(i) = install {
        println!("\nInstall: {} ({})", i.kind, i.reason);
        if let Some(a) = &i.asset {
            println!("Asset: {}", a.name);
        }
    }
}

fn cmd_detect(a: &Args) -> Result<()> {
    let exe = a.need(&a.exe, "--exe")?;
    let install = install::describe(&a.env(), &exe, None, &AssetPatterns::default());
    out_line(&serde_json::to_string(&DetectOut { ok: true, install }).expect("serialises"));
    Ok(())
}

#[cfg(unix)]
fn ignore_hangup() {
    use nix::sys::signal::{signal, SigHandler, Signal};
    // SAFETY: installing SIG_IGN is async-signal-safe and touches no Rust state.
    unsafe {
        let _ = signal(Signal::SIGHUP, SigHandler::SigIgn);
    }
}

fn install_options(a: &Args, mode: Mode, in_terminal: bool) -> Result<InstallOptions> {
    let mut o = InstallOptions::new(a.source()?, a.need(&a.version, "--version")?, a.need(&a.exe, "--exe")?);
    o.mode = mode;
    o.in_terminal = in_terminal;
    o.patterns = a.patterns()?;
    o.include_prereleases = a.prereleases;
    o.token = a.token();
    o.env = a.env();
    o.escalation = escalation_override();
    Ok(o)
}

fn cmd_install(a: &Args) -> Result<()> {
    let mode = match a.mode.as_deref() {
        None | Some("dialog") => Mode::Dialog,
        Some("terminal") => Mode::Terminal,
        Some("terminal-inner") => return cmd_install_in_terminal(a),
        Some(other) => return Err(usage(format!("'{other}' is not a mode; use dialog or terminal"))),
    };
    let opts = install_options(a, mode, false)?;
    if a.detach {
        return cmd_detach(a);
    }
    relocate_out_of_appimage(a)?;
    let restart = match (a.restart, a.restart_pid) {
        (true, pid) => {
            if a.restart_argv.is_empty() {
                return Err(usage("--restart needs the command that starts the app after --"));
            }
            let mut spec = RestartSpec::new(pid, a.restart_argv.clone());
            if let Some(t) = a.restart_timeout {
                spec.timeout = Duration::from_secs(t);
            }
            Some(spec)
        }
        (false, Some(_)) => return Err(usage("--restart-pid needs --restart -- COMMAND")),
        (false, None) => None,
    };
    ignore_hangup();
    let mut emit = |e: &Event| out_line(&e.to_line());
    let result = match update::run(&opts, restart.as_ref(), &mut emit) {
        Ok(()) => Ok(()),
        Err(e) => {
            emit(&error_event(&e));
            Err(e)
        }
    };
    remove_self_copy();
    result
}


/// Test seam, compiled into debug builds only: a release binary always
/// escalates with pkexec or sudo.
fn escalation_override() -> Option<Vec<OsString>> {
    #[cfg(debug_assertions)]
    if let Some(cmd) = std::env::var("PRAECO_ESCALATION_CMD").ok().filter(|c| !c.trim().is_empty()) {
        return Some(cmd.split_whitespace().map(OsString::from).collect());
    }
    None
}

#[derive(Serialize)]
struct InitOut<'a> {
    ok: bool,
    app: &'a registry::App,
    registry_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_error: Option<String>,
    hook: hook::HookOutcome,
}

/// `praeco init`: register an app so `update-all` keeps it current, and add
/// praeco to the system's own update command where there is one.
fn cmd_init(a: &Args) -> Result<()> {
    let name = a.need(&a.name, "--name")?;
    let source = a.source()?;
    let hook_mode = match a.hook.as_deref() {
        None | Some("auto") => true,
        Some("none") => false,
        Some(other) => return Err(usage(format!("'{other}' is not a hook mode; use auto or none"))),
    };
    let env = a.env();
    let inside_appimage = |exe: &std::path::Path| env.appdir.as_ref().is_some_and(|d| exe.starts_with(d));
    let (exe, appimage) = match (&env.appimage, &a.exe) {
        (Some(img), None) => (img.clone(), true),
        (Some(img), Some(exe)) if inside_appimage(exe) => (img.clone(), true),
        (_, Some(exe)) => (exe.clone(), false),
        (None, None) => return Err(usage("praeco init needs --exe PATH (or --appimage PATH)")),
    };
    let exe = std::path::absolute(exe)?;
    let spec = if let Some(g) = &a.github {
        registry::SourceSpec::Github { repo: g.clone(), api_base: a.github_api_base.clone() }
    } else {
        let (base, repo) = a.forgejo.clone().expect("source() accepted a forgejo source");
        registry::SourceSpec::Forgejo { base, repo }
    };
    drop(source);
    let mut version_cmd = vec![exe.to_string_lossy().into_owned()];
    if a.version_args.is_empty() {
        version_cmd.push("--version".into());
    } else {
        version_cmd.extend(a.version_args.iter().cloned());
    }
    let app = registry::App { name, source: spec, exe, appimage, version_cmd, prereleases: a.prereleases, asset_patterns: a.patterns.clone() };
    let registry_file = registry::save(&env, &app)?;
    let (version, version_error) = match registry::installed_version(&app) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e.message)),
    };
    let hook = if hook_mode {
        hook::ensure_omarchy_hook(&env, &std::env::current_exe()?, &hook::os_release_path())?
    } else {
        hook::HookOutcome::NotApplicable
    };
    if a.json {
        let out = InitOut { ok: true, app: &app, registry_file: registry_file.display().to_string(), version, version_error, hook };
        out_line(&serde_json::to_string(&out).expect("serialises"));
        return Ok(());
    }
    println!("Registered {} ({}).", app.name, registry_file.display());
    match (&version, &version_error) {
        (Some(v), _) => println!("Installed version: {v}."),
        (_, Some(e)) => println!("Warning: {e}"),
        _ => {}
    }
    match hook {
        hook::HookOutcome::Installed { path } => println!("Added praeco to the system update command ({}).", path.display()),
        hook::HookOutcome::Current { path } => println!("praeco is already part of the system update command ({}).", path.display()),
        hook::HookOutcome::Skipped { reason } => println!("Did not add praeco to the system update command: {reason}"),
        hook::HookOutcome::NotApplicable => {}
    }
    Ok(())
}

#[derive(Serialize)]
struct UpdateAllOut<'a> {
    ok: bool,
    apps: &'a [updateall::AppResult],
    problems: &'a [String],
}

/// `praeco update-all`; returns whether every app is fine.
fn cmd_update_all(a: &Args) -> Result<bool> {
    let mode = match a.mode.as_deref() {
        None => updateall::UpdateMode::Auto,
        Some(m) => updateall::UpdateMode::parse(m).ok_or_else(|| usage(format!("'{m}' is not a mode; use auto, inline, dialog or terminal")))?,
    };
    let env = a.env();
    let reg = registry::load(&env);
    for p in &reg.problems {
        eprintln!("praeco: {p}");
    }
    if reg.apps.is_empty() && !a.json {
        println!("No apps are registered with praeco.");
    }
    let opts = updateall::Options { mode, stdin_is_tty: std::io::IsTerminal::is_terminal(&std::io::stdin()), env, token: a.token(), escalation: escalation_override() };
    ignore_hangup();
    let json = a.json;
    let results = updateall::run(&reg, &opts, &mut |line| {
        if !json {
            println!("{line}");
        }
    });
    let all_ok = results.iter().all(|r| r.status != updateall::Status::Failed);
    if json {
        out_line(&serde_json::to_string(&UpdateAllOut { ok: all_ok, apps: &results, problems: &reg.problems }).expect("serialises"));
    }
    Ok(all_ok)
}

/// Is `exe` inside an AppImage mount (under `appdir`, or a `/tmp/.mount_*`)?
/// That mount vanishes when the app exits, taking praeco with it.
///
/// `APPDIR` counts only when it looks like a mount: a directory named `.mount_*`.
/// `/`, `/tmp`, the home folder and the like never do, so they cannot make praeco
/// copy itself again and again.
pub fn in_appimage_mount(exe: &std::path::Path, appdir: Option<&std::path::Path>) -> bool {
    let mount_like = |d: &std::path::Path| d.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".mount_"));
    exe.to_string_lossy().starts_with("/tmp/.mount_") || appdir.is_some_and(|d| mount_like(d) && exe.starts_with(d))
}

/// When praeco itself lives inside the app's AppImage mount and will outlive the
/// app (a restart or a detached run), copy it to a private temp directory and
/// continue from the copy. Replaces the process on success.
fn relocate_out_of_appimage(a: &Args) -> Result<()> {
    if !(a.restart || a.detach) {
        return Ok(());
    }
    // At most once: the copy carries this marker and never relocates again.
    if std::env::var_os("PRAECO_SELF_COPY_DIR").is_some() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    if !in_appimage_mount(&exe, a.env().appdir.as_deref()) {
        return Ok(());
    }
    let dir = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).prefix("praeco-self-").tempdir()?.keep();
    let copy = dir.join("praeco");
    let fail = |e: std::io::Error| {
        let _ = std::fs::remove_dir_all(&dir);
        Error::new(ErrorCode::Io, format!("praeco runs from inside the AppImage, which disappears when the app quits, and could not copy itself out: {e}"))
    };
    std::fs::copy(&exe, &copy).map_err(fail)?;
    {
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o700)).map_err(fail)?;
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&copy);
    cmd.args(&a.raw).env("PRAECO_SELF_COPY_DIR", &dir);
    crate::restart::clean_search_paths(&mut cmd);
    let err = cmd.exec();
    Err(fail(err))
}

/// The relocated copy deletes itself (its private directory) when it is done.
fn remove_self_copy() {
    let Some(dir) = std::env::var_os("PRAECO_SELF_COPY_DIR").map(PathBuf::from) else { return };
    let ours = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
    let named = dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("praeco-self-"));
    if named && ours.as_deref() == Some(dir.as_path()) {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `--detach`: create the progress file, start praeco again in a new session
/// writing its events there, report the path, and return at once.
fn cmd_detach(a: &Args) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // Relocate first (this replaces the process), so a default progress
    // directory is created exactly once, by the process that keeps it.
    relocate_out_of_appimage(a)?;
    let path = match &a.progress_file {
        Some(p) => std::path::absolute(p)?,
        None => tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).prefix("praeco-progress-").tempdir()?.keep().join("progress.jsonl"),
    };
    let parent = path.parent().ok_or_else(|| usage("--progress-file needs a file name"))?;
    let meta = std::fs::metadata(parent).map_err(|e| usage(format!("The folder for --progress-file ({}) is not usable: {e}", parent.display())))?;
    if !meta.is_dir() || meta.uid() != nix::unistd::geteuid().as_raw() || meta.permissions().mode() & 0o022 != 0 {
        return Err(usage(format!(
            "The folder for --progress-file ({}) must be yours and not writable by anyone else",
            parent.display()
        )));
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| usage(format!("Could not create the progress file {}: {e} (it must not exist yet)", path.display())))?;

    // The child gets the same arguments minus --detach and --progress-file.
    let mut child_args: Vec<OsString> = Vec::new();
    let mut it = a.raw.iter();
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--detach") => {}
            Some("--progress-file") => {
                it.next();
            }
            Some(s) if s.starts_with("--progress-file=") => {}
            Some("--restart") => {
                // Everything after --restart belongs to the app's own command line.
                child_args.push(arg.clone());
                child_args.extend(it.by_ref().cloned());
            }
            _ => child_args.push(arg.clone()),
        }
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(child_args).stdin(Stdio::null()).stdout(Stdio::from(file)).stderr(Stdio::null());
    crate::restart::make_session_leader(&mut cmd);
    // A child that fails early writes its own error event into the file.
    cmd.spawn().map_err(|e| Error::new(ErrorCode::Io, format!("Could not start the background update: {e}")))?;
    out_line(&Event::Detached { progress_file: path.display().to_string() }.to_line());
    Ok(())
}

/// The worker that runs inside the terminal window.
fn cmd_install_in_terminal(a: &Args) -> Result<()> {
    let events_path = a.need(&a.events_file, "--events-file")?;
    let opts = install_options(a, Mode::Terminal, true)?;
    ignore_hangup();
    let mut events = std::fs::OpenOptions::new().append(true).create(true).open(&events_path)?;
    let mut record = |e: &Event| {
        let _ = writeln!(events, "{}", e.to_line());
        let _ = events.flush();
    };
    record(&Event::Hello { pid: std::process::id() });
    println!("praeco: updating to version {}\n", opts.version.trim_start_matches(['v', 'V']));
    let mut last_printed = 0u64;
    let result = install::install(&opts, &mut |e| {
        record(e);
        if let Event::Downloading { bytes, total, .. } = e {
            let done = total.is_some_and(|t| *bytes >= t);
            if !(done || *bytes == 0 || bytes.saturating_sub(last_printed) >= 5_000_000) {
                return;
            }
            last_printed = *bytes;
        }
        if let Some(line) = e.human() {
            println!("{line}");
        }
    });
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            record(&Event::Error { code: e.code.as_str().to_string(), message: e.message.clone() });
            println!("\nSomething went wrong: {}\n\nPress Enter to close this window.", e.message);
            let _ = std::io::stdin().lock().read_line(&mut String::new());
            Err(e)
        }
    }
}

/// Run the CLI with the process's arguments; returns the exit code.
pub fn main_entry() -> i32 {
    run(std::env::args_os().skip(1).collect())
}

pub fn run(args: Vec<OsString>) -> i32 {
    let first = args.first().and_then(|a| a.to_str()).unwrap_or("").to_string();
    match first.as_str() {
        "" | "-h" | "--help" | "help" => {
            print!("{HELP}");
            return if first.is_empty() { 2 } else { 0 };
        }
        "-V" | "--version" | "version" => {
            println!("praeco {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }
    let wants_json = args.iter().any(|a| a == "--json");
    let result = parse(args).and_then(|a| match a.command.as_str() {
        "check" => cmd_check(&a),
        "install" => cmd_install(&a),
        "detect" => cmd_detect(&a),
        "init" => cmd_init(&a),
        "update-all" => cmd_update_all(&a).and_then(|all_ok| {
            if all_ok {
                Ok(())
            } else {
                Err(Error::new(ErrorCode::InstallFailed, "Some apps could not be updated"))
            }
        }),
        other => Err(usage(format!("Unknown command '{other}'. Try praeco --help."))),
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `install` already reported through its event stream; everything else reports here.
            let streamed = (first == "install" || first == "update-all") && e.code != ErrorCode::Usage;
            if !streamed {
                if wants_json || first == "detect" || (first == "install") {
                    out_line(&serde_json::json!({"ok": false, "error": {"code": e.code.as_str(), "message": e.message}}).to_string());
                } else {
                    eprintln!("praeco: {}", e.message);
                }
            }
            e.code.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args> {
        parse(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn parses_check() {
        let a = p(&["check", "--github", "you/myapp", "--current", "1.0.4", "--marker=## Setup", "--json"]).unwrap();
        assert_eq!(a.command, "check");
        assert_eq!(a.source().unwrap().owner, "you");
        assert_eq!(a.current.as_deref(), Some("1.0.4"));
        assert_eq!(a.marker.as_deref(), Some("## Setup"));
        assert!(a.json);
    }

    #[test]
    fn parses_forgejo_and_restart_tail() {
        let a = p(&[
            "install", "--forgejo", "https://code.example.com", "o/r", "--version", "1.0.5", "--exe", "/usr/bin/myapp", "--restart-pid", "42", "--restart", "--",
            "/usr/bin/myapp", "--flag", "--version",
        ])
        .unwrap();
        assert!(a.forgejo.is_some());
        assert_eq!(a.version.as_deref(), Some("1.0.5"));
        assert_eq!(a.restart_pid, Some(42));
        assert_eq!(a.restart_argv, ["/usr/bin/myapp", "--flag", "--version"].map(OsString::from));
    }

    #[test]
    fn detach_flags_parse() {
        let a = p(&["install", "--detach", "--progress-file", "/run/user/1/p.jsonl", "--restart", "--", "app", "--detach"]).unwrap();
        assert!(a.detach);
        assert_eq!(a.progress_file.as_deref(), Some(std::path::Path::new("/run/user/1/p.jsonl")));
        assert_eq!(a.restart_argv, ["app", "--detach"].map(OsString::from), "the app's own arguments are left alone");
    }

    #[test]
    fn appimage_mount_detection() {
        use std::path::Path;
        assert!(in_appimage_mount(Path::new("/tmp/.mount_MyApp1/usr/bin/praeco"), None));
        assert!(in_appimage_mount(Path::new("/run/x/.mount_ab/usr/bin/praeco"), Some(Path::new("/run/x/.mount_ab"))));
        for appdir in ["/", "/tmp", "/home/u", "/opt/mnt"] {
            assert!(!in_appimage_mount(Path::new("/tmp/praeco-self-x/praeco"), Some(Path::new(appdir))), "{appdir}");
        }
        assert!(!in_appimage_mount(Path::new("/usr/bin/praeco"), None));
        assert!(!in_appimage_mount(Path::new("/tmp/mount_x/praeco"), None));
    }

    #[test]
    fn usage_errors() {
        assert_eq!(p(&["check", "--bogus"]).err().unwrap().code, ErrorCode::Usage);
        assert_eq!(p(&["check", "--current"]).err().unwrap().code, ErrorCode::Usage);
        assert_eq!(p(&["install", "--restart", "x"]).err().unwrap().code, ErrorCode::Usage);
        assert!(p(&["check", "--current", "1"]).unwrap().source().is_err());
        let both = p(&["check", "--github", "a/b", "--forgejo", "https://x.y", "a/b"]).unwrap();
        assert!(both.source().is_err());
    }
}
