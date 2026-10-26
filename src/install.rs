//! The install pipeline: resolve the release, download, verify, install.
//! Used identically by dialog mode, by the in-terminal worker, and by library
//! callers; only the way administrator rights are obtained differs.

use crate::assets::{self, AssetPatterns};
use crate::check::fetch_releases;
use crate::detect::{self, Detection, Env, InstallKind};
use crate::error::{Error, ErrorCode, Result};
use crate::files;
use crate::http::Http;
use crate::progress::Event;
use crate::release::Release;
use crate::source::Source;
use crate::verify;
use crate::version;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use serde::Serialize;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask for administrator rights through polkit (`pkexec`), in a dialog.
    Dialog,
    /// Use `sudo`, inside a terminal.
    Terminal,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub source: Source,
    /// Version to install, with or without a leading `v`.
    pub version: String,
    /// The running app's executable.
    pub exe: PathBuf,
    pub mode: Mode,
    /// True inside the terminal worker: the package manager inherits the
    /// terminal so `sudo` can prompt, and `Mode::Terminal` is taken as given.
    pub in_terminal: bool,
    pub patterns: AssetPatterns,
    pub include_prereleases: bool,
    pub token: Option<String>,
    pub env: Env,
    /// Replaces `pkexec`/`sudo`: this argv is put in front of the package
    /// manager command. For tests and unusual setups (`doas`, a wrapper).
    pub escalation: Option<Vec<OsString>>,
}

impl InstallOptions {
    pub fn new(source: Source, version: impl Into<String>, exe: impl Into<PathBuf>) -> InstallOptions {
        InstallOptions {
            source,
            version: version.into(),
            exe: exe.into(),
            mode: Mode::Dialog,
            in_terminal: false,
            patterns: AssetPatterns::default(),
            include_prereleases: false,
            token: None,
            env: Env::from_process(),
            escalation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub kind: InstallKind,
}

/// Find the release for `want` in a release list.
pub fn find_release<'a>(releases: &'a [Release], want: &str) -> Result<&'a Release> {
    let v = version::parse(want).ok_or_else(|| Error::new(ErrorCode::InvalidVersion, format!("'{want}' is not a version number like 1.2.3")))?;
    releases
        .iter()
        .find(|r| version::same(&r.version, &v))
        .ok_or_else(|| Error::new(ErrorCode::NoRelease, format!("There is no release {want} to install")))
}

/// The exact argv (package manager and its arguments, before any escalation
/// prefix) used to install the verified file `file`.
///
/// - pacman: `pacman -U --noconfirm <file>`
/// - deb:    `apt-get install -y <file>` (resolves dependencies); `dpkg -i <file>`
///   when apt-get is not installed
/// - rpm:    `dnf install -y <file>` (resolves dependencies); `rpm -U <file>`
///   when dnf is not installed
///
/// The program is resolved to an absolute path on `env.path`.
pub fn package_command(kind: InstallKind, file: &Path, env: &Env) -> Result<Vec<OsString>> {
    let (candidates, args): (&[(&str, &[&str])], &str) = match kind {
        InstallKind::Pacman => (&[("pacman", &["-U", "--noconfirm"])], "pacman"),
        InstallKind::Deb => (&[("apt-get", &["install", "-y"]), ("dpkg", &["-i"])], "apt-get or dpkg"),
        InstallKind::Rpm => (&[("dnf", &["install", "-y"]), ("rpm", &["-U"])], "dnf or rpm"),
        _ => return Err(Error::new(ErrorCode::InstallFailed, format!("{kind} is not a package"))),
    };
    for (prog, a) in candidates {
        if let Some(path) = env.which(prog) {
            let mut v: Vec<OsString> = vec![path.into_os_string()];
            v.extend(a.iter().map(OsString::from));
            v.push(file.as_os_str().to_os_string());
            return Ok(v);
        }
    }
    Err(Error::new(ErrorCode::InstallFailed, format!("Could not find {args} to install the package with")))
}

