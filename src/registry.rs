//! The registry of apps that `praeco update-all` keeps up to date.
//!
//! One JSON file per app: `~/.config/praeco/apps.d/<name>.json` for apps a user
//! registered, and `/usr/share/praeco/apps.d/<name>.json` for apps whose
//! package registers them. An app adds itself with `praeco init`.

use crate::assets::AssetPatterns;
use crate::detect::{Env, InstallKind};
use crate::error::{Error, ErrorCode, Result};
use crate::source::Source;
use crate::version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where packages put registrations (root-owned).
pub const SYSTEM_DIR: &str = "/usr/share/praeco/apps.d";
const MAX_FILE: u64 = 64 * 1024;
const MAX_VERSION_OUTPUT: u64 = 64 * 1024;
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "host", rename_all = "lowercase")]
pub enum SourceSpec {
    Github {
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_base: Option<String>,
    },
    Forgejo {
        base: String,
        repo: String,
    },
}

impl SourceSpec {
    pub fn to_source(&self) -> Result<Source> {
        match self {
            SourceSpec::Github { repo, api_base: None } => Source::github(repo),
            SourceSpec::Github { repo, api_base: Some(base) } => Source::github_with_api_base(repo, base),
            SourceSpec::Forgejo { base, repo } => Source::forgejo(base, repo),
        }
    }
}

