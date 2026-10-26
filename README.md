# praeco

Update checks and in-place updates for Linux desktop apps released on GitHub or Forgejo.

praeco is a Rust library and a command-line tool. It does all the update logic: it
asks a release source what is new, works out how the running app was installed,
downloads the right file, verifies its SHA-256, installs it, and restarts the app.
It draws no UI. Each app shows its own pill, "What's new" sheet and buttons, and
calls praeco underneath.

- **Fairfax** (C++/Qt) runs the `praeco` CLI and reads its JSON.
- **casabon2** (Rust/gpui) can use the `praeco` library crate directly.

Linux only, x86_64 today. Nothing is hard-coded to x86_64: asset selection follows
the machine's architecture (`x86_64`/`amd64`, `aarch64`/`arm64`, ...).

## Quick start

```sh
# Is there something newer than 1.0.4? (JSON)
praeco check --github jrimmer/fairfax --current 1.0.4 --json

# Install 1.0.5 over the running app, then restart it
praeco install --github jrimmer/fairfax --version 1.0.5 --exe /usr/bin/fairfax \
    --restart-pid 4242 --restart -- /usr/bin/fairfax --some-arg

# A Forgejo or Gitea server instead of GitHub
praeco check --forgejo https://code.example.com owner/repo --current 1.0.4 --json
```

## How an update works

1. **Check.** `GET /repos/{owner}/{repo}/releases` (GitHub) or
   `GET /api/v1/repos/{owner}/{repo}/releases` (Forgejo/Gitea). Drafts and
   prereleases are ignored unless `--prereleases`. Versions are compared as semantic
   versions (a leading `v` is fine, `1.2` reads as `1.2.0`). The result lists
   **every** release newer than the running one, newest first, each with its notes.
   The notes are the release body with the install boilerplate cut off at the first
   line that starts with the marker (default `## Install`; `--marker` changes it,
   `--marker ''` keeps whole bodies).
2. **Detect** how the app was installed from its executable path and the
   environment (see below).
3. **Download** the matching asset into a private (`0700`) temporary directory.
   Only URLs from the release's own asset list are used, and only if they belong to
   the configured repository.
4. **Verify** the SHA-256 against the release's `<asset>.sha256` file and/or
   `SHA256SUMS`. No match, or no checksum at all, means failure: nothing is
   installed.
5. **Install** (dialog or terminal mode).
6. **Restart** the app, if asked.

A check is one request. It sends `If-None-Match` with the cached `ETag` (cache in
`$XDG_CACHE_HOME/praeco`, or `~/.cache/praeco`), so repeated checks are cheap:
GitHub does not count a `304` against the rate limit. Rate limits come back as a
clean `rate_limited` error with a "try again in N minutes" message. There is no
authentication by default; if you need it, put a token in the environment variable
`PRAECO_TOKEN` (or name another with `--token-env`). The token goes to the API host
only, never to downloads. Only the first page of releases is read (100 on GitHub, 50
on Forgejo), which is plenty for "what is new since my version".

## CLI reference

```
praeco check   SOURCE --current VERSION [--marker TEXT] [--prereleases] [--exe PATH] [--json]
praeco install SOURCE --version VERSION --exe PATH [--mode dialog|terminal]
               [--restart-pid PID] [--restart -- COMMAND [ARGS...]]
praeco detect  --exe PATH
praeco --help | --version
```

Source (exactly one):

| Flag | Meaning |
| --- | --- |
| `--github OWNER/REPO` | GitHub release source |
| `--forgejo BASE_URL OWNER/REPO` | Forgejo or Gitea at `BASE_URL`, e.g. `https://code.example.com` |
| `--github-api-base URL` | With `--github`: a GitHub-compatible API base (GitHub Enterprise) |

HTTPS is required. Plain HTTP is accepted only for loopback addresses.

Options for all commands:

| Flag | Meaning |
| --- | --- |
| `--prereleases` | Include prereleases (default: stable only). For `install`, needed to install a prerelease version. |
| `--asset-pattern KIND=GLOB` | Asset name pattern for `KIND` (`pacman`, `deb`, `rpm`, `appimage`, `tarball`). Repeatable; replaces that kind's defaults. `*` and `?` wildcards, case-insensitive. `{arch}` expands to the machine's architecture names. |
| `--token-env NAME` | Environment variable holding an API token (default `PRAECO_TOKEN`) |
| `--appimage PATH`, `--appdir PATH` | The AppImage the app runs from and its mount directory. Default: the `APPIMAGE` and `APPDIR` environment variables. |

