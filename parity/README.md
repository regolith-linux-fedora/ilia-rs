# Ilia black-box parity suite

Run from the repository root with `python3 -m pytest parity/ -v`. The suite
drives the built Vala executable (`build/src/ilia`, or `ILIA_BINARY`) under a
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

Unmatched Enter is deliberately pinned as a no-op in both TextList and
Terminal: after a bounded settling interval it has emitted or executed
nothing and the UI is still running; Escape then exits successfully. The
settling interval prevents a delayed launch from racing the no-op assertion.
The corresponding free-text branches in the page classes are unreachable
through the current dialog event routing.

The suite records existing behavior, including `ilia version 0.12`, native
little-endian i3 framing, the single-read IPC client, the fixed rofication
socket path, and `systemd-run` wrapping. Sway mode still needs `I3SOCK` for the
IPC transport because the current client reads that variable even when
`SWAYSOCK` selects sway behavior.

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
