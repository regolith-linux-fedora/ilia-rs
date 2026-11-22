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
  written against that Vala build, and updated wherever the port has since
  changed behaviour on purpose, passes against the Rust binary. On top of
  that it carries the bug fixes listed below, which include all of ilia
  0.19.0. It prints `ilia version 0.19`, like ilia; the package version is
  `0.19.0+rs1`.
- **Upstream has moved on.** ilia has received many fixes since 0.18.0.
  Most of the smaller ones are in this port (fixed here independently, or
  ported), but one large one still needs porting: window-manager and
  desktop keys while ilia is open (#171). See
  [Differences from upstream](#differences-from-upstream). That list was
  made on 2026-11-09 against upstream `main` at `b075ac2`, and checked by
  running both builds side by side under i3 and sway.
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

A second batch (November 2026) ported, one commit each: #165 with the rest
of the dialog chrome, the Ctrl+$ fix of #171 with Unicode word motion, #150,
#149, the rest of #138, #141, #143, #166, and #146 with the removal of the
Vala-era packaging files. Each commit updates the parity suite where it
asserted the old behaviour.

Then #169 (matching and ranking) in one commit, together with the rest of
#134 (standard input read while the window is open) and the additions
listed under "In the port but not (known to be) upstream".

Then #158–#161 (running a binding from the keybindings page) in one commit.

Then the open upstream pull request #174 (pages defined in key files, and
no window-manager pages under other window managers) in one commit.

Then the open upstream pull request #184 (adding applications from a form
on the Applications page, and row clicks under the X11 grab) in one commit.

Then the open upstream pull request #181 (the program path of the
highlighted application, and Ctrl+Shift+C to copy it) in one commit.

Then #168 (launching desktop entries per the Desktop Entry spec) in one
commit.

Then #170 (the keybindings page shows the keys the window manager binds)
in one commit.

Then #162 (desktop files read during GTK start-up, icons decoded off the
main thread) in one commit, together with a fix for a start-up deadlock it
brings along and a reader that one stuck desktop file cannot hold up (see
"In the port but not (known to be) upstream").

## Differences from upstream

Compared with regolith-linux/ilia `main` at `b075ac2` (2026-11-09), merged
pull requests #125–#171:

**Already in the port** (fixed independently in steps 6–7, or by design):
#125 icons loaded on the main thread and no icon work with `icon-size=0`;
#126 whole IPC replies read; #127 Enter accepts text that matches no row;
#128 multi-word filter; #129 Home/End; #130 tinysparql or tracker-sparql
loaded at run time; #131 `Terminal=true` apps in a terminal; #132 newest
notification first; #133 layout switch while open (most-recently-used
order); #135 Ctrl+n/p from the highlighted row; #136 keypad Enter; #137
Delete removes the highlighted notification; #139 Wayland detected from the
GDK display; #140 Alt+minus on Wayland; #142 file search off the UI thread;
#153 entry positions counted in characters; #154 transparent icon
placeholder; #155 right Super closes; #167 no GTK criticals for native
Wayland windows.

**Ported in this repository** (one commit each, with a unit test or a
headless check against the Vala build): #134 large text lists filter
without blocking, and standard input is read in the background, so complete
lines join the list while the producer is still writing; #138 a "Could not
load notifications" row when the rofication daemon is missing or silent
(the port already read the list off the main thread with a timeout); #141
the sway socket read from `SWAYSOCK` (an earlier version of this README
listed #141 as already in the port, but a stale `I3SOCK` still won over
`SWAYSOCK` under sway); #143 close before a suspend or a screen lock; #145
entry text applied to the switched page; #146 version string 0.19; #147
dialog windows listed on i3; #148 files found by content; #149 the Windows
page keeps its icon column; #150 the first match highlighted for new text
after arrow keys; #151 apps searched by generic name, keywords and comment;
#152 new last notification highlighted after deleting the last one; #156
each command listed once, only executables; #157 paste without blocking on
the clipboard owner; #165 no page icon in the entry after a tab switch, and
the app-grid icon on the Applications tab; #166 stay open on sway when the
seat's last keyboard goes away; #169 name matches ranked first in seven
groups, accents, marks and case folded on every filtering page, opt-in
subsequence matching (`fuzzy-matching`), `match-fields`, and `hidden-apps`
with Ctrl+Delete; #158–#161 the chosen keybinding run once over IPC exactly
as the window manager holds it (sway's includes read in its order,
variables and resource values taken where they are used, quotes and
spacing kept), also when clicked under the X11 grab; #168 applications
started as their desktop entry describes, without a shell: `%c`, `%i`
and `%k` filled in literally, the file and URL codes left out, the
entry's `Path` as the working directory, `Terminal=true` inside
x-terminal-emulator, all inside the `run_ilia_<id>_<8 hex>.scope` scope
when systemd-run is on `PATH`, `DBusActivatable=true` entries activated
over the session bus (the service started first when its name has no
owner), and a double click launching once; #170 the keybindings page shows
the keys each documented binding is left with after resources retune it
(i3's own reply, sway's configuration with the resource service's values,
key codes through the first layout), keeps the comment's key text while a
row is bound as it is with no resource set, filters on the keys shown, and
runs a row bound only with `bindcode` or standing before a `mode` block;
#162 with no `-p`, `-p apps` or `-a` the desktop files are read on worker
threads started before GTK, the window is shown while they are read and
the list fills in when they are, the rows use the objects read (no second
parse per entry), icons are decoded on GTK's worker threads with the
visible rows first, and Enter pressed before the list arrives launches,
once, the best match for the text typed when it was pressed.