`check`:

| Flag | Meaning |
| --- | --- |
| `--current VERSION` | The running app's version (required) |
| `--marker TEXT` | Cut notes at the first line starting with `TEXT` (default `## Install`) |
| `--exe PATH` | Also report how that executable is installed and which asset would be used |
| `--no-cache` | Skip the ETag cache |
| `--json` | One JSON object on stdout. Without it, a short human summary. |

`install`:

| Flag | Meaning |
| --- | --- |
| `--version VERSION` | Version to install (required); must be a release of the source |
| `--exe PATH` | The running app's executable (required); decides the install kind |
| `--mode dialog\|terminal` | `dialog` (default): permission prompt through polkit. `terminal`: open a terminal and use `sudo`. |
| `--restart-pid PID` | The app's PID: it is sent SIGTERM and awaited before relaunch |
| `--restart -- CMD ARGS...` | The command line that starts the new version. Everything after `--` belongs to it, so it must come last. Without `--restart-pid`, the command is just started. |
| `--restart-timeout SECS` | How long to wait for the app to quit (default 30) |

Output of `install` is JSON lines on stdout, one event per line, flushed as it
happens. A closed pipe never kills praeco, which matters when the app quits
mid-restart.

Environment: `PRAECO_ESCALATION_CMD` replaces `pkexec`/`sudo` with another command
(whitespace-separated argv placed in front of the package manager); see
[Security](#security).

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. For `check`, also when no update exists. |
| 1 | Failure: network, API, rate limit, missing or bad checksum, install failed, permission refused, terminal closed |
| 2 | Usage error (bad flags) |
| 3 | `needs_terminal`: dialog mode cannot work here; run again with `--mode terminal` |
| 4 | Unsupported install (a source build): nothing to update automatically |
| 5 | Installed, but the app could not be restarted (the new version is on disk) |

## JSON formats

### `check --json`

```json
{
  "ok": true,
  "source": "github:jrimmer/fairfax",
  "current": "1.0.4",
  "latest": "1.0.6",
  "update_available": true,
  "releases": [
    {
      "version": "1.0.6",
      "tag": "v1.0.6",
      "name": "Fairfax 1.0.6",
      "published_at": "2026-10-24T09:12:00Z",
      "url": "https://github.com/jrimmer/fairfax/releases/tag/v1.0.6",
      "prerelease": false,
      "notes": "### Fixed\n- The sidebar no longer flickers."
    },
    {
      "version": "1.0.5",
      "tag": "v1.0.5",
      "name": "Fairfax 1.0.5",
      "published_at": "2026-10-20T16:40:00Z",
      "url": "https://github.com/jrimmer/fairfax/releases/tag/v1.0.5",
      "prerelease": false,
      "notes": "### Added\n- Tag chips in the editor."
    }
  ]
}
```

(The real output is a single line.)

- `latest` is the highest eligible release, whether or not it is newer than `current`
  (`null` when the source has no eligible releases).
- `releases` holds every release newer than `current`, newest first; empty when up
  to date.
- `notes` is Markdown: the release body up to the install marker.

With `--exe PATH`, an `install` object is added, so the app knows whether to offer
an Install button at all:

```json
"install": {
  "kind": "pacman",
  "supported": true,
  "reason": "Installed as a pacman package",
  "detail": "/usr/bin/fairfax is owned by fairfax 1.0.4-1",
  "target": "/usr/bin/fairfax",
  "asset": { "name": "fairfax-x86_64.pkg.tar.zst", "size": 18874368 }
}
```

`kind` is `pacman`, `deb`, `rpm`, `appimage`, `tarball` or `unsupported`.
`supported` is false for `unsupported`, and also when the latest release has no file
for this kind and architecture; `reason` says which.

Up to date:

```json
{"ok":true,"source":"github:jrimmer/fairfax","current":"1.0.6","latest":"1.0.6","update_available":false,"releases":[]}
```

Failure (exit code 1: offline, rate limit, bad response, ...):

```json
{"ok":false,"error":{"code":"network","message":"Could not reach api.github.com. Check the internet connection. (...)"}}
```

Error codes: `usage`, `invalid_version`, `network`, `rate_limited`, `api`,
`no_release`, `no_asset`, `unsupported`, `checksum_missing`, `checksum_mismatch`,
`download`, `needs_terminal`, `auth_cancelled`, `install_failed`, `terminal_closed`,
`restart_failed`, `io`. Messages are plain language and can be shown to a user as is.

### `detect`

```json
{"ok":true,"kind":"appimage","supported":true,"reason":"Running from the AppImage /home/jason/Apps/Fairfax.AppImage","detail":null,"target":"/home/jason/Apps/Fairfax.AppImage","asset":null}
```

### `install`: JSON lines

A successful dialog-mode install with a restart:

```
{"event":"resolving","version":"1.0.5"}
{"event":"downloading","asset":"fairfax-x86_64.pkg.tar.zst","bytes":0,"total":18874368}
{"event":"downloading","asset":"fairfax-x86_64.pkg.tar.zst","bytes":9437184,"total":18874368}
{"event":"downloading","asset":"fairfax-x86_64.pkg.tar.zst","bytes":18874368,"total":18874368}
{"event":"verifying","asset":"fairfax-x86_64.pkg.tar.zst"}
{"event":"installing","kind":"pacman","message":"Installing the pacman package. Administrator permission is needed."}
{"event":"installed","version":"1.0.5","kind":"pacman"}
{"event":"restarting","message":"Closing the app so the new version can start..."}
{"event":"restarting","message":"Starting the new version..."}
{"event":"done","version":"1.0.5","restarted":true}
```

| Event | Fields | Notes |
| --- | --- | --- |
| `resolving` | `version` | First event |
| `downloading` | `asset`, `bytes`, `total` | First, last, and at most every ~150 ms between. `total` is `null` if unknown. |
| `verifying` | `asset` | The SHA-256 check |
| `installing` | `kind`, `message` | `message` is plain language |
| `installed` | `version`, `kind` | The new version is on disk |
| `restarting` | `message` | Only with `--restart`; two events (closing, starting) |
| `done` | `version`, `restarted` | Last event of a success |
| `terminal_opened` | `terminal` | Terminal mode only: first event, which terminal was started |
| `needs_terminal` | `message` | Last event, exit code 3: retry with `--mode terminal` |
| `error` | `code`, `message` | Last event of a failure; the exit code follows the table above |

Every run ends with exactly one of `done`, `needs_terminal` or `error`. In terminal
mode the same events are relayed to stdout; the plain-language progress is shown in
the terminal window instead. Usage errors (exit 2) print the failure object shown
for `check` instead of events.

Examples of the last line on failure:

```
{"event":"error","code":"checksum_mismatch","message":"The downloaded fairfax-x86_64.AppImage does not match the checksum in fairfax-x86_64.AppImage.sha256. It was not installed."}
{"event":"needs_terminal","message":"No polkit authentication agent is running, so a permission prompt cannot appear. Continue in a terminal."}
{"event":"error","code":"unsupported","message":"/opt/build/fairfax is not owned by a package, is not an AppImage and is not under your home folder, so it looks like a source build. Update it the way you built it."}
```

## Install kinds and the exact commands

Detection looks at the executable path (`--exe`) and the environment, in this order:

| Order | Test | Kind | Default asset pattern |
| --- | --- | --- | --- |
| 1 | `APPIMAGE` (or `--appimage`) names an existing file, and the executable is under `APPDIR` when that is set | `appimage` | `*.AppImage` |
| 2 | `pacman -Qo <exe>` succeeds | `pacman` | `*.pkg.tar.zst` |
| 3 | `dpkg -S <exe>` succeeds | `deb` | `*.deb` |
| 4 | `rpm -qf <exe>` succeeds | `rpm` | `*.rpm` |
| 5 | the executable lives under the user's home | `tarball` | `*.tar.gz` |
| 6 | anything else | `unsupported` (a source build), with a reason | none |

The package queries are tried on the path as given and on its symlink-resolved path.
A build you compiled yourself under your home folder is indistinguishable from a
tarball install and is treated as one.

Asset selection: the name must match the kind's pattern; names for another
architecture (`aarch64`, `arm64`, ...) are excluded; a name that mentions this
architecture beats one that mentions none. Fairfax's assets
(`fairfax-x86_64.pkg.tar.zst`, `fairfax-amd64.deb`, `fairfax-x86_64.rpm`,
`fairfax-x86_64.AppImage`, `fairfax-linux-x86_64.tar.gz`, their `.sha256` files and
`SHA256SUMS`) all match the defaults. Override with `--asset-pattern`, for example
`--asset-pattern 'deb=myapp_*_{arch}.deb'`.

The commands that run **elevated**, always on the verified local file in the private
temp directory, never a script (`<file>` is its absolute path; the program is
resolved to an absolute path first):

| Kind | Elevated command |
| --- | --- |
| pacman | `pacman -U --noconfirm <file>` |
| deb | `apt-get install -y <file>`, or `dpkg -i <file>` if `apt-get` is not installed |
| rpm | `dnf install -y <file>`, or `rpm -U <file>` if `dnf` is not installed |

Why these: `apt-get install` and `dnf install` on a local file also pull in
dependencies and handle it as an upgrade; `pacman -U` is the supported way to install
a local package; `dpkg -i` and `rpm -U` are the plain fallbacks. In dialog mode the
full command is `pkexec <program> <args> <file>`; in terminal mode it is
`sudo <program> <args> <file>`; when praeco already runs as root there is no prefix.
`apt-get` may print a notice that the file cannot be read by the `_apt` user and is
used "unsandboxed"; that is expected for a local file and harmless.

No root is involved for the other two kinds:

- **AppImage**: the file is written beside the old one (hidden name, same directory),
  fsynced, given the old file's mode (the owner execute bit is guaranteed), and
  renamed over it; the directory is fsynced too. A running AppImage keeps working
  from the old file until it exits. If anything fails the old file is untouched and
  the temporary file is removed.
- **Tarball**: praeco finds the archive entry for the app's executable (same file
  name, longest common path suffix with the installed path). Everything above that
  suffix in the archive is dropped, and everything above it in the installed path is
  the install root. For `~/.local/opt/fairfax/bin/fairfax` and an archive holding
  `fairfax-1.0.5/bin/fairfax`, the root is `~/.local/opt/fairfax` and the
  `fairfax-1.0.5/` prefix is stripped. The archive is extracted into a private staging
  directory inside the root (same file system), refusing absolute paths, `..`, hard
  links, devices and symlinks that point outside their tree; then each file is
  renamed over its destination. Each rename is atomic, so a running program never
  sees a half-written file; the tree as a whole is not one transaction. Files in the
  install that are not in the archive are left alone (nothing is deleted).

