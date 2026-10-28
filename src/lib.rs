//! praeco: update logic for Linux desktop apps, shared as a library and a CLI.
//!
//! The library does everything except the UI: check a GitHub or Forgejo
//! release source, work out how the app was installed, download and verify the
//! right asset, install it, and restart the app. See the README for the CLI
//! and its JSON formats.

pub mod assets;
pub mod check;
pub mod cli;
pub mod detect;
pub mod error;
pub mod files;
pub mod hook;
pub mod http;
pub mod install;
pub mod notes;
pub mod progress;
pub mod registry;
pub mod release;
pub mod restart;
pub mod source;
pub mod terminal;
pub mod update;
pub mod updateall;
pub mod verify;
pub mod version;

pub use assets::AssetPatterns;
pub use check::{check, CheckOptions, CheckResult, ReleaseInfo};
pub use detect::{detect, Detection, Env, InstallKind};
pub use error::{Error, ErrorCode, Result};
pub use install::{install, InstallOptions, Installed, Mode};
pub use progress::Event;
pub use restart::RestartSpec;
pub use source::Source;
pub use update::run as update;
