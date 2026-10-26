//! The complete update: install (either mode), then restart if asked.

use crate::error::Result;
use crate::install::{self, InstallOptions, Mode};
use crate::progress::Event;
use crate::restart::{self, RestartSpec};
use crate::terminal;

/// Install, optionally restart, and finish with a `done` event. In
/// `Mode::Terminal` the install itself happens in a terminal window and its
/// progress is relayed; the restart is always done here, by the same code.
pub fn run(opts: &InstallOptions, restart: Option<&RestartSpec>, emit: &mut dyn FnMut(&Event)) -> Result<()> {
    let installed = if opts.mode == Mode::Terminal && !opts.in_terminal {
        terminal::install_via_terminal(opts, emit)?
    } else {
        install::install(opts, emit)?
    };
    if let Some(spec) = restart {
        restart::restart(spec, emit)?;
    }
    emit(&Event::Done { version: installed.version, restarted: restart.is_some() });
    Ok(())
}