## Dialog mode, terminal mode and the fallback

**Dialog mode (default)** asks for administrator rights with `pkexec`, which shows
the desktop's own polkit prompt. praeco decides up front, before downloading
anything, whether that can work. It reports `needs_terminal` (exit code 3) unless
all of these hold:

1. `pkexec` is on `PATH`;
2. a graphical session exists (`WAYLAND_DISPLAY` or `DISPLAY` is set);
3. a polkit authentication agent is running: some process in `/proc` named like
   `*polkit*` or `*policykit*` (`polkit-gnome-authentication-agent-1`,
   `hyprpolkitagent`, `lxqt-policykit-agent`, ...; `polkitd`, the daemon, does not
   count), or a shell that embeds an agent (`gnome-shell`, `cinnamon`, `cosmic-osd`).

Item 3 is a heuristic. If it guesses wrong and `pkexec` still cannot show a prompt, it
exits 127, which praeco also turns into `needs_terminal`; exit 126 (dismissed or
refused) becomes `auth_cancelled`. When praeco already runs as root none of this is
needed. AppImage and tarball installs need no rights and never fall back.

The app's job on `needs_terminal` is to run the same `praeco install ...` again with
`--mode terminal`.

**Terminal mode** (`--mode terminal`) re-runs praeco itself inside the user's
terminal, doing the same steps with `sudo` instead of `pkexec`, and shows plain
language progress. On failure it waits for Enter so the message can be read. The
terminal is chosen as: `xdg-terminal-exec`, then `$TERMINAL`, then the first found of
foot, kitty, alacritty, ghostty, wezterm, gnome-terminal, konsole, xfce4-terminal,
tilix, terminator, lxterminal, x-terminal-emulator, urxvt, xterm. The terminal
process and the original praeco talk through an events file in a private temp
directory; the original relays the worker's events to stdout as JSON lines. If the
window is closed before the update finishes, the result is `terminal_closed`. The
restart is done by the original process after the worker reports success, with the
same code as dialog mode. (The worker is the hidden mode `--mode terminal-inner`;
apps should not call it.)

