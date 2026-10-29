//! Operating-system update hooks: a place where the OS's own "update
//! everything" command can also run `praeco update-all`, or an app's own
//! update command when the app links praeco as a library.
//!
//! Today that is Omarchy's `post-update` hook directory. Other systems report
//! `NotApplicable`; their apps are still registered and `praeco update-all`
//! still works when run by hand.

use crate::detect::Env;
use crate::error::Result;
use crate::registry::App;
use serde::Serialize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

const MARKER: &str = "# praeco-managed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HookOutcome {
    /// The hook was written (or rewritten for a new praeco path).
    Installed { path: PathBuf },
    /// The hook was already in place.
    Current { path: PathBuf },
    /// This system has no hook directory praeco knows.
    NotApplicable,
    /// A hook was possible but not written; `reason` says why.
    Skipped { reason: String },
}

/// What `remove_omarchy_hook` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RemoveOutcome {
    Removed { path: PathBuf },
    /// There was no hook to remove.
    NotPresent,
    /// The hook was left in place; `reason` says why.
    Kept { reason: String },
}

pub fn os_release_path() -> PathBuf {
    // Test seam, compiled into debug builds only.
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("PRAECO_OS_RELEASE").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    PathBuf::from("/etc/os-release")
}

/// Is the system described by this os-release file Omarchy?
pub fn on_omarchy(os_release: &Path) -> bool {
    std::fs::read_to_string(os_release).is_ok_and(|t| is_omarchy(&t))
}

/// Is `ID=omarchy` set in this os-release text?
pub fn is_omarchy(os_release: &str) -> bool {
    os_release.lines().any(|l| l.strip_prefix("ID=").is_some_and(|v| v.trim().trim_matches(['"', '\'']) == "omarchy"))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shared hook: one file for every app that relies on the `praeco` program.
const SHARED_HOOK: &str = "50-praeco";

fn script(praeco: &Path) -> String {
    format!(
        "#!/bin/bash\n{MARKER}: rewritten by `praeco init`. Delete this file to stop updating apps with the system.\n{} update-all || true\n",
        shell_quote(&praeco.to_string_lossy())
    )
}

/// The hook of an app that updates itself: it runs the app's own command. A command
/// that needs no administrator rights runs in a session of its own, with no terminal
/// and stdin closed, so it cannot reuse the update's unlocked `sudo`. A program that
/// is gone (the app was uninstalled) is skipped quietly.
fn app_script(app: &App, cmd: &[String]) -> String {
    let words = cmd.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ");
    let run = if app.needs_terminal {
        "\"${cmd[@]}\" || true".to_string()
    } else {
        "if command -v setsid >/dev/null 2>&1; then setsid -w \"${cmd[@]}\" </dev/null || true; else \"${cmd[@]}\" </dev/null || true; fi".to_string()
    };
    format!(
        "#!/bin/bash\n{MARKER}: rewritten by `praeco init`. Delete this file to stop updating {} with the system.\ncmd=({words})\n[ -x \"${{cmd[0]}}\" ] || exit 0\n{run}\n",
        app.name
    )
}

fn hooks_dir(home: &Path) -> PathBuf {
    home.join(".config/omarchy/hooks/post-update.d")
}

fn app_hook_name(name: &str) -> String {
    format!("50-praeco-{name}")
}

/// Install through Omarchy's own `omarchy-hook-install` when it is on the user's
/// PATH, so a change to where Omarchy keeps hooks is followed. The tool reports the
/// path it used; the result counts only if the file there is what we meant to write.
fn install_with_omarchy_tool(env: &Env, file_name: &str, wanted: &str) -> Option<PathBuf> {
    let tool = env.which("omarchy-hook-install")?;
    let dir = tempfile::tempdir().ok()?;
    let source = dir.path().join(file_name);
    std::fs::write(&source, wanted).ok()?;
    let out = std::process::Command::new(tool).arg("post-update").arg(&source).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let path = PathBuf::from(stdout.lines().rev().find_map(|l| l.strip_prefix("Installed post-update hook: "))?.trim());
    (std::fs::read_to_string(&path).ok()? == wanted).then_some(path)
}

/// Take a hook back out, but only a file praeco wrote.
fn remove_hook_file(env: &Env, file_name: &str) -> Result<RemoveOutcome> {
    let Some(home) = &env.home else { return Ok(RemoveOutcome::NotPresent) };
    let path = hooks_dir(home).join(file_name);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoveOutcome::NotPresent),
        Err(e) => Err(e.into()),
        Ok(text) if !text.contains(MARKER) => Ok(RemoveOutcome::Kept { reason: format!("{} was not written by praeco, so it was left alone", path.display()) }),
        Ok(_) => {
            std::fs::remove_file(&path)?;
            Ok(RemoveOutcome::Removed { path })
        }
    }
}

