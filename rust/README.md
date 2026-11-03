# Ilia Rust compositor pages

Build from this directory with `cargo build --release --locked`.
The binary is `target/release/ilia`. Cargo
resolves the locked dependencies from crates.io.

This step retains the earlier shell and the Applications page. It adds
the i3/Sway IPC client, compositor config and tree decoding, keybinding parsing,
real Keybindings and Windows tree views, WM command execution, and native
Wayland layer-shell startup. Terminal, Notifications, TextList, and Tracker
remain deferred stubs. `src/runner.rs` still contains the Vala-compatible
`systemd-run` wrapper and escaping rules.

Validation against the parity harness:

- `cargo fmt --check`: passed.
- `cargo test --locked`: 26 unit and 5 CLI integration tests passed.
- `cargo build --release --locked`: passed without warnings.
- Keybindings + Windows + CLI parity subsets: 9 passed, 0 failed.
- Apps + CLI regression subsets: 6 passed, 0 failed.
- `smoke-bootstrap.sh`: passed every launch/shutdown case, including isolated
  headless Keybindings and Windows starts with no WM socket configured and the
  pinned graceful load-failure diagnostics.
- Full parity suite: 14 passed, 9 failed. Keybindings, Windows, Apps, and CLI
  had zero failures. The remaining failures are exclusively the deferred
  Notifications (3), Terminal (2), TextList (3), and Tracker (1) behavior.
- The generic AT-SPI snapshot helper successfully observed the running Apps
  page (`application ilia`) and emitted a bounded accessibility tree.
- The untouched Vala reference compiled, passed its Meson test, and passed all
  23 parity probes.

History overflow retains 32 entries: the oldest entry is discarded and the
newly launched desktop ID remains last. One intentional safety
limitation still differs from the pinned Vala build: `icon-size=0` omits icon
loading and keeps stderr clean instead of constructing Vala's invalid 0x0
blank pixbuf.

Native Wayland now checks compositor support, initializes the GTK window as an
overlay layer-shell surface, requests exclusive keyboard input, and skips the
X11 seat grab. An unsupported compositor exits with the pinned Vala message.
