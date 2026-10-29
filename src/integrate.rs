//! Register an app and wire it into the system's update command: what
//! `praeco init` and `praeco init --remove` do, for apps that link praeco as a
//! library and have no `praeco` program of their own to point a hook at.
//!
//! Such an app registers with an `update_cmd`, the command that makes it update
//! itself (headless), built on `updateall::update_hosted`. The system update runs
//! that command directly.

use crate::detect::Env;
use crate::error::Result;
use crate::hook::{self, HookOutcome, RemoveOutcome};
use crate::registry::{self, App, Removal};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    /// Also add (or remove) the system's update hook where there is one.
    Auto,
    /// Only the registration.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub registry_file: PathBuf,
    pub hook: HookOutcome,
}

/// Save the registration and add the hook. An app with an `update_cmd` gets a hook
/// of its own that runs that command. Any other app shares the hook that runs
/// `praeco update-all`, which needs `praeco`, the full path of a praeco program that
/// will still be there at the next update.
pub fn register(env: &Env, app: &App, hook_mode: HookMode, praeco: Option<&Path>) -> Result<Registered> {
    let registry_file = registry::save(env, app)?;
    let os_release = hook::os_release_path();
    let hook = match (hook_mode, praeco) {
        (HookMode::None, _) => HookOutcome::NotApplicable,
        (HookMode::Auto, _) if app.hosted() => hook::ensure_app_hook(env, app, &os_release)?,
        (HookMode::Auto, Some(praeco)) => hook::ensure_omarchy_hook(env, praeco, &os_release)?,
        (HookMode::Auto, None) if hook::on_omarchy(&os_release) => HookOutcome::Skipped {
            reason: "there is no praeco program for the hook to call; an app that only links praeco as a library registers with an update command instead".into(),
        },
        (HookMode::Auto, None) => HookOutcome::NotApplicable,
    };
    Ok(Registered { registry_file, hook })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unregistered {
    pub removal: Removal,
    pub hook: RemoveOutcome,
}

/// Delete the registration and the app's own hook. The shared hook goes too once
/// no app that relies on it is left.
pub fn unregister(env: &Env, name: &str, hook_mode: HookMode) -> Result<Unregistered> {
    let removal = registry::remove(env, name)?;
    let hook = match hook_mode {
        HookMode::None => RemoveOutcome::Kept { reason: "--hook none".into() },
        HookMode::Auto => match hook::remove_app_hook(env, name)? {
            RemoveOutcome::NotPresent => {
                let left = registry::load(env).apps.iter().filter(|a| !a.hosted()).count();
                if left > 0 {
                    RemoveOutcome::Kept { reason: format!("{left} other app{} still registered", if left == 1 { " is" } else { "s are" }) }
                } else {
                    hook::remove_omarchy_hook(env)?
                }
            }
            removed_or_kept => removed_or_kept,
        },
    };
    Ok(Unregistered { removal, hook })
}