/// Remove the shared hook (the one that runs `praeco update-all`).
pub fn remove_omarchy_hook(env: &Env) -> Result<RemoveOutcome> {
    remove_hook_file(env, SHARED_HOOK)
}

/// Remove the hook of one app that updates itself.
pub fn remove_app_hook(env: &Env, name: &str) -> Result<RemoveOutcome> {
    if !crate::registry::valid_name(name) {
        return Ok(RemoveOutcome::NotPresent);
    }
    remove_hook_file(env, &app_hook_name(name))
}

/// Write `wanted` as the hook `file_name`, when this is an Omarchy system. `program`
/// is what the hook runs: it must outlive the caller, so a temporary location (an
/// AppImage mount, /tmp) is refused.
fn ensure_hook(env: &Env, os_release: &Path, file_name: &str, wanted: String, program: &Path) -> Result<HookOutcome> {
    let Ok(os_release) = std::fs::read_to_string(os_release) else { return Ok(HookOutcome::NotApplicable) };
    if !is_omarchy(&os_release) {
        return Ok(HookOutcome::NotApplicable);
    }
    let skip = |reason: String| Ok(HookOutcome::Skipped { reason });
    let Some(home) = &env.home else { return skip("HOME is not set".into()) };
    let text = program.to_string_lossy();
    if text.contains('\n') || text.starts_with("/tmp/") || crate::cli::in_appimage_mount(program, env.appdir.as_deref()) {
        return skip(format!("{text} may not exist at the next update. Run `praeco init` from an installed copy to add the hook."));
    }
    let dir = hooks_dir(home);
    let path = dir.join(file_name);
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == wanted => return Ok(HookOutcome::Current { path }),
        Ok(existing) if !existing.contains(MARKER) => return skip(format!("{} exists and is not managed by praeco, so it was left alone", path.display())),
        _ => {}
    }
    if let Some(path) = install_with_omarchy_tool(env, file_name, &wanted) {
        return Ok(HookOutcome::Installed { path });
    }
    std::fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::io::Write::write_all(&mut tmp, wanted.as_bytes())?;
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o755))?;
    tmp.persist(&path).map_err(|e| crate::error::Error::from(e.error))?;
    Ok(HookOutcome::Installed { path })
}

/// Make `omarchy update` run `praeco update-all`, when this is an Omarchy system.
/// `os_release` is normally `os_release_path()`; `praeco` is the program the hook calls.
pub fn ensure_omarchy_hook(env: &Env, praeco: &Path, os_release: &Path) -> Result<HookOutcome> {
    ensure_hook(env, os_release, SHARED_HOOK, script(praeco), praeco)
}

/// Make `omarchy update` run an app's own update command (`app.update_cmd`), when
/// this is an Omarchy system. One hook file per app.
pub fn ensure_app_hook(env: &Env, app: &App, os_release: &Path) -> Result<HookOutcome> {
    let Some(cmd) = &app.update_cmd else {
        return Ok(HookOutcome::Skipped { reason: format!("{} has no update command", app.name) });
    };
    ensure_hook(env, os_release, &app_hook_name(&app.name), app_script(app, cmd), Path::new(&cmd[0]))
}
