# ilia-rs

A Rust port of [ilia](https://github.com/regolith-linux/ilia), the launcher
of the [Regolith](https://regolith-desktop.com/) desktop: a small GTK 3
window that filters applications, commands, keybindings, notifications,
windows, files and piped-in text lists, and acts on the selected row. It
runs under i3 (X11) and sway (Wayland, as a layer-shell surface).

## Status

**Experimental port; not a drop-in replacement yet.**

- **A specific moment in time.** The port reproduces ilia as of
  [regolith-linux/ilia@d982583](https://github.com/regolith-linux/ilia/commit/d982583f94dfe846b4a26d8ce53c231a16fa0167)
  (version 0.18.0, November 2025): a black-box parity suite (`parity/`)
  written against that Vala build passes against the Rust binary. On top of
  that it carries the bug fixes listed below.
- **Upstream has moved on.** ilia has received many fixes since 0.18.0.
  Some are already in this port (fixed here independently, or ported), and
  some still need porting: #143, #146, #149 and #150 among the merged ones,
  and the open #158–#161 (running a keybinding from the keybindings page).
  See [Differences from upstream](#differences-from-upstream). That list
  was made on 2026-11-03 against upstream `main` at `a799dc1` and may not be
  complete.
- **Very deep testing is needed** before anyone considers this port as a
  replacement for the Vala ilia.
- **No deep code reviews yet.** The port has not yet been reviewed in depth
  by the major AI coding agents and code reviewers. Those reviews are
  planned before any replacement decision.
- **Upstreaming is undecided.** This repository is not a pull request to
  regolith-linux/ilia; whether, how and when any of it goes upstream will be
  decided later.

## What it ports

Everything the Vala program does, page by page: Applications (desktop
entries, history-ranked sorting, `systemd-run` launch wrapper), Commands
(PATH scan), Notifications (rofication socket client), TextList (stdin
rows), Files (Tracker/tinysparql full-text search), Keybindings and Windows
(i3/sway IPC), and the dialog shell (stylesheets, Alt+letter page
switching, Alt+plus/Alt+minus resizing with persistence, the Help tab,
Ctrl entry-editing keys, focus-loss quitting, the X11 seat grab, native
Wayland layer-shell start-up). The command line, GSettings schema
(`org.regolith-linux.ilia`), installed files and Debian package name are
unchanged. `rust/README.md` has the module map and developer notes.

## History

The branch starts from upstream's history up to `d982583` and adds the port
one step per commit:

1. `parity:` a black-box parity suite for the Vala launcher (23 cases).
2. `rust:` the Rust/GTK3 foundation: CLI, dialog shell and page stubs.
3. `rust:` the Applications page.
4. `rust:` the Keybindings and Windows pages and native Wayland start-up.
5. Cut over to Rust: the remaining pages, the meson and Debian build; the
   Vala sources are removed.
6. `rust:` twenty launcher bugs fixed after the port (the port first
   reproduced 0.18.0 exactly, bugs included).
7. `rust:` keyboard-layout switching while the launcher is open.

Then seven upstream fixes ported one commit each (#147, #148, #151, #152,
#156, #157, #145), a test-harness commit that lets the parity suite and
probes run outside the original build image, and a commit that updates the
tests for the behaviour step 6 changed on purpose.

## Differences from upstream

Compared with regolith-linux/ilia `main` at `a799dc1` (2026-11-02), merged
pull requests #125–#157:

**Already in the port** (fixed independently in steps 6–7, or by design):
#125 icons loaded on the main thread and no icon work with `icon-size=0`;
#126 whole IPC replies read; #127 Enter accepts text that matches no row;
#128 multi-word filter; #129 Home/End; #130 tinysparql or tracker-sparql
loaded at run time; #131 `Terminal=true` apps in a terminal; #132 newest
notification first; #133 layout switch while open (most-recently-used
order); #134 large text lists; #135 Ctrl+n/p from the highlighted row;
#136 keypad Enter; #137 Delete removes the highlighted notification; #138
silent rofication daemon; #139 Wayland detected from the GDK display; #140
Alt+minus on Wayland; #141 `SWAYSOCK`; #142 file search off the UI thread;
#153 entry positions counted in characters; #154 transparent icon
placeholder; #155 right Super closes.

**Ported in this repository** (one commit each, with a unit test or a
headless check): #145 entry text applied to the switched page; #147 dialog
windows listed on i3; #148 files found by content; #151 apps searched by
generic name, keywords and comment; #152 new last notification highlighted
after deleting the last one; #156 each command listed once, only
executables; #157 paste without blocking on the clipboard owner.

**Not yet ported** (open items):

- #143: close before a suspend or screen lock on X11, so the locker can
  grab the keyboard (logind `PrepareForSleep`, `org.gnome.ScreenSaver`
  `ActiveChanged`).
- #149: Windows page keeps its icon column (fixed renderer size, generic
  icon for windows without one).
- #150: highlight the first match whenever the filter text changes, also
  after arrow-key moves, on every list page.
- #146: version string. The port prints `ilia version 0.12`, as 0.18.0
  did; upstream now prints 0.19.
- Open upstream PRs #158–#161 (keybindings page): run the chosen binding
  over IPC with quotes intact, run a clicked row on X11, read config files
  in order each on its own lines, and send the command the window manager
  holds for the binding. The port still has the 0.18.0 behaviour, and
  `parity/test_keybindings.py` asserts it, so those tests need updating
  when these fixes are ported.

**In the port but not (known to be) upstream:** text-list lines that are not
valid UTF-8 print back byte-exact; one notification missing a field no
longer empties the list; a non-UTF-8 argument no longer aborts; a broken
`-t` stylesheet is reported and the default style is used; the Commands
list stays sorted while PATH is scanned. Files shows up to 1000 results
(upstream: 100).

## Build

On Ubuntu 26.04, install the build dependencies from `debian/control`
(for example `sudo apt build-dep ./` from the checkout), then:

```sh
meson setup build
meson compile -C build      # runs cargo; fetches the locked crates
build/src/ilia -p apps
```

or with cargo alone:

```sh
cd rust
cargo build --release --locked
target/release/ilia -p apps
```

Offline builds: vendor the crates once, and cargo (and therefore meson and
the Debian build) uses them:

```sh
cd rust
mkdir -p .cargo
cargo vendor --locked vendor > .cargo/config.toml
```

The Files page loads `libtinysparql-3.0.so.0` (or
`libtracker-sparql-3.0.so.0`) at run time; every other page works without
it.

## Test

```sh
(cd rust && cargo test --locked && cargo fmt --check)
meson test -C build
ILIA_BINARY=$PWD/rust/target/release/ilia python3 -m pytest parity/ -v
ILIA_BINARY=$PWD/rust/target/release/ilia rust/smoke-bootstrap.sh
ILIA_BINARY=$PWD/rust/target/release/ilia rust/smoke-cutover.sh
```

The parity suite and the probes need Xvfb, xdotool, python3-pytest,
python3-gi with the AT-SPI typelib, and `dbus-daemon`. The Windows and
Keybindings parity cases expect `/usr/bin/i3-msg` and `/usr/bin/swaymsg` to
be absent (see `parity/README.md`). In a container that cannot create user
namespaces, GdkPixbuf's glycin loaders print
`WARNING: Glycin running without sandbox.` on stderr, which the probes'
exact stderr checks reject; run them where bwrap works.

## Package

```sh
dpkg-buildpackage -b -us -uc
```

builds `ilia_<version>_<arch>.deb` with the same payload as the Vala package
(the binary, the GSettings schema, and the `data/local` and `doc` copies).
cargo needs network access during the build unless the crates were vendored
as above. `debian/control` still lists Vala-era dependencies that the Rust
binary does not use (for example the package depends on `libgee-0.8-2`).
`flake.nix` and `nix/` are left from the Vala project and have not been
updated for the Rust build.

## Usage

The command line is the Vala program's. From the original README:

Ilia is a tool for users to invoke actions such as launching apps, viewing notifications, and finding windows.
It provides a modal interface consisting of a filter box, and a list of matching items.
There are a set pages that can be selected from by specifying the desired page via a command-line parameter or by tab if all pages are loaded (controlled by command-line parameter).


### Features

Ilia provides pages to view and select from the following types of things:

- Desktop Apps (`apps`)
- System Commands (`terminal`)
- Desktop Keybindings (`keybindings`)
- Notifications (`notifications`)
- Text List (`textlist`)
- File Search (`tracker`)
- Open Windows (`windows`)

### Usage

Ilia can be launched from a terminal. The `-p` option allows to specify which page to present to the user.

Ex:

```
ilia -p keybindings
```

#### Desktop Apps

Ilia searches the system-configured `$XDG_DATA_DIRS` with `applications` applied at the end of each path for desktop application files.
For example, if your system has `$HOME/.local/share` in the set of paths in `$XDG_DATA_DIRS`, the path `$HOME/.local/share/applications/` will be searched for desktop app files.

#### Text List

The `textlist` page is a bit different than the others pages.
It provides a general purpose utility to provide a list of options for the user select and when the selection occurs, the selected item is returned by the invocation to `ilia`.
In this way the program can be used in scripts to get user input from dynamic data, similar to `rofi`.
The `textlist` page supports two additional parameters for more customization:

```
'textlist' - select an item from a specified list
        -l: page label
        -i: page icon
```

### Configuration

Ilia supports the following configuration via gsettings under the namespace `org.regolith-linux.ilia`:

- window-width: width of window in pixels
- window-height: height of window in pixels
- icon-size: size of icons in pixels or 0 to disable icons

#### Example: Resetting screen width

```
gsettings reset org.regolith-linux.ilia window-width
```

### Dependencies

#### Notifications

This page communicates with the [Rofication](https://github.com/regolith-linux/regolith-rofication) daemon for managing desktop notifications.

#### Keybindings

This page uses the comment format specified by [Remontoire](https://github.com/regolith-linux/remontoire) and reads the i3 config via it's local socket.

## License

Apache License 2.0, as the original program; see [LICENSE](LICENSE).
ilia was written by Ken Gilmer and the Regolith contributors.