**Ported ahead of upstream** (open upstream pull requests): #174 pages
defined in key files (`ilia/pages/<name>.page`: favourite apps, fixed lines,
or the lines a command prints, with `Exec`, `Sort=frequency`,
`OnlyShowIn`/`NotShowIn` and an Alt key), and the Keybindings and Windows
pages left out under window managers other than i3 and sway, where `-p`
for either exits with an error instead of opening an empty page; #184 a `+`
button and Ctrl+= on the Applications page open a form inside the window
that writes a checked desktop entry to the user's applications directory
and lists and highlights it at once, a click on an Applications row
launches it under the X11 grab too, and keywords holding an escaped `;`
are searched; #181 the strip under the Applications list names the
program of the highlighted app as its `Exec` line finds it (through
`env`, the search path and links, or the session-bus name of a
D-Bus-activatable entry), kept current while the page is shown, and
Ctrl+Shift+C copies the path so that it can still be pasted after ilia
closes.

**Partly ported:**

- #171: only its Ctrl+$ fix (see below).

**Not yet ported** (open items):

- #171: window-manager and desktop keys work while ilia is open.
- Open upstream PRs that will likely land: #164 (notification actions),
  #172 (calculator page) and #173 (desktop-entry actions). #174, #181 and
  #184 are already in the port (see above).