/// How administrator rights are obtained. An empty argv means "already
/// allowed, run directly" (running as root).
///
/// Dialog mode needs, all of: `pkexec` on PATH, a graphical session
/// (`WAYLAND_DISPLAY` or `DISPLAY`), and a running polkit authentication
/// agent (a process named like `*polkit*`/`*policykit*`, or a shell that
/// embeds one). Otherwise the result is `needs_terminal`.
pub fn resolve_escalation(opts: &InstallOptions) -> Result<Vec<OsString>> {
    if let Some(e) = &opts.escalation {
        return Ok(e.clone());
    }
    if opts.env.euid == 0 {
        return Ok(Vec::new());
    }
    let nt = |m: &str| Err(Error::new(ErrorCode::NeedsTerminal, m));
    match opts.mode {
        Mode::Dialog => {
            let Some(pkexec) = opts.env.which("pkexec") else {
                return nt("This computer has no graphical permission prompt (pkexec is not installed). Continue in a terminal.");
            };
            if !opts.env.graphical {
                return nt("There is no graphical session to show a permission prompt in. Continue in a terminal.");
            }
            if !detect::polkit_agent_running(&opts.env) {
                return nt("No polkit authentication agent is running, so a permission prompt cannot appear. Continue in a terminal.");
            }
            Ok(vec![pkexec.into_os_string()])
        }
        Mode::Terminal => match opts.env.which("sudo") {
            Some(s) => Ok(vec![s.into_os_string()]),
            None => Err(Error::new(ErrorCode::InstallFailed, "sudo is not installed, so praeco cannot get administrator rights")),
        },
    }
}

fn tail_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(4)..].join(" / ")
}

fn run_elevated(escalation: &[OsString], cmd: &[OsString], interactive: bool, env: &Env) -> Result<()> {
    let mut argv: Vec<&OsString> = escalation.iter().collect();
    argv.extend(cmd.iter());
    let via_pkexec = escalation.first().and_then(|p| Path::new(p).file_name()).is_some_and(|n| n == "pkexec");
    let mut c = Command::new(argv[0]);
    c.args(&argv[1..]).env("PATH", &env.path).env("LC_ALL", "C");
    let spawn_err = |e: std::io::Error| Error::new(ErrorCode::InstallFailed, format!("Could not start {}: {e}", argv[0].to_string_lossy()));
    let (status, detail) = if interactive {
        (c.status().map_err(spawn_err)?, String::new())
    } else {
        let out = c.stdin(Stdio::null()).output().map_err(spawn_err)?;
        let mut d = tail_lines(&out.stderr);
        if d.is_empty() {
            d = tail_lines(&out.stdout);
        }
        (out.status, d)
    };
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(126) if via_pkexec => Err(Error::new(ErrorCode::AuthCancelled, "The permission prompt was dismissed or the password was refused. Nothing was installed.")),
        Some(127) if via_pkexec => Err(Error::new(ErrorCode::NeedsTerminal, "The permission prompt could not be shown. Continue in a terminal.")),
        code => {
            let code = code.map(|c| format!("exit status {c}")).unwrap_or_else(|| "killed by a signal".into());
            let more = if detail.is_empty() { String::new() } else { format!(": {detail}") };
            Err(Error::new(ErrorCode::InstallFailed, format!("The package manager reported an error ({code}){more}")))
        }
    }
}

fn plain_kind(kind: InstallKind) -> &'static str {
    match kind {
        InstallKind::Pacman => "pacman package",
        InstallKind::Deb => "deb package",
        InstallKind::Rpm => "rpm package",
        InstallKind::AppImage => "AppImage",
        InstallKind::Tarball => "tarball install",
    }
}

fn private_dir() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new().prefix("praeco-").tempdir()?)
}

/// Run the whole pipeline. Emits `resolving`, `downloading`, `verifying`,
/// `installing`, `installed`; errors are returned for the caller to report
/// (the CLI turns them into `error` / `needs_terminal` events).
pub fn install(opts: &InstallOptions, emit: &mut dyn FnMut(&Event)) -> Result<Installed> {
    install_with(&Http::new(), opts, emit)
}

