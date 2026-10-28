//! `praeco update-all`: bring every registered app up to date, silently.
//!
//! One app failing never stops the others. Administrator rights come from the
//! terminal the command runs in when there is one (so `omarchy update` needs no
//! second prompt), otherwise from the desktop's permission prompt, and from a
//! terminal window when that cannot appear: an app is always updated if it can be.

use crate::assets::AssetPatterns;
use crate::check::{self, CheckOptions};
use crate::detect::{Env, InstallKind};
use crate::error::{Error, ErrorCode, Result};
use crate::http::Http;
use crate::install::{InstallOptions, Mode};
use crate::progress::Event;
use crate::registry::{self, App, Registry};
use crate::update;
use serde::Serialize;
use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// A terminal if stdin is one, else the permission prompt, then a terminal window.
    Auto,
    /// `sudo` in the current terminal.
    Inline,
    Dialog,
    Terminal,
}

impl UpdateMode {
    pub fn parse(s: &str) -> Option<UpdateMode> {
        Some(match s {
            "auto" => UpdateMode::Auto,
            "inline" => UpdateMode::Inline,
            "dialog" => UpdateMode::Dialog,
            "terminal" => UpdateMode::Terminal,
            _ => return None,
        })
    }
}

/// The ways to get administrator rights, in the order to try them, as
/// `(mode, already_in_a_terminal)`.
pub fn attempts(mode: UpdateMode, stdin_is_tty: bool) -> Vec<(Mode, bool)> {
    match mode {
        UpdateMode::Inline => vec![(Mode::Terminal, true)],
        UpdateMode::Dialog => vec![(Mode::Dialog, false)],
        UpdateMode::Terminal => vec![(Mode::Terminal, false)],
        UpdateMode::Auto if stdin_is_tty => vec![(Mode::Terminal, true)],
        UpdateMode::Auto => vec![(Mode::Dialog, false), (Mode::Terminal, false)],
    }
}

/// Try each attempt in turn; only `needs_terminal` moves on to the next one.
pub fn first_that_works<T>(attempts: &[(Mode, bool)], mut f: impl FnMut(Mode, bool) -> Result<T>) -> Result<T> {
    for (i, &(mode, in_terminal)) in attempts.iter().enumerate() {
        match f(mode, in_terminal) {
            Err(e) if e.code == ErrorCode::NeedsTerminal && i + 1 < attempts.len() => continue,
            other => return other,
        }
    }
    Err(Error::new(ErrorCode::InstallFailed, "There is no way to get administrator rights"))
}

#[derive(Debug, Clone)]
pub struct Options {
    pub mode: UpdateMode,
    pub stdin_is_tty: bool,
    pub env: Env,
    pub token: Option<String>,
    /// Replaces `pkexec`/`sudo` (tests and unusual setups).
    pub escalation: Option<Vec<OsString>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    UpToDate,
    Updated,
    /// The registered executable no longer exists (the app was uninstalled).
    NotInstalled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppResult {
    pub name: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

/// Update every app in `registry`. `log` receives plain progress lines.
pub fn run(registry: &Registry, opts: &Options, log: &mut dyn FnMut(String)) -> Vec<AppResult> {
    registry.apps.iter().map(|app| update_one(app, opts, log)).collect()
}

fn update_one(app: &App, opts: &Options, log: &mut dyn FnMut(String)) -> AppResult {
    let mut r = AppResult { name: app.name.clone(), status: Status::Failed, from: None, to: None, error: None };
    if !app.exe.exists() {
        r.status = Status::NotInstalled;
        log(format!("{}: not installed any more, skipped", app.name));
        return r;
    }
    if let Err(e) = try_update(app, opts, log, &mut r) {
        log(format!("{}: could not update: {}", app.name, e.message));
        r.status = Status::Failed;
        r.error = Some(ErrorInfo { code: e.code.as_str().to_string(), message: e.message });
    }
    r
}

fn try_update(app: &App, opts: &Options, log: &mut dyn FnMut(String), r: &mut AppResult) -> Result<()> {
    let current = registry::installed_version(app)?;
    r.from = Some(current.clone());
    let source = app.source.to_source()?;
    let mut copts = CheckOptions::new(source.clone(), current.clone());
    copts.include_prereleases = app.prereleases;
    copts.token = opts.token.clone();
    let (result, _) = check::check_full(&Http::for_source(&source), &copts)?;
    let latest = match (result.update_available, result.latest) {
        (true, Some(latest)) => latest,
        _ => {
            r.status = Status::UpToDate;
            r.to = Some(current.clone());
            log(format!("{}: up to date ({current})", app.name));
            return Ok(());
        }
    };
    log(format!("{}: updating {current} -> {latest}", app.name));

    let mut env = opts.env.clone();
    if app.appimage {
        // The .AppImage file stands for both the AppImage and its "mount": the
        // real mount only exists while the app runs.
        env.appimage = Some(app.exe.clone());
        env.appdir = Some(app.exe.clone());
    }
    let mut patterns = AssetPatterns::default();
    let mut seen: Vec<InstallKind> = Vec::new();
    for spec in &app.asset_patterns {
        patterns.apply_override(spec, &mut seen).map_err(|m| Error::new(ErrorCode::Usage, m))?;
    }

    let mut installed: Option<String> = None;
    first_that_works(&attempts(opts.mode, opts.stdin_is_tty), |mode, in_terminal| {
        let mut io = InstallOptions::new(source.clone(), latest.clone(), app.exe.clone());
        io.mode = mode;
        io.in_terminal = in_terminal;
        io.patterns = patterns.clone();
        io.include_prereleases = app.prereleases;
        io.token = opts.token.clone();
        io.env = env.clone();
        io.escalation = opts.escalation.clone();
        update::run(&io, None, &mut |e| {
            if let Event::Installed { version, .. } = e {
                installed = Some(version.clone());
            }
            if !matches!(e, Event::Downloading { .. }) {
                if let Some(line) = e.human() {
                    log(format!("  {line}"));
                }
            }
        })
    })?;
    r.status = Status::Updated;
    r.to = Some(installed.unwrap_or(latest));
    Ok(())
}