**Dialog details brought in line with ilia** (port bugs, no upstream pull
request): tab icons are flat buttons that switch the page; the Help tab's
heading has the `help_heading` style class and its list fixed row heights;
the Commands and TextList help texts say what Enter does when no row
matches; the list has the focus when the window opens; `-a -p textlist`
reports the unknown page; Ctrl+$ moves to the end of the entry (the key
arrives as `dollar`, never as Shift+4; ilia fixed this in #171); Ctrl+w and
Ctrl+b stop at Unicode punctuation and symbols, as `g_unichar_ispunct`
does.

**In the port but not (known to be) upstream:** text-list lines that are not
valid UTF-8 print back byte-exact; one notification missing a field no
longer empties the list; a non-UTF-8 argument no longer aborts; a broken
`-t` stylesheet is reported and the default style is used; the Commands
list stays sorted while PATH is scanned. Files shows up to 1000 results
(upstream: 100). With #169: changes to `fuzzy-matching`, `match-fields` and
`hidden-apps` apply while the launcher runs (upstream reads them at
start-up), keeping the highlighted row when the list is rebuilt; a
`hidden-apps` entry starting with `!` lists again what it matches, the last
fitting entry deciding; Ctrl+z undoes this run's Ctrl+Delete hides, latest
first. With #162: GIO's default VFS is created before the reader threads
start, because a reader creating it (for an entry whose `Icon` is a full
path) while GTK picks the keyfile settings backend deadlocks GIO and the
window never shows (sway with `GSETTINGS_BACKEND=keyfile` or no dconf;
ilia 0.20.1 starts its reader thread the same way); each desktop file is
read outside GLib on reader threads that a stuck read does not hold up
(GLib reads entries under one process-wide lock), the list comes without
a file still unreadable ten seconds after start and that entry joins it
once read; with `-a`, the Windows page shows the generic icon until the
list arrives instead of waiting in GLib's search, then each window's entry
icon, found as GLib's search ranks entries.

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
python3-gi with the AT-SPI typelib, and `dbus-daemon`. The Windows parity
cases expect `/usr/bin/i3-msg` and `/usr/bin/swaymsg` to be absent (see
`parity/README.md`). In a container that cannot create user
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
as above (or `CARGO_HOME` already holds them and `CARGO_NET_OFFLINE=true` is
set). `debian/control` lists only what the Rust build and binary use; the
Vala project's Nix flake, AutoVala, uncrustify and gettext files were
removed.

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
- Pages defined in files: favourite apps, fixed lines, or the lines a command prints (see [Pages from files](#pages-from-files))

### Usage

Ilia can be launched from a terminal. The `-p` option allows to specify which page to present to the user.

Ex:

```
ilia -p keybindings
```

#### Desktop Apps

Ilia searches the system-configured `$XDG_DATA_DIRS` with `applications` applied at the end of each path for desktop application files.
For example, if your system has `$HOME/.local/share` in the set of paths in `$XDG_DATA_DIRS`, the path `$HOME/.local/share/applications/` will be searched for desktop app files.

The `+` button below the list, or Ctrl and the `+` key (Ctrl+= on a US layout, or the keypad's `+`), opens a form inside the window for adding an application.
It asks for a name and a command, and optionally an icon, a comment, keywords and whether the program runs in a terminal.
The command is split into words with shell quoting rules (no expansion), and its program must be an executable file, given as an absolute path, a `~/` path or a name found on `PATH`.
Add writes a new desktop entry to `$XDG_DATA_HOME/applications` (`~/.local/share/applications` by default), named after the application, with `-2`, `-3`, ... added when that desktop file id is taken.
The new application is listed and highlighted at once. Escape or Cancel closes the form without writing anything.

The same line under the list shows where the highlighted app's program is on disk, as its `Exec` line finds it on `PATH` (through `env`, with the target of a symbolic link after `→`), or `D-Bus: ` and the bus name for an entry started over the session bus. It follows the highlight and changes on disk while ilia is open. Ctrl+Shift+C copies that path, and the copy stays on the clipboard after ilia closes: an X11 clipboard manager takes it, and without one ilia keeps running without a window until something else is copied. The path has the style class `app_path`.

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

#### Pages from files

More pages can be defined in key files, without changing ilia: a file `<name>.page` in the `ilia/pages` directory of an XDG configuration directory.
The directories are `$XDG_CONFIG_HOME/ilia/pages` (`~/.config/ilia/pages`), then `ilia/pages` in each directory of `$XDG_CONFIG_DIRS` (`/etc/xdg/ilia/pages`).
When several files define the same page, the first one found is used: the user's file hides the shared ones.
`<name>` may hold ASCII letters, digits, `-` and `_`, and cannot be the name of a built-in page; names are matched without regard to case.
A page is opened with `ilia -p <name>`, and `ilia -a` shows the pages from files after the built-in ones, ordered by file name.

```
[Ilia Page]
Name=Favourites
Icon=starred
Key=v
Apps=org.gnome.Nautilus.desktop;firefox.desktop;org.gnome.Terminal.desktop
```

```
[Ilia Page]
Name=Hosts
Icon=network-server
Key=h
ItemsCommand=awk '/^Host / && $2 !~ /[*?]/ { print $2 }' ~/.ssh/config
Exec=x-terminal-emulator -e ssh %s
Sort=frequency
OnlyShowIn=i3;sway
```

The keys of the `[Ilia Page]` group:

- `Name` (required): the label of the page's tab, as plain text. `Name[<lang>]` gives a translation.
- `Icon`: the icon of the tab and of the text rows (`applications-other` by default).
- `Key`: a letter that selects the page with Alt when all pages are shown; a letter another page already uses is ignored.
- Exactly one of:
  - `Apps`: desktop file IDs, listed in the order given. Choosing one launches it as the Applications page does; entries that are not installed or not shown on this desktop are left out.
  - `Items`: lines of text, separated by `;`.
  - `ItemsCommand`: a shell command (`/bin/sh -c`), each line it prints is a row. It runs when the page is first shown, rows appear as they are printed, and it is stopped, with its process group, when ilia closes.
- `Exec`: for `Items` and `ItemsCommand`, the command that choosing a row runs, with each `%s` replaced by the row's value as one argument (`%%` is a `%`; without `%s` the value is added as the last argument). The command is not run by a shell; it starts in its own `systemd-run --user --scope` unit when `systemd-run` is available. Without `Exec`, the value is printed, as with `textlist`.
- `Sort=frequency`: for `Items` and `ItemsCommand`, the rows chosen most often come first. Choices are recorded as `<name>#<value>` in the `app-launch-counts` setting, the launch history the Applications page ranks by, which keeps the latest 32 launches.
- `OnlyShowIn`, `NotShowIn`: window managers the page is or is not shown under: `i3`, `sway`, or `other` for any other.

A row of `Items` or `ItemsCommand` may be `label<tab>value`: the label is shown and searched, and choosing the row uses the value (in a key file, a tab is written `\t`).
Rows are matched as on the other pages, following the `fuzzy-matching` setting; rows of `Apps` are matched as on the Applications page, following `match-fields` too, but keep their order.
Files that cannot be used are reported on standard error and skipped.

The window manager is sway when `SWAYSOCK` is set to something, else i3 when `I3SOCK` is.
The `keybindings` and `windows` pages need i3 or sway: under other window managers they are not shown, and `ilia -p keybindings` exits with an error.

### Configuration

Ilia supports the following configuration via gsettings under the namespace `org.regolith-linux.ilia`:

- window-width: width of window in pixels
- window-height: height of window in pixels
- icon-size: size of icons in pixels or 0 to disable icons
- fuzzy-matching: also list rows whose main text holds the characters of
  each typed word in order, after the other matches (default `false`)
- match-fields: application fields searched besides the name, any of
  `generic-name`, `keywords` and `comment` (default: all three)
- hidden-apps: desktop ids, or patterns with `*` and `?`, of applications
  never listed; an entry starting with `!` lists again what it matches.
  Ctrl+Delete on the Applications page adds the highlighted app, Ctrl+z
  takes it out again (default `[]`)

Changes to these three keys apply while ilia runs.

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
