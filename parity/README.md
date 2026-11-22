# Ilia black-box parity suite

Run from the repository root with `python3 -m pytest parity/ -v`. The suite
drives the built executable (`build/src/ilia`, or `ILIA_BINARY`) under a
private Xvfb display. Each test receives isolated HOME, both XDG data roots,
GSettings-memory, PATH, runtime directory, and D-Bus state; the private bus
also stands in for the system bus, where ilia watches for a suspend. The private D-Bus
configuration deliberately has no service directories, so Tracker cannot be
activated behind the test's back. No network or host desktop service is used.

Every fake produces a file-backed transcript in that test's pytest temporary
directory. The i3/sway and rofication transcripts are JSON Lines in receipt
order; executable stubs append their argv to plain-text logs. Assertions read
those files rather than treating the fake server's in-memory request list as
the oracle. The fixed fixtures live under `parity/fixtures/`: desktop entries,
the i3 config reply, the sway base config and include partial, the complete WM
tree, the textlist input, and the rofication notification list.

The suite was written against ilia 0.18.0 (Vala) and is kept as the
behaviour contract of the Rust port. Where the port has since fixed a bug on
purpose, the case expects the fixed behaviour: Enter on text that matches no
row prints it (TextList) or runs it (Terminal), and the notification fixture
carries rofication `timestamp` fields because the page lists the newest
notification first. An Applications entry starts with its `%c`, `%i` and
`%k` field codes filled in as literal arguments, in the entry's `Path`
(upstream #168).

The suite records existing behavior, including `ilia version 0.19`, native
little-endian i3 framing, the single-read IPC client, the fixed rofication
socket path, and `systemd-run` wrapping. Sway mode reaches the fake window
manager through `SWAYSOCK` alone; its `I3SOCK` names a socket that does not
exist, as a stale variable from another session would (upstream #141).

The Keybindings tests require the chosen binding as exactly one RUN_COMMAND
request on the window manager's socket, carrying the command as the window
manager holds it (upstream #158–#161), and nothing on standard output. The
fake answers sway's GET_VERSION request with numbers, as sway does, so the
main file's relative includes are read beside it. A row's key text is the
key it is bound to once a resource retuned it, and its comment's text while
it is bound as with no resource set (upstream #170); the cases see it
through the filter, which matches that text. A comment before a `mode`
block and a row bound only with `bindcode` run their bindings too.

`test_entries_read_early.py` checks that the entries read during
start-up (upstream #162, read outside GLib in the port) keep what GLib's own
entries give: `%k` is the desktop file, a `DBusActivatable=true` entry is
activated over the private session bus (`fakes/bus_app.py` records the
call) and its Exec line does not run, and the Windows page shows the icon
of the entry GLib's search ranks first for a hyphenated app name of several
words (pixels of the window, read through GDK). Each case runs with a fake
i3 and a fake sway, alone on its page and with `-a`; GDK uses X11 here, so
the sway windows carry a window type.

For the Windows page this harness chooses the deterministic absent-binary behavior for the absolute
`/usr/bin/i3-msg` and `/usr/bin/swaymsg` paths rather than modifying system
files. The Windows tests require either the exact expected IPC command
transcript (if one of those clients exists) or the corresponding GLib
child-process error that names the absent absolute binary; unrelated stderr
cannot satisfy them.
Tracker uses the no-miner judgment call and checks nonempty, distribution-
dependent D-Bus stderr while proving that the UI remains responsive.

The rofication path is a process-global upstream constant. Its fixture holds a
module-level lock for the server's whole lifetime, accepts the initial and
post-delete `list` connections, and removes the socket at teardown. Tests are
intentionally serial (no xdist). All created regular files and transcripts are
inside pytest temporary directories; `/tmp/rofi_notification_daemon` is the
only required fixed-path exception and is removed after every test.