## Restart

With `--restart-pid N --restart -- COMMAND...`, after a successful install praeco:

1. sends `SIGTERM` to `N` (the app should quit cleanly) and waits, polling every
   100 ms, until it has exited (a zombie counts as exited), up to `--restart-timeout`
   seconds (default 30). If it does not exit, praeco stops with `restart_failed`
   ("The app did not close within 30 seconds. The update is installed; ..."), exit
   code 5, and never sends `SIGKILL`;
2. starts `COMMAND...` fully detached: a new session (`setsid`), no controlling
   terminal, stdio on `/dev/null`, not through a shell. It survives praeco exiting
   and the terminal closing. The AppImage variables `APPIMAGE`, `APPDIR`, `ARGV0` and
   `OWD` are removed from its environment, since they describe the old mount;
3. exits.

`N` must be a real process other than praeco and init. praeco ignores `SIGHUP`
during an install so a closing terminal does not interrupt a package manager.

The app is told to quit with a signal, so it should handle `SIGTERM` by quitting
cleanly (a Qt app can install a handler that calls `QCoreApplication::quit()`).
Start praeco with `QProcess::startDetached`, or otherwise make sure the app's exit
does not take praeco down with it: if praeco is a plain child that the app kills on
exit, nobody is left to start the new version.

## Using praeco from an app