/// One registered app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// File-name safe: letters, digits, `.`, `_`, `-`.
    pub name: String,
    pub source: SourceSpec,
    /// The app's executable. For an AppImage: the `.AppImage` file itself, since
    /// the mount inside it only exists while the app runs.
    pub exe: PathBuf,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub appimage: bool,
    /// Prints the installed version. Absolute program path first.
    pub version_cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prereleases: bool,
    /// `KIND=GLOB`, as for `--asset-pattern`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asset_patterns: Vec<String>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl App {
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::new(ErrorCode::Usage, m));
        if !valid_name(&self.name) {
            return bad(format!("'{}' is not a usable app name (letters, digits, '.', '_' and '-', at most 64, not starting with '.' or '-')", self.name));
        }
        if !self.exe.is_absolute() {
            return bad(format!("The executable path must be absolute, not {}", self.exe.display()));
        }
        match self.version_cmd.first() {
            None => return bad("The version command is empty".into()),
            Some(p) if !Path::new(p).is_absolute() => return bad(format!("The version command must start with an absolute path, not {p}")),
            Some(_) => {}
        }
        self.source.to_source()?;
        let mut patterns = AssetPatterns::default();
        let mut seen: Vec<InstallKind> = Vec::new();
        for spec in &self.asset_patterns {
            patterns.apply_override(spec, &mut seen).map_err(|m| Error::new(ErrorCode::Usage, m))?;
        }
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/praeco/apps.d`, else `~/.config/praeco/apps.d`.
pub fn user_dir(env: &Env) -> Option<PathBuf> {
    let base = env.config_home.clone().or_else(|| env.home.as_ref().map(|h| h.join(".config")))?;
    Some(base.join("praeco").join("apps.d"))
}

pub fn system_dir() -> PathBuf {
    // Test seam, compiled into debug builds only.
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("PRAECO_SYSTEM_APPS_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    PathBuf::from(SYSTEM_DIR)
}

#[derive(Debug, Default)]
pub struct Registry {
    pub apps: Vec<App>,
    /// Files or folders that were ignored, with the reason. Never fatal.
    pub problems: Vec<String>,
}

/// Registrations must belong to the folder's owner (root for the system folder)
/// and not be writable by anyone else: praeco runs their version command.
fn trusted(path: &Path, env: &Env, owner: u32, dir: bool) -> std::result::Result<(), String> {
    let m = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if m.is_dir() != dir {
        return Err(if dir { "not a folder".into() } else { "not a regular file".into() });
    }
    if env.check_ownership && m.uid() != owner {
        return Err(format!("not owned by user {owner}"));
    }
    if m.permissions().mode() & 0o022 != 0 {
        return Err("writable by others".into());
    }
    Ok(())
}

fn load_file(env: &Env, path: &Path, owner: u32) -> std::result::Result<App, String> {
    trusted(path, env, owner, false)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("too large".into());
    }
    let app: App = serde_json::from_slice(&bytes).map_err(|e| format!("not a valid registration: {e}"))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if app.name != stem {
        return Err(format!("the name '{}' does not match the file name", app.name));
    }
    app.validate().map_err(|e| e.message)?;
    Ok(app)
}

/// Every registered app. The user's registrations replace system ones of the same name.
pub fn load(env: &Env) -> Registry {
    let mut reg = Registry::default();
    let mut apps: BTreeMap<String, App> = BTreeMap::new();
    let mut dirs = vec![(system_dir(), 0u32)];
    if let Some(u) = user_dir(env) {
        dirs.push((u, env.euid));
    }
    for (dir, owner) in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        if let Err(why) = trusted(&dir, env, owner, true) {
            reg.problems.push(format!("{}: ignored, {why}", dir.display()));
            continue;
        }
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for path in files {
            match load_file(env, &path, owner) {
                Ok(app) => {
                    apps.insert(app.name.clone(), app);
                }
                Err(why) => reg.problems.push(format!("{}: ignored, {why}", path.display())),
            }
        }
    }
    reg.apps = apps.into_values().collect();
    reg
}

/// Write (or replace) the user's registration for `app`; returns the file.
pub fn save(env: &Env, app: &App) -> Result<PathBuf> {
    app.validate()?;
    let dir = user_dir(env).ok_or_else(|| Error::new(ErrorCode::Io, "Could not find your config folder (HOME is not set)"))?;
    std::fs::create_dir_all(&dir)?;
    let mut json = serde_json::to_vec_pretty(app).expect("serialises");
    json.push(b'\n');
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(&json)?;
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o644))?;
    let path = dir.join(format!("{}.json", app.name));
    tmp.persist(&path).map_err(|e| Error::from(e.error))?;
    Ok(path)
}

/// The first version number in a program's output: a token like `1.2.3`,
/// `v1.2` or `2.0.1-rc1`. A bare integer does not count (`Qt 6`).
pub fn parse_version_output(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || ",;()[]\"'".contains(c))
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .find(|t| {
            let digits = t.trim_start_matches(['v', 'V']);
            digits.contains('.') && digits.starts_with(|c: char| c.is_ascii_digit()) && version::parse(t).is_some()
        })
        .map(|t| t.trim_start_matches(['v', 'V']).to_string())
}

pub fn installed_version(app: &App) -> Result<String> {
    installed_version_within(app, VERSION_TIMEOUT)
}

/// Run the version command (stdin closed, no controlling terminal, output
/// capped, killed after `timeout`) and read the version from what it prints.
pub fn installed_version_within(app: &App, timeout: Duration) -> Result<String> {
    let prog = &app.version_cmd[0];
    let fail = |m: String| Error::new(ErrorCode::Io, format!("Could not read the installed version of {}: {m}", app.name));
    let mut cmd = Command::new(prog);
    cmd.args(&app.version_cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // A session of its own, so no terminal: during `omarchy update` the caller's
    // sudo is unlocked for its terminal, and this program (an AppImage or tarball
    // the user can write to) must not be able to use that.
    crate::restart::make_session_leader(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| fail(format!("could not run {prog}: {e}")))?;
    let reader = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(MAX_VERSION_OUTPUT).read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = reader(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = reader(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| fail(e.to_string()))? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // Not joining the readers: a grandchild may still hold the pipes open.
                return Err(fail(format!("{prog} did not answer within {} seconds", timeout.as_secs())));
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        return Err(fail(format!("{prog} exited with {status}")));
    }
    parse_version_output(&out).or_else(|| parse_version_output(&err)).ok_or_else(|| fail(format!("no version number in the output of {prog}")))
}
