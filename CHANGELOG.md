# Changelog

## 0.3.0 (2026-10-28)

The 0.2.0 GitHub release was never published to crates.io; this is the next crates.io
release after 0.1.1 and includes everything in 0.2.0.

- Apps that link praeco as a library, with no `praeco` program of their own, can now
  update with the system: they register with an update command (`praeco init
  --update-cmd -- PROGRAM ARGS`, or `integrate::register`), and the system update runs
  that command through a hook of its own (`50-praeco-NAME` on Omarchy). The app's
  settings decide whether it updates; the command runs without a terminal unless
  `--needs-terminal` says it may need `sudo`. `updateall::update_hosted` is the call
  for the app's headless update.
- New library API: `integrate::{register, unregister}`, `hook::{ensure_app_hook,
  remove_app_hook}`, `updateall::Options::from_process`, `SourceSpec::from(&Source)`.
  `update-all` reports such apps as `hosted` and leaves them to their own hook.
- Registrations may omit `version_cmd` when they have an `update_cmd`; `registry::App`
  gains `update_cmd` and `needs_terminal`, so code that builds one by hand must set them.

## 0.2.0 (2026-10-28)

- New `praeco init`: an app registers itself once (name, source, executable) and
  praeco adds itself to the system's update command where there is one.
- New `praeco update-all`: updates every registered app silently, one failure never
  stopping the others, with sudo from the current terminal when there is one.
- `praeco init --remove --name NAME` takes an app back out, and the system hook with
  the last one. Files praeco did not write are never deleted.
- On Omarchy, `praeco init` adds a `post-update` hook, so `omarchy update` also
  updates the registered apps with the password it already asked for. The hook is
  installed with `omarchy-hook-install` when it is available.
- Dialog mode now recognises the polkit agent that Omarchy and other Quickshell
  desktops run inside the `quickshell` process, so installs there show the
  desktop's permission prompt instead of falling back to a terminal.
- Library: `detect::Env` has a new public field, `config_home` (`XDG_CONFIG_HOME`).
  Code that builds an `Env` by hand must set it; `Env::from_process()` does.

## 0.1.1 (2026-10-26)

Packaging and documentation only; praeco behaves as in 0.1.0.

- Each GitHub release now carries static binaries for x86_64 and aarch64 Linux,
  each with a `.sha256` file, and a `SHA256SUMS` listing both.
- Published on crates.io: Rust apps can `cargo add praeco`.
- The README has a table of contents, says how an app ships and updates its own
  praeco, and its examples call praeco by full path and use a placeholder app.

## 0.1.0 (2026-10-26)

First release: `check`, `detect` and `install` for apps released on GitHub or
Forgejo, installed as a pacman, deb or rpm package, an AppImage or a tarball, with
SHA-256 verification, a permission dialog or terminal fallback, and restart.
