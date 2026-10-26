//! How was the running app installed? Decided from its executable path and
//! the environment, never from guesses about the distribution.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallKind {
    Pacman,
    Deb,
    Rpm,
    AppImage,
    Tarball,
}

impl InstallKind {
    pub const ALL: [InstallKind; 5] =
        [InstallKind::Pacman, InstallKind::Deb, InstallKind::Rpm, InstallKind::AppImage, InstallKind::Tarball];

    pub fn as_str(self) -> &'static str {
        match self {
            InstallKind::Pacman => "pacman",
            InstallKind::Deb => "deb",
            InstallKind::Rpm => "rpm",
            InstallKind::AppImage => "appimage",
            InstallKind::Tarball => "tarball",
        }
    }

    pub fn parse(s: &str) -> Option<InstallKind> {
        InstallKind::ALL.into_iter().find(|k| k.as_str() == s.to_ascii_lowercase())
    }

    /// Package kinds need administrator rights; AppImage and tarball do not.
    pub fn needs_root(self) -> bool {
        matches!(self, InstallKind::Pacman | InstallKind::Deb | InstallKind::Rpm)
    }
}

impl fmt::Display for InstallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything praeco reads from the machine, gathered in one place so tests
/// can substitute it (fake PATH with shim package managers, fake /proc, ...).
#[derive(Debug, Clone)]
pub struct Env {
    /// Search path for package managers, pkexec, sudo and terminals.
    pub path: OsString,
    /// `APPIMAGE`: the .AppImage file the app was started from.
    pub appimage: Option<PathBuf>,
    /// `APPDIR`: where an AppImage is mounted.
    pub appdir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub euid: u32,
    /// A graphical session is present (`WAYLAND_DISPLAY` or `DISPLAY` set).
    pub graphical: bool,
    /// Where to look for processes when probing for a polkit agent.
    pub proc_root: PathBuf,
    /// `TERMINAL`, the user's preferred terminal command.
    pub terminal: Option<String>,
    /// Machine architecture as Rust names it (`x86_64`, `aarch64`).
    pub arch: String,
}

fn nonempty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|s| !s.is_empty())
}

impl Env {
    pub fn from_process() -> Env {
        let var = |k: &str| nonempty(std::env::var_os(k));
        Env {
            path: var("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
            appimage: var("APPIMAGE").map(PathBuf::from),
            appdir: var("APPDIR").map(PathBuf::from),
            home: var("HOME").map(PathBuf::from),
            euid: nix::unistd::geteuid().as_raw(),
            graphical: var("WAYLAND_DISPLAY").is_some() || var("DISPLAY").is_some(),
            proc_root: PathBuf::from("/proc"),
            terminal: var("TERMINAL").map(|s| s.to_string_lossy().into_owned()),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// Resolve a program name against `self.path`.
    pub fn which(&self, prog: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path).map(|d| d.join(prog)).find(|p| {
            std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
        })
    }

    /// Run `prog args...` (found on `self.path`) and return the first line of
    /// stdout if it exited successfully.
    fn query(&self, prog: &str, args: &[&str], file: &Path) -> Option<String> {
        let exe = self.which(prog)?;
        let out = Command::new(exe)
            .args(args)
            .arg(file)
            .env("LC_ALL", "C")
            .env("PATH", &self.path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string())
    }
}

#[derive(Debug, Clone)]
pub struct Detection {
    /// `None` means unsupported; see `reason`.
    pub kind: Option<InstallKind>,
    /// The file an update replaces: the .AppImage, or the executable.
    pub target: PathBuf,
    /// What the package manager said owns the file, when it did.
    pub detail: Option<String>,
    /// Plain-language explanation, for both outcomes.
    pub reason: String,
}

/// Decide how the app at `exe` was installed. Order: AppImage, pacman, dpkg,
/// rpm, tarball under the user's home, otherwise unsupported.
pub fn detect(env: &Env, exe: &Path) -> Detection {
    if let Some(img) = &env.appimage {
        let inside = match &env.appdir {
            Some(dir) => exe.starts_with(dir),
            None => true,
        };
        if inside && img.is_file() {
            return Detection {
                kind: Some(InstallKind::AppImage),
                target: img.clone(),
                detail: None,
                reason: format!("Running from the AppImage {}", img.display()),
            };
        }
    }

    let canonical = std::fs::canonicalize(exe).ok().filter(|c| c != exe);
    let candidates: Vec<&Path> = std::iter::once(exe).chain(canonical.as_deref()).collect();
    let managers: [(InstallKind, &str, &[&str], &str); 3] = [
        (InstallKind::Pacman, "pacman", &["-Qo"], "pacman package"),
        (InstallKind::Deb, "dpkg", &["-S"], "deb package"),
        (InstallKind::Rpm, "rpm", &["-qf"], "rpm package"),
    ];
    for (kind, prog, args, what) in managers {
        for file in &candidates {
            if let Some(detail) = env.query(prog, args, file) {
                return Detection {
                    kind: Some(kind),
                    target: exe.to_path_buf(),
                    reason: format!("Installed as a {what}"),
                    detail: Some(detail).filter(|d| !d.is_empty()),
                };
            }
        }
    }

    if let (Some(home), Some(real)) = (&env.home, canonical.as_deref().or_else(|| exe.exists().then_some(exe))) {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.clone());
        if real.starts_with(&home) && real != home {
            return Detection {
                kind: Some(InstallKind::Tarball),
                target: real.to_path_buf(),
                detail: None,
                reason: format!("Unpacked from a tarball under {}", home.display()),
            };
        }
    }

    Detection {
        kind: None,
        target: exe.to_path_buf(),
        detail: None,
        reason: format!(
            "{} is not owned by a package, is not an AppImage and is not under your home folder, so it looks like a source build. Update it the way you built it.",
            exe.display()
        ),
    }
}

/// Is a polkit authentication agent running in this session? A heuristic: look
/// for a process whose name says so. `polkitd` is the daemon, not an agent.
pub fn polkit_agent_running(env: &Env) -> bool {
    const EMBEDDED: [&str; 3] = ["gnome-shell", "cinnamon", "cosmic-osd"];
    let Ok(rd) = std::fs::read_dir(&env.proc_root) else { return false };
    for ent in rd.flatten() {
        let name = ent.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(ent.path().join("comm")) else { continue };
        let comm = comm.trim().to_ascii_lowercase();
        if comm == "polkitd" {
            continue;
        }
        if comm.contains("polkit") || comm.contains("policykit") || EMBEDDED.contains(&comm.as_str()) {
            return true;
        }
    }
    false
}
