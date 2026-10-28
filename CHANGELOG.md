# Changelog

## Unreleased

- Dialog mode now recognises the polkit agent that Omarchy and other Quickshell
  desktops run inside the `quickshell` process, so installs there show the
  desktop's permission prompt instead of falling back to a terminal.

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
