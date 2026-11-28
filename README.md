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
  All of those merged by 2026-11-09 are in this port, fixed here
  independently or ported, and so is #175 (help as a page, merged on
  2026-11-10); the last large one was window-manager and
  desktop keys while ilia is open (#171). See
  [Differences from upstream](#differences-from-upstream). That list was
  made against upstream `main` at `b075ac2`, and checked by running both
  builds side by side under i3 and sway.
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
switching, Alt+plus/Alt+minus resizing with persistence, the Help page,
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

Then the open upstream pull request #172 (the calculator page) in one
commit.

Then the rest of #171 (window-manager and desktop keys while ilia is open)
in one commit.

Then the open upstream pull request #180 (favourite applications, and
Ctrl+Enter on the next free workspace) in one commit.

Then the open upstream pull request #190 (sway's included files read the
way sway reads them) in one commit.

Then #175 (help as a page of its own that scrolls, in single-page and
all-pages mode) in one commit.

Then the open upstream pull request #187 (menu pages with submenus,
binding and command rows) in one commit.

Then the open upstream pull request #188 (Wi-Fi, Bluetooth and power
profile pages) in one commit.

Then the open upstream pull request #179 (desktop-entry actions on the
Applications page; it replaces #173) in one commit.

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
once, the best match for the text typed when it was pressed; #171
window-manager and desktop keys while ilia is open: on X11 a key ilia does
not use itself closes ilia when i3 binds the key in its current mode, and
i3 then runs the binding (matched as i3 matches it: layouts, Shift
fallback, `bindcode`, exact modifiers, `--release`); the desktop's Print,
volume, mute, brightness, calculator and playback keys reach the program
that grabs them, with their modifiers; on sway ilia closes when sway runs
a binding, except one that only switches the keyboard layout; an unusable
window-manager socket is reported once on standard error; #175 (merged on
2026-11-10, after `b075ac2`) help is a page like the others: the last tab
with `-p` and with `-a`, with the `help-browser` icon and Alt+h (no letter
when a page from a file has `h`), describing the page in view before it
was opened: that page's name as plain text, its help text, and its keys
ordered by key, then the keys every page shares (Page Up/Page Down and
← → among them), the Alt+letter of each tab, and Alt -, Alt + and Esc, in
a list that scrolls in the room the text leaves, so that Alt - shrinks the
dialog while help is shown. The port differs from that pull request in a
few places: a name or help text too long for the room scrolls in its own
region above the Keys heading; the window opens wide enough for the whole
row of tabs, and once Alt - or Alt + has changed its size the tabs scroll
rather than hold it that wide (the Vala notebook does not scroll its
tabs); on the Calculator page Left and Right switch pages from either end
of the text when nothing is selected; on Help the entry's Ctrl keys do
nothing, so Ctrl+c leaves the clipboard alone, and Ctrl+f opens no search box over the list (GTK's tree view opens
one for that key even with searching off); under X11 the keys the
described page keeps from i3 stay with ilia on Help too, doing nothing;
and a `-l` label that is not valid markup shows as typed, on its tab as
well (GTK leaves a label given broken markup empty).

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
closes; #172 a Calculator page (`-p calculator`, the last tab with `-a`,
Alt+l) that works out the typed expression with libqalculate in a helper
process as it is typed, shows `Too long` after three seconds, prints and
copies the answer with Enter, and keeps it pasteable after the window
closes (see [Calculator](#calculator)). The port differs from the Vala pull
request in a few places: the expression goes to the helper on standard
input rather than as an argument, the helper is installed as
`libexecdir/ilia-calculator` and always built (no `calculator` meson
option), nesting is refused beyond 1000 levels (Vala: 100), the helper has
no 10 s alarm of its own (ilia kills it after three seconds, and it dies
with ilia), a part with no value (`0*(1.5!)`, `(-3)!^0`) leaves the whole
expression without one, and with `-a` each page keeps its own entry text
(see below); #180 favourite applications on the Applications page
(`favorite-apps`): Ctrl+f pins the highlighted app at the end of the list
or unpins it, pinned apps come first in their stored order and lead each
group of matches when text is typed, a ★ cell follows their names, Alt+Up
and Alt+Down move a pinned app past the pinned app shown above or below it
while the filter is empty, and changes another program makes apply while
ilia runs; Ctrl+Enter (or Ctrl with the keypad Enter) switches the window
manager to the next free workspace, named by its `wm.workspace.NN.name`
resource, and starts the app there once the switch is confirmed (see
[Desktop Apps](#desktop-apps)). The port differs from that pull request in
a few places: a switch that fails starts nothing and leaves the window open
(the Vala change starts the app on the current workspace); Ctrl+Enter
pressed before the list arrives acts once it has, as Enter does; Ctrl+f and
Ctrl+Enter do nothing from the Help page; under X11 the resource is read
from the root window's `RESOURCE_MANAGER` as it is at the key press and
decoded as i3 decodes it, so a backslash that starts no escape stays (the
Vala change takes Xlib's copy from when the display was opened, decoded by
Xlib); and under sway a name holding both quote kinds, a backslash before a
quote, or `$` reaches sway verbatim (sway expands variables in commands);
#190 under sway the Keybindings page reads the files the configuration
includes the way sway reads them: in every file a line ending in a
backslash goes on with the next (so a binding written over several lines
runs whole), `include` and `set` are matched in any letter case, `set`
variables take the value they have at that line, and the include's
argument then goes through the word expansion sway gives it (glibc's
`wordexp`, short of running anything): `~`, environment variables (an
unset one is empty, an unquoted value splits into several paths), quotes
and backslashes, wildcards whose matches come sorted and a pattern without
matches standing for itself, and shell syntax such as `(`, `|` or `{a,b}`
reading nothing; relative paths start beside the main file as sway reports
it (not beside a link's target) and beside an included file's real path,
and each file is read once. The port goes further than that pull request,
which leaves quotes, backslashes and `${NAME:-word}`-style forms alone: it
follows glibc in those too, including the rule that a word only becomes a
pattern at a wildcard written unquoted; #187 menu pages: a page file with
`Entries` lists a tree of rows that open submenus, run one of the window
manager's bindings (named by its description on the Keybindings page,
showing the same key text) or run a command, with `When` and `State`
commands, BackSpace and Alt+Left back up, search through every level below
the open one, and `-p <page>/<id>[/<id>...]` opening a submenu (see
[Menus](#menus)). The port differs from that pull request in a few places:
a `State` command runs again about 5 seconds after each run ends while its
row is listed on the tab in view, so a row's `On`/`Off` follows a change
within about 7 seconds (the Vala change runs it once); a small guardian
process ends the `When` and `State` commands still running, with their
process groups, also when ilia is killed by a signal it cannot catch, and
ilia catches the usual terminating signals to stop them before it ends as
the signal would have ended it; search follows the `fuzzy-matching`
setting; a click acts only when the button goes down and comes up on the
same entry, even if rows arrive in between (the Vala change needs the row
in the same place); an `Exec` value that cannot be split into words keeps
its row, which does nothing when chosen, with a warning (the Vala change
leaves the entry out); `-p unknown/<path>` keeps the usual messages for an
unknown page, `Unknown page type: unknown` (the Vala change reports that
the page has no menu); #188 Wi-Fi, Bluetooth and power profile pages
(`-p wifi`, `-p bluetooth`, `-p power`, not part of `-a`) that list what
NetworkManager, BlueZ and power-profiles-daemon report on the system bus,
follow their changes while open and act on Enter or a click (see
[Wi-Fi, Bluetooth and Power](#wi-fi-bluetooth-and-power)). The port
differs from that pull request in a few places: an OWE ("Enhanced Open")
network, and the open network of an OWE transition pair, is joined without
a passphrase, with `key-mgmt` `owe`, and Enter on a WEP network reports
`WEP is not supported` (the Vala change asks both for a WPA passphrase); a
WPA3-Enterprise 192-bit network counts as Enterprise; the pages follow
their service's bus name, so a daemon that starts, stops or restarts while
the page is open is read again; rows appear as each answer arrives, so an
object that never answers holds back only its own row, and what a signal
announces is applied as it comes (the Vala change reads every object
again, one after the other, after each signal and shows the rows once all
have answered); an error line is always one line, and never shows the
passphrase typed, even when the service repeats it; discovery that the
page started is also stopped when ilia closes for a suspend or a screen
lock; and a dialog that takes the focus while an action waits (a polkit or
pairing agent's) leaves the window open; #179 (which replaces #173)
desktop-entry actions on the Applications page: the row of an app whose
entry declares actions (its `Actions` key and `[Desktop Action <id>]`
groups) ends with `›`, which assistive technologies read as expandable,
and as expanded while the actions are shown; Right shows them beneath the
app in `Actions` order, Right again highlights the first, Left closes
them, and a move off them or a new filter text closes them too; with text
typed, an action is listed on its own as `App: Action` when a word of the
filter occurs in the action's name but not in the app's, ranked among the
apps; a filter with a colon lists the actions, whose names hold every word
after it, of the apps the text before it finds; Enter (or a double click
under X11) starts an action as its app starts, in the app's `Path`, its
terminal and its scope, or with `ActivateAction` over the session bus for
a D-Bus-activated app; each action has a launch count of its own, and
Ctrl+Delete hides one action, both under `<desktop id>#<action id>`; a
click on `›` under X11 opens or closes the actions (see
[Desktop Apps](#desktop-apps)). The port differs from that pull request in
a few places: while an app's actions are shown, typed characters narrow
them to those whose names hold every typed word, and Backspace widens them
again until that text is empty (the Vala change edits the filter, which
closes them); Alt+Enter shows the actions as Right does and never starts
anything (not in the Vala change); names follow the first of `LANGUAGE`,
`LC_ALL`, `LC_MESSAGES` and `LANG` that is set, in the Desktop Entry
specification's key order, and an action whose translated name is empty
is left out (the Vala change uses GLib's lookup); the `App: ` part is
drawn halfway between the text and background colours of a plain row (the
Vala change uses the theme's insensitive colour), and action rows are set
in 16 pixels (Vala: 24); Ctrl+z brings back a hidden action as it does an
app; and, with #180, Ctrl+Enter starts the highlighted action on the next
free workspace, an action's own row is never lifted with its pinned app,
and Ctrl+f, Alt+Up and Alt+Down do nothing on an action's row.

**Not yet ported** (open items):

- Open upstream PRs that will likely land: #164 (notification actions).
  #172, #174, #179 (which replaces #173), #180, #181, #184, #187, #188 and
  #190 are already in the port (see above).

**Dialog details brought in line with ilia** (port bugs, no upstream pull
request): tab icons are flat buttons that switch the page; the Help page's
headings have the `help_heading` style class and its list fixed row heights;
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
icon, found as GLib's search ranks entries. With #172: with `-a` each page
keeps its own entry text, so switching pages brings back the text last
typed on the new page (empty at first) instead of filtering it by the
text typed on the old one, as #145 does upstream; the Calculator page keeps
its expression, and its answer, while another page is shown. With #171:
on X11 the keys a page acts on beyond the dialog's own stay with ilia even
when i3 binds them (on the Applications page Ctrl+Shift+C, Ctrl+= and
Ctrl+z, and Tab in its form; upstream's #181 and #184 leave them to i3),
and i3's key names are read with libxkbcommon as i3 reads them, so a
binding written with a hexadecimal or Unicode key name (`0x1008ff13`,
`U00E9`) is matched too (upstream reads them with GDK, which knows neither).

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
python3-gi with the AT-SPI typelib, and `dbus-daemon`; the Calculator
cases also need xclip and the helper meson builds beside the binary
(`build/src/ilia-calculator`). The Windows parity
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
(the binary, the GSettings schema, and the `data/local` and `doc` copies),
plus the Calculator page's helper, `ilia-calculator`, in meson's
`libexecdir` (`/usr/lib/<triplet>` with this debhelper compat level).
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
- Calculator (`calculator`)
- Wi-Fi Networks (`wifi`)
- Bluetooth Devices (`bluetooth`)
- Power Profiles (`power`)
- Pages defined in files: favourite apps, fixed lines, the lines a command prints, or a menu of actions (see [Pages from files](#pages-from-files))

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

Ctrl+f pins the highlighted application, or unpins it; pinned applications have a ★ after their name.
With an empty filter they come first, in the order of the `favorite-apps` setting, and the other applications follow by launch count.
With text typed, the groups of matches keep their order and pinned applications lead within each group, so an application whose name starts with the text still comes before a pinned one found through a keyword.
While the filter is empty, Alt+Up moves the highlighted pinned application before the pinned one shown above it, and Alt+Down after the one shown below it.
Changes another program makes to `favorite-apps` (`gsettings set`, dconf-editor) show at once.

Ctrl+Enter (or Ctrl with the keypad Enter) opens the highlighted application on the next free workspace: the lowest positive number no workspace has, where a name such as `2:web` takes 2.
The workspace is named by the resource `wm.workspace.NN.name` (`NN` the number with at least two digits) when it is set and not empty, read from the X resource database under i3 and from trawl under sway, and is the number otherwise.
ilia switches to the workspace, waits for the window manager to confirm, and then starts the application as Enter does.

Many desktop entries declare actions besides starting the application, such as a browser's private window or a mail client's new message.
An application with actions has `›` at the end of its row.
Right (or Alt+Enter) on that row shows its actions beneath it, in the entry's order; Right again highlights the first one, and Left closes them, as do moving the highlight off them and changing the filter.
While they are shown, typing narrows them to the actions whose names hold every typed word, without touching the filter, and Backspace widens them again.
Under X11 a click on `›` opens or closes them, and a double click on an action starts it.
With text typed, a word of the application's name and one of the action's list the action on its own, as `Application: Action`; the application's name alone lists only the application.
A colon picks actions directly: the text before it finds the applications, the text after it their actions (`term:pref`, or `:private` for every application).
Enter starts an action the way its application starts, in the same directory, terminal and scope, or over the session bus for an application started there.
Ctrl+Delete hides the highlighted action and Ctrl+z brings it back; each action keeps its own launch count.

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

#### Calculator

`ilia -p calculator` works out the expression typed in the entry while it is typed, and shows the answer on the line below (style class `calculator_answer`).
Expressions use numbers (`1.5`, `1,5`, `3e7`), `+ - * / ^`, `!` after an operand, brackets, `sqrt`, `sin`, `cos`, `tan` (radians), `ln`, `exp`, `abs`, `pi` and `e`; `^` groups to the right and `-3^2` is `-9`.
Text outside that, units and names included, and expressions with no real, finite value (`5/0`, `sqrt(-4)`, `(-3)!`) leave the line empty.
Answers are rounded to 15 significant digits, whole numbers up to 40 digits are shown in full, values below `0.000001` or from `1e40` are shown as `1.5e-7`, and the decimal mark is the locale's (`LC_NUMERIC`, or `LC_ALL`).
An answer not worked out within three seconds shows `Too long`.
Enter prints the answer on standard output, copies it to the clipboard and closes the window; pressed while the answer is still being worked out, it waits for it.
The answer stays pasteable after ilia closes: an X11 clipboard manager takes it, and without one (always on Wayland) ilia keeps running without a window until something else is copied, so a caller that waits for `ilia -p calculator` to exit waits until then.
On this page Left and Right move the cursor in the entry; they switch pages only from either end of the text, with nothing selected.
The answers come from `ilia-calculator`, a helper built against libqalculate and installed in `libexecdir`.

#### Wi-Fi, Bluetooth and Power

The `wifi`, `bluetooth` and `power` pages are quick panels for the system services behind them, reached on the system bus (`DBUS_SYSTEM_BUS_ADDRESS` when set).
Each is shown on its own with `-p` (it is not one of the pages `-a` shows) and lists one row per network, device or profile, with a detail and a state.
The window opens at once and the rows fill in as the service answers; they follow the service's changes while the page is open, and the highlight stays on the same network, device or profile.
The filter keeps the rows whose name holds every word typed, ignoring case and accents.
Enter, or a click, acts on the highlighted row and closes ilia once the service accepts; if the service returns an error, or no reply within five seconds, ilia prints `ilia: <row>: <message>` on standard error and stays open.

- `wifi` (NetworkManager) lists the networks the Wi-Fi devices see, one row per network name with the strength of its strongest access point, connected ones first, then by signal strength. The state reads `Connected`, `Connecting`, `Saved`, `Open`, `Enterprise` or `Secured`. Enter disconnects a connected network, activates a saved one, joins an open or OWE one, and asks for the passphrase of a WPA/WPA3 personal network in the entry, whose text is then hidden (Escape leaves the prompt). Enterprise (802.1X) and WEP networks are listed but not joined from here. Every Wi-Fi device is asked to scan when the page opens.
- `bluetooth` (BlueZ) lists paired, connected and named devices, with their battery level when known. Enter disconnects a connected device, connects a paired one, and pairs, trusts and connects any other. Discovery runs while the page is open, on the powered adapters that were not already discovering, and is stopped on exactly those when ilia closes.
- `power` (power-profiles-daemon, at `org.freedesktop.UPower.PowerProfiles` or the older `net.hadess.PowerProfiles`) lists the profiles and marks the active one. Enter makes the highlighted profile active.

When a service is missing, or its radio is off, the page says so in a single row; Enter on the "is off" row turns the Wi-Fi radio or the Bluetooth adapters on.
A page file named `wifi`, `bluetooth` or `power` is now skipped with a warning, as for the other built-in names.

#### Help

The last tab is Help, with `ilia -p <page>` as with `ilia -a`. Alt+h opens it, and Left and Right walk the tabs, going round at either end.
It describes the page in view before it was opened: that page's name and help text, then its keys, the keys that work on every page, the Alt+letter of each tab, and Alt -, Alt + and Esc.
The list scrolls in the room the text leaves; the arrows, Page Up/Page Down, Home/End, Ctrl+n/Ctrl+p and Ctrl+j/Ctrl+k move through it, and Alt - and Alt + resize the dialog as on the other pages.
When a page from a file takes `h` as its `Key`, Help has no letter.

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
  - `Entries`: the entries of a menu, see [Menus](#menus).
- `Exec`: for `Items` and `ItemsCommand`, the command that choosing a row runs, with each `%s` replaced by the row's value as one argument (`%%` is a `%`; without `%s` the value is added as the last argument). The command is not run by a shell; it starts in its own `systemd-run --user --scope` unit when `systemd-run` is available. Without `Exec`, the value is printed, as with `textlist`.
- `Sort=frequency`: for `Items` and `ItemsCommand`, the rows chosen most often come first. Choices are recorded as `<name>#<value>` in the `app-launch-counts` setting, the launch history the Applications page ranks by, which keeps the latest 32 launches.
- `OnlyShowIn`, `NotShowIn`: window managers the page is or is not shown under: `i3`, `sway`, or `other` for any other.

A row of `Items` or `ItemsCommand` may be `label<tab>value`: the label is shown and searched, and choosing the row uses the value (in a key file, a tab is written `\t`).
Rows are matched as on the other pages, following the `fuzzy-matching` setting; rows of `Apps` are matched as on the Applications page, following `match-fields` too, but keep their order.
Files that cannot be used are reported on standard error and skipped.

#### Menus

A page with `Entries` is a menu: a short tree of rows that open submenus, run one of the window manager's bindings, or run a command.

```
[Ilia Page]
Name=Actions
Icon=system-run
Key=i
Entries=capture;toggles;system

[Entry capture]
Name=Capture
Entries=area;window

[Entry area]
Name=Screenshot of an area
Binding=Session // Screenshot Selection

[Entry window]
Name=Screenshot of a window
Binding=Window Snapshot

[Entry toggles]
Name=Toggles
Entries=banners

[Entry banners]
Name=Do not disturb
Exec=dunstctl set-paused toggle
State=dunstctl is-paused

[Entry system]
Name=System
Entries=lock;hibernate

[Entry lock]
Name=Lock screen
Binding=Session // Lock Screen

[Entry hibernate]
Name=Hibernate
Exec=systemctl hibernate
When=grep -qw disk /sys/power/state
```

`Entries` lists the ids of the top-level rows, in order (empty pieces between `;` are skipped); `Sort` and `Exec` of the page are not used.
Each id (ASCII letters, digits, `-` and `_`) is described by an `[Entry <id>]` group:

- `Name` (required): the row's text, as plain text.
- `Icon`: the row's icon; the page's icon by default.
- Exactly one of:
  - `Entries`: the ids of a submenu's rows, in order. The row shows `›`.
  - `Binding`: a binding of the window manager, by its description on the Keybindings page: `Action` or `Category // Action`, ignoring letter case and the spaces around `//` and at the ends. The row shows the key text the Keybindings page shows for it, and choosing it sends the binding's command to the window manager, as choosing it on the Keybindings page does; if several bindings have that description, the one the Keybindings page lists first. The row is hidden when no binding with a command has the description, and under window managers other than i3 and sway.
  - `Exec`: a command, split into words as a shell would do it but not run by a shell; `%%` is a `%`. It runs as `Exec` of a page does, in its own `systemd-run --user --scope` unit when `systemd-run` is available.
- `When`: a command; the row is shown only once it has exited with status 0.
- `State`: a command; the first line it prints, `on`, `true` or `1`, or `off`, `false` or `0` (trimmed, in any letter case), shows `On` or `Off` in the row.
- `OnlyShowIn`, `NotShowIn`: as for a page.

A submenu is shown only if some row below it is shown.
Binding rows are matched before the page is first displayed, so they never arrive late.
When the page is first shown (at start-up, or with `-a` when its tab is first selected) all `When` and `State` commands of the tree start, directly (no shell), with standard input from `/dev/null`, each in a process group of its own.
ilia does not wait for them: rows appear, and `On` or `Off` is filled in, as they finish, and the highlighted row stays highlighted.
A command still running after 2 seconds is stopped with its process group, which leaves its row hidden (`When`) or its column as it was (`State`).
`When` runs once; `State` runs again about 5 seconds after each run ends, for as long as its row is listed on the tab in view.
The commands still running when ilia ends are stopped too, however it ends.

Enter or a click opens a submenu or runs the row. BackSpace, with nothing typed, and Alt+Left go back up, to the row of the submenu that was open.
Typing searches every row below the open submenu: a row deeper down is shown with its path, such as `Capture › Screenshot of an area`, and matches when every typed word occurs in that text, compared as on the other pages (ignoring letter case and accents, and following `fuzzy-matching`).

`ilia -p <page>/<id>[/<id>...]` opens the menu at a submenu, for example `ilia -p actions/system` from a key binding or a script; `ilia -a -p actions/system` does the same on the menu's tab.
The ids must be exact, each a submenu of the one before, shown under the current window manager; `When` and bindings are not checked, so such a submenu may open empty.
For any other path to a page that exists, ilia prints `ilia: page '<page>' has no menu '<path>'` and exits with status 1.

An entry that cannot be used (no group, no `Name` or a blank one, none or several of `Entries`, `Binding` and `Exec`, or an id with other characters) is left out with a warning on standard error, and the rest of the menu works.
An `Exec` that cannot be split into words is reported too; its row does nothing.
An id listed twice anywhere in the file, or a submenu that leads back to itself, makes the whole file unusable.

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
  never listed, or `<desktop id>#<action id>` for one action of an
  application; an entry starting with `!` lists again what it matches.
  Ctrl+Delete on the Applications page adds the highlighted app or action,
  Ctrl+z takes it out again (default `[]`)
- favorite-apps: desktop ids of the applications pinned at the head of the
  Applications page, in their order. Ctrl+f adds or removes the highlighted
  app, Alt+Up and Alt+Down move it (default `[]`)

Changes to these four keys apply while ilia runs.

#### Example: Resetting screen width

```
gsettings reset org.regolith-linux.ilia window-width
```

### Dependencies

#### Notifications

This page communicates with the [Rofication](https://github.com/regolith-linux/regolith-rofication) daemon for managing desktop notifications.

#### Wi-Fi, Bluetooth and Power

These pages talk to [NetworkManager](https://networkmanager.dev/), [BlueZ](https://www.bluez.org/) and [power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon) over the system bus with GIO; none of them is needed to build or run ilia. Pairing a device that asks for a PIN or a passkey needs a Bluetooth agent from the desktop.

#### Keybindings

This page uses the comment format specified by [Remontoire](https://github.com/regolith-linux/remontoire) and reads the i3 config via it's local socket.

## License

Apache License 2.0, as the original program; see [LICENSE](LICENSE).
ilia was written by Ken Gilmer and the Regolith contributors.