### Fairfax (C++/Qt): call the CLI

On startup, now and then, and on "Check for updates":

```cpp
QProcess p;
p.start("praeco", {"check", "--github", "jrimmer/fairfax", "--current", APP_VERSION,
                   "--exe", QCoreApplication::applicationFilePath(), "--json"});
p.waitForFinished(20000);
auto doc = QJsonDocument::fromJson(p.readAllStandardOutput());   // always valid JSON with --json
```

Show the pill only when `update_available` is true and `latest` is not the version
the user chose to skip. Use `install.supported` to decide whether the sheet offers
**Install** or only a link to `releases[0].url`.

To install, run `praeco install` with `--version` set to the `latest` from the check,
`--exe`, and the restart arguments: `--restart-pid` is
`QCoreApplication::applicationPid()`, and the command after `--restart --` is
`applicationFilePath()` plus its arguments (for an AppImage, the `APPIMAGE` path).
Read stdout line by line and drive a progress bar from `downloading`
(`bytes`/`total`). On `needs_terminal`, run the same command again with
`--mode terminal`. Let SIGTERM quit the app cleanly when the restart comes, and show
`message` text as is.

### casabon2 (Rust/gpui): use the library

```rust
use praeco::{check, CheckOptions, InstallOptions, Mode, RestartSpec, Source};

let src = Source::github("jrimmer/casabon2")?;
// Blocking: run on a background thread.
let result = check(&CheckOptions::new(src.clone(), env!("CARGO_PKG_VERSION")))?;
if result.update_available {
    // show the pill and the sheet from result.releases
}

let exe = std::env::current_exe()?;
let mut opts = InstallOptions::new(src, "1.2.0", &exe);
opts.mode = Mode::Dialog;
let restart = RestartSpec::new(Some(std::process::id()), vec![exe.into()]);
praeco::update(&opts, Some(&restart), &mut |event| { /* forward to the UI */ })?;
```

`praeco::update` sends `SIGTERM` to the PID in the `RestartSpec`. An app that wants
to do its own shutdown can instead pass `None` as the PID, or call `praeco::install`
alone, then quit and relaunch with `praeco::restart::launch_detached`.
Terminal mode starts the `praeco` executable inside a terminal (it locates itself
with `std::env::current_exe`), so a library user that wants the terminal fallback
should ship the `praeco` binary and run the CLI for that case.

### What the app should show

praeco supplies the data; the UI belongs to the app:

- **A pill** near the version number or title bar when `update_available` is true:
  "Update 1.0.6".
- **A "What's new" sheet** opened from the pill, listing `releases` newest first
  (version, date, rendered Markdown `notes`).
