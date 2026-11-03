# Ilia (Rust)

Build from this directory with `cargo build --release --locked`.
The binary is `target/release/ilia`. Cargo
resolves the locked dependencies from crates.io; the only crate outside the
gtk-rs graph is `libc`, for `dlopen`. The SPARQL library (`libtinysparql-3.0.so.0`, or
`libtracker-sparql-3.0.so.0` on older systems) is not linked: the Files page
loads it the first time it searches, so every other page works without it.
`sparql.rs`, which wraps its C calls in a safe API, is the only module with
`unsafe`.

This is the complete port. All seven pages are real: Applications,
Keybindings and Windows, and the Commands (terminal),
Notifications, TextList, and Files (Tracker) pages. The dialog shell is
complete as well: stylesheet modes, Alt+letter page switching, Alt+plus /
Alt+minus resizing with `window-width`/`window-height` persistence, the Help
tab and its entry desensitizing, the Ctrl entry-editing keys, focus-loss and
click-outside quitting, and the seat-grab retry with its failure messages.

Module map:

- `rofication.rs`, `notifications.rs`: the rofication socket client
  (`/tmp/rofi_notification_daemon`, read-until-EOF `list`, `del:`, `delm:`)
  and the notifications page.
- `sparql.rs`, `tracker.rs`: the run-time SPARQL binding and the Files page,
  which searches on worker threads once typing pauses.
- `terminal.rs`: the Commands page (asynchronous PATH scan, launches through
  the `systemd-run` wrapper in `runner.rs`).
- `textlist.rs`: the TextList page (stdin drained synchronously at startup,
  filtered into a prebuilt list).
- `filter.rs`: the every-word query matching shared by the pages.
- `icons.rs`: the reference icon-loader order shared by the new pages.
- `input_sources.rs`: the desktop's layout-switch shortcut while ilia is
  open (gnome-flashback's input-source D-Bus service on X11).
- `dialog.rs`, `application.rs`: the shell.

## Meson cutover

The meson build no longer declares the `vala` language and has no Vala
target. `meson compile` runs `meson-build.sh`, which builds this crate with
`cargo build --release --locked` (target directory inside the meson
build tree) and exposes the binary as `build/src/ilia`. `meson install`
installs the same payload as before: the `ilia` binary, the gschema XML (its
compilation is skipped under `DESTDIR`), and the `data/local` and `doc`
copies; no desktop or appdata file. The Vala unit-test target retired with the
Vala sources (its `systemd_escape` and `compare_desktop_apps` vectors live on
as crate unit tests); `meson test` runs a `-v` smoke test of the built binary.
`debian/control` builds with `cargo` and `rustc` instead of `valac`.

## Probes

`smoke-bootstrap.sh` is the bootstrap probe with the compositor-page coverage.
With no WM socket and no rofication daemon it now expects the reference's
single `Error: load_notifications failed: ...` line from the notifications
page as well (one line for `-p notifications`; three lines in `-a` modes, in
the reference order keybindings, windows, notifications).


### Cutover probe (`smoke-cutover.sh`)

`./rust/smoke-cutover.sh` (from the repository root; `ILIA_BINARY` overrides
`rust/target/release/ilia`, `ILIA_SOURCE_ROOT` overrides the checkout) runs unprivileged under a private Xvfb display, a private
D-Bus session whose only activatable service is the accessibility bus (so no
Tracker miner exists), and private `HOME`/`TMPDIR`/`XDG_*` dirs with a
compiled schema copy. Each case starts a fresh `ilia` with a private `bin`
dir as its whole `PATH` and the `parity.fakes`, and prints one
`ok: <case>` line: terminal quiet and `x-terminal-emulator -e` launches
(wrapper-before-target transcripts) and the unmatched-Enter no-op; textlist
selection, filtered selection, `-l`/`-i`, and unmatched Enter; notifications
single delete (`list`, `del:11`, `list`) and both filtered multi-deletes
against the fake daemon on `/tmp/rofi_notification_daemon` (removed at
teardown); Tracker no-miner; stylesheet modes (`-t` missing file, `-t` file,
`-n`, default); `Alt+w` page switching in `-a` mode observed through the
AT-SPI helper; `Alt+plus`/`Alt+minus` resize with persistence read from the
keyfile gsettings backend; focus-loss quit (focus moved to another window,
then the seat grab broken with the XKB `XF86Ungrab` action, since GDK ignores
focus moves reported while grabbed); and the meson build (setup, compile,
start/Escape of `build/src/ilia`, then a `DESTDIR`-staged install checked for
the binary, gschema XML, `share/ilia` and `share/doc/ilia` copies, no
desktop/appdata file, no compiled schemas, nothing outside the stage).
The GUI cases also pass against the Vala reference binary.

## Validation

Against the parity harness:

- `cargo fmt --check`: passed.
- `cargo test --locked`: 49 unit and 5 CLI integration tests passed.
- `cargo build --release --locked`: passed without warnings.
- Full parity suite against the Rust binary (`target/release/ilia`): 23 passed,
  0 failed.
- `smoke-bootstrap.sh`: passed.
- `smoke-cutover.sh`: all 20 cases passed (unprivileged); its GUI cases also
  pass against the Vala reference.
- Cutover with a failing `valac` stub first on `PATH`: `meson setup`,
  `meson compile`, and `meson test` passed; the full parity suite passed
  against the meson-built binary; a `DESTDIR`-staged `meson install` produced
  `usr/local/bin/ilia`, the gschema XML, and the `share/ilia` and
  `share/doc/ilia` copies, with no desktop or appdata file and no compiled
  schema.

## Known limits

- Launch results: gio-rs reports a launch that returns `FALSE` without a
  `GError` as success, so the reference's message-only failure lines
  (`Error: execute_command failed`, `Error: execute_app failed`,
  `Error: execute_keybinding failed`) cannot be produced from safe Rust. GIO
  sets an error whenever a launch fails, so the observable failure paths are
  the `<prefix>: <message>` forms, which are reproduced.
- Text that is not valid UTF-8 is shown lossily, since GTK string values must
  be UTF-8; TextList keeps the bytes of such stdin lines and prints them back
  unchanged.
- History overflow retains 32 entries: the oldest entry is discarded and the
  newly launched desktop ID remains last. The Applications page keeps
  its `icon-size=0` behavior: icon loading is skipped and stderr stays clean
  instead of constructing the reference's invalid 0x0 blank pixbuf.
- Native Wayland checks compositor support, initializes the GTK window as an
  overlay layer-shell surface, requests exclusive keyboard input, and skips
  the X11 seat grab. An unsupported compositor exits with the pinned message.