pub fn install_with(http: &Http, opts: &InstallOptions, emit: &mut dyn FnMut(&Event)) -> Result<Installed> {
    emit(&Event::Resolving { version: opts.version.trim_start_matches(['v', 'V']).to_string() });

    let det: Detection = detect::detect(&opts.env, &opts.exe);
    let Some(kind) = det.kind else {
        return Err(Error::new(ErrorCode::Unsupported, det.reason));
    };

    let releases = fetch_releases(http, &opts.source, opts.token.as_deref(), None, true)?;
    let release = find_release(&releases, &opts.version)?;
    if release.prerelease && !opts.include_prereleases {
        return Err(Error::new(ErrorCode::NoRelease, format!("{} is a prerelease; installing it needs --prereleases", release.tag)));
    }
    let asset = assets::select(release, kind, &opts.patterns, &opts.env.arch).ok_or_else(|| {
        Error::new(
            ErrorCode::NoAsset,
            format!("Release {} has no {} file for {}", release.tag, plain_kind(kind), opts.env.arch),
        )
    })?;

    // Decide how to get administrator rights before spending a download on it.
    let escalation = if kind.needs_root() { resolve_escalation(opts)? } else { Vec::new() };

    let dir = private_dir()?;
    let file = verify::download(http, &opts.source, asset, dir.path(), &mut |bytes, total| {
        emit(&Event::Downloading { asset: asset.name.clone(), bytes, total });
    })?;
    emit(&Event::Verifying { asset: asset.name.clone() });
    let digest = verify::verify(http, &opts.source, release, asset, &file)?;

    let version = release.version.to_string();
    match kind {
        InstallKind::Pacman | InstallKind::Deb | InstallKind::Rpm => {
            let cmd = package_command(kind, &file, &opts.env)?;
            // Narrow the window between checking the file and handing it to a root process.
            if verify::sha256_file(&file)? != digest {
                return Err(Error::new(ErrorCode::ChecksumMismatch, "The downloaded file changed after it was checked. It was not installed."));
            }
            let who = if escalation.is_empty() { "" } else { " Administrator permission is needed." };
            emit(&Event::Installing { kind, message: format!("Installing the {}.{who}", plain_kind(kind)) });
            run_elevated(&escalation, &cmd, opts.in_terminal, &opts.env)?;
        }
        InstallKind::AppImage => {
            emit(&Event::Installing { kind, message: format!("Replacing {} with the new version.", det.target.display()) });
            files::replace_file_atomically(&file, &det.target)?;
        }
        InstallKind::Tarball => {
            emit(&Event::Installing { kind, message: format!("Unpacking the new version over {}.", det.target.display()) });
            files::install_tarball(&file, &det.target)?;
        }
    }
    let done = Installed { version, kind };
    emit(&Event::Installed { version: done.version.clone(), kind });
    Ok(done)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssetInfo {
    pub name: String,
    pub size: u64,
}

/// What `praeco check --exe` and `praeco detect` report about installing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InstallInfo {
    /// `pacman`, `deb`, `rpm`, `appimage`, `tarball` or `unsupported`.
    pub kind: String,
    /// Can praeco update this install (and, when a release was given, is there a file for it)?
    pub supported: bool,
    pub reason: String,
    pub detail: Option<String>,
    /// The file an update replaces.
    pub target: String,
    /// The asset that would be downloaded from the release.
    pub asset: Option<AssetInfo>,
}

pub fn describe(env: &Env, exe: &Path, release: Option<&Release>, patterns: &AssetPatterns) -> InstallInfo {
    let det = detect::detect(env, exe);
    let asset = det.kind.and_then(|k| release.and_then(|r| assets::select(r, k, patterns, &env.arch)));
    let missing = det.kind.is_some() && release.is_some() && asset.is_none();
    InstallInfo {
        kind: det.kind.map(|k| k.as_str().to_string()).unwrap_or_else(|| "unsupported".to_string()),
        supported: det.kind.is_some() && !missing,
        reason: if missing { format!("{}, but the release has no matching file for {}", det.reason, env.arch) } else { det.reason },
        detail: det.detail,
        target: det.target.display().to_string(),
        asset: asset.map(|a| AssetInfo { name: a.name.clone(), size: a.size }),
    }
}
