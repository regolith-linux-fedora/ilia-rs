# Ilia black-box parity suite

Run from the repository root with `python3 -m pytest parity/ -v`. The suite
drives the built executable (`build/src/ilia`, or `ILIA_BINARY`) under a
private Xvfb display. Each test receives isolated HOME, both XDG data roots,
GSettings-memory, PATH, runtime directory, and D-Bus state. The private D-Bus
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
notification first.

The suite records existing behavior, including `ilia version 0.12`, native
little-endian i3 framing, the single-read IPC client, the fixed rofication
socket path, and `systemd-run` wrapping. Sway mode reaches the fake window
manager through `SWAYSOCK` alone; its `I3SOCK` names a socket that does not
exist, as a stale variable from another session would (upstream #141).

This harness chooses the deterministic absent-binary behavior for the absolute
`/usr/bin/i3-msg` and `/usr/bin/swaymsg` paths rather than modifying system
files. The WM tests require either the exact expected IPC command transcript
(if one of those clients exists) or the corresponding GLib child-process error
that names the absent absolute binary; unrelated stderr cannot satisfy them.
Tracker uses the no-miner judgment call and checks nonempty, distribution-
dependent D-Bus stderr while proving that the UI remains responsive.

The rofication path is a process-global upstream constant. Its fixture holds a
module-level lock for the server's whole lifetime, accepts the initial and
post-delete `list` connections, and removes the socket at teardown. Tests are
intentionally serial (no xdist). All created regular files and transcripts are
inside pytest temporary directories; `/tmp/rofi_notification_daemon` is the
only required fixed-path exception and is removed after every test.
