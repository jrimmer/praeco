//! Operating-system update hooks: a place where the OS's own "update
//! everything" command can also run `praeco update-all`.
//!
//! Today that is Omarchy's `post-update` hook directory. Other systems report
//! `NotApplicable`; their apps are still registered and `praeco update-all`
//! still works when run by hand.

use crate::detect::Env;
use crate::error::Result;
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

pub fn os_release_path() -> PathBuf {
    // Test seam, compiled into debug builds only.
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("PRAECO_OS_RELEASE").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    PathBuf::from("/etc/os-release")
}

/// Is `ID=omarchy` set in this os-release text?
pub fn is_omarchy(os_release: &str) -> bool {
    os_release.lines().any(|l| l.strip_prefix("ID=").is_some_and(|v| v.trim().trim_matches(['"', '\'']) == "omarchy"))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn script(praeco: &Path) -> String {
    format!(
        "#!/bin/bash\n{MARKER}: rewritten by `praeco init`. Delete this file to stop updating apps with the system.\n{} update-all || true\n",
        shell_quote(&praeco.to_string_lossy())
    )
}

/// Make `omarchy update` run `praeco update-all`, when this is an Omarchy system.
/// `os_release` is normally `os_release_path()`. `praeco` is the binary the hook will call; it must outlive the caller, so a
/// temporary location (an AppImage mount, /tmp) is refused.
pub fn ensure_omarchy_hook(env: &Env, praeco: &Path, os_release: &Path) -> Result<HookOutcome> {
    let Ok(os_release) = std::fs::read_to_string(os_release) else { return Ok(HookOutcome::NotApplicable) };
    if !is_omarchy(&os_release) {
        return Ok(HookOutcome::NotApplicable);
    }
    let skip = |reason: String| Ok(HookOutcome::Skipped { reason });
    let Some(home) = &env.home else { return skip("HOME is not set".into()) };
    let text = praeco.to_string_lossy();
    if text.contains('\n') || text.starts_with("/tmp/") || crate::cli::in_appimage_mount(praeco, env.appdir.as_deref()) {
        return skip(format!("praeco runs from {text}, which may not exist at the next update. Run `praeco init` with an installed praeco to add the hook."));
    }
    let dir = home.join(".config/omarchy/hooks/post-update.d");
    let path = dir.join("50-praeco");
    let wanted = script(praeco);
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == wanted => return Ok(HookOutcome::Current { path }),
        Ok(existing) if !existing.contains(MARKER) => return skip(format!("{} exists and is not managed by praeco, so it was left alone", path.display())),
        _ => {}
    }
    std::fs::create_dir_all(&dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::io::Write::write_all(&mut tmp, wanted.as_bytes())?;
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o755))?;
    tmp.persist(&path).map_err(|e| crate::error::Error::from(e.error))?;
    Ok(HookOutcome::Installed { path })
}
