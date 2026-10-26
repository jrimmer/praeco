//! Progress events. The CLI prints each as one JSON line; the library hands
//! them to a callback.

use crate::detect::InstallKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Looking up the release and deciding which file to fetch.
    Resolving { version: String },
    Downloading { asset: String, bytes: u64, total: Option<u64> },
    Verifying { asset: String },
    Installing { kind: InstallKind, message: String },
    /// The new version is on disk. A restart, if requested, follows.
    Installed { version: String, kind: InstallKind },
    Restarting { message: String },
    /// Last event of a successful run.
    Done { version: String, restarted: bool },
    /// Dialog mode cannot work here; run again with `--mode terminal`.
    NeedsTerminal { message: String },
    TerminalOpened { terminal: String },
    /// Last event of a failed run.
    Error { code: String, message: String },
    /// Internal: first line the in-terminal worker writes to its events file.
    Hello { pid: u32 },
}

impl Event {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("events always serialise")
    }

    /// A plain-language line for the terminal worker; `None` for events that
    /// are not worth a line.
    pub fn human(&self) -> Option<String> {
        Some(match self {
            Event::Resolving { version } => format!("Looking for version {version}..."),
            Event::Downloading { asset, bytes, total } => match total {
                Some(t) => format!("Downloading {asset}: {} of {}", mb(*bytes), mb(*t)),
                None => format!("Downloading {asset}: {}", mb(*bytes)),
            },
            Event::Verifying { .. } => "Checking that the download is genuine (SHA-256)...".to_string(),
            Event::Installing { message, .. } => message.clone(),
            Event::Installed { version, .. } => format!("Version {version} is installed."),
            Event::Restarting { message } => message.clone(),
            Event::Done { .. } => "All done.".to_string(),
            Event::NeedsTerminal { message } => message.clone(),
            Event::TerminalOpened { .. } | Event::Hello { .. } => return None,
            Event::Error { message, .. } => format!("Something went wrong: {message}"),
        })
    }
}

fn mb(n: u64) -> String {
    format!("{:.1} MB", n as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        let e = Event::Downloading { asset: "a.deb".into(), bytes: 5, total: None };
        assert_eq!(e.to_line(), r#"{"event":"downloading","asset":"a.deb","bytes":5,"total":null}"#);
        let e = Event::Installed { version: "1.0.5".into(), kind: InstallKind::AppImage };
        assert_eq!(e.to_line(), r#"{"event":"installed","version":"1.0.5","kind":"appimage"}"#);
        let back: Event = serde_json::from_str(&e.to_line()).unwrap();
        assert_eq!(back, e);
    }
}