- **Buttons: Install / Later / Skip this version.** Install runs the install command
  (progress from the events, then the app restarts). Later closes the sheet and asks
  again at the next check. Skip this version stores `latest` in the app's settings
  and hides the pill until a newer version appears.

## Library API

All blocking; run it on a worker thread.

| Item | Purpose |
| --- | --- |
| `Source::github`, `Source::forgejo` | Where releases come from |
| `check(&CheckOptions) -> CheckResult` | The check, as in `check --json` |
| `detect(&Env, &Path) -> Detection` | How an executable was installed |
| `InstallOptions`, `install(...)` | Download, verify, install; progress through a callback |
| `update(&InstallOptions, Option<&RestartSpec>, &mut FnMut(&Event))` | Install in either mode, restart, finish with `done` |
| `Env` | Everything praeco reads from the machine (`PATH`, `APPIMAGE`, uid, `/proc` root, ...), replaceable in tests |
| `InstallOptions::escalation` | Replaces `pkexec`/`sudo` (an argv prefix) |

Errors are `praeco::Error { code, message }`; `code.as_str()` is the string the CLI
prints.

## Security

What is trusted:

- **HTTPS to the release source.** Certificates are checked against the bundled
  Mozilla root set (rustls with `webpki-roots`); HTTP is refused except on loopback.
  Redirects are followed by hand, one hop at a time, and every hop must be HTTPS.
- **The repository's release.** The release list from the configured source says
  which files exist. Only URLs from that asset list are downloaded, and they must be
  HTTPS URLs on the source's own host under that repository's release-download path
  (GitHub's CDN redirects are followed from there). A release that lists an asset
  anywhere else is refused before any request is made.
- **The checksum published with the release.** The file must match every checksum
  source that mentions it (`<asset>.sha256`, `SHA256SUMS`), and at least one must
  exist. Missing, unreadable or mismatching means failure, and nothing is run.

What is not trusted or not done:

- **There are no signatures yet.** The checksum comes from the same place as the
  file, so it catches corruption and a tampered mirror or CDN, but not a compromised
  repository or release account: anyone who can publish a release can publish a
  matching checksum. Signature verification is future work.
- praeco does not check that the requested version is newer than the installed one
  (the app decides), or what is inside a package.

Privilege handling:

- **Only the package manager runs elevated, on a verified file.** praeco itself
  never runs as root unless you start it that way. The elevated process is
  `pacman`, `apt-get`/`dpkg` or `dnf`/`rpm` with the arguments listed above. praeco
  never executes anything it downloaded, and never runs a script or a shell.
- Right before handing the file to the elevated command, praeco hashes it again and
  refuses to continue if it changed since verification. The file lives in a `0700`
  directory only the user can write to; a process already running as the same user
  could still swap it in the last instant, which is the limit of what a user-owned
  file can promise.
- Downloads go to a fresh private temp directory that is removed afterwards.
- AppImage and tarball installs run as the user and write only inside the existing
  install location.
- `PRAECO_ESCALATION_CMD` replaces the `pkexec`/`sudo` prefix. It is for tests and
  unusual setups (`doas`). It is read from the environment of the user who runs
  praeco, so it grants nothing that user did not already have.
- An optional API token is sent only to the API host and is never put on a command
  line.

## Limitations

- Linux only.
- The tarball install needs the archive to contain the app's executable under the same
  file name, and does not delete files that a newer version no longer ships.
- The polkit-agent probe is a heuristic (see above); pkexec's exit codes catch what
  it misses.
- Terminal mode does not forward `PRAECO_TOKEN`. `APPIMAGE` and `APPDIR` are passed
  explicitly, since some terminals (gnome-terminal's server) do not inherit the
  environment.
- Private repositories are not supported: asset downloads carry no credentials.
- Only the first page of releases is read.
- No delta updates, no rollback, no signature checks.

## Building and testing

```sh
cargo build --release      # target/release/praeco
cargo test                 # offline, no root
cargo clippy --all-targets
```

The tests need neither network nor root. They use a fake release server bound to
`127.0.0.1`, fake `pacman`/`dpkg`/`rpm`/`apt-get`/`dnf`/`pkexec`/`sudo` shims on a
private `PATH`, a recording escalation command that proves the exact package-manager
argv without running it, a fake `/proc` for the polkit probe, a fake `APPIMAGE`, and
dummy child processes for restart.

## License

MIT, see [LICENSE](LICENSE).
