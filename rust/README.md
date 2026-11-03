# Ilia Rust applications page

Build from this directory with `cargo build --release --locked`.
The binary is `target/release/ilia`. Cargo
resolves the locked dependencies from crates.io.

This step retains the earlier CLI, GTK dialog shell, Help surface, and six
deferred page stubs. It replaces only the Applications stub with desktop-entry
discovery, filtering, history-ranked sorting, selection, asynchronous icon
loading, and launch lifecycle behavior. `src/runner.rs` contains the
Vala-compatible `systemd-run` wrapper and escaping rules.

Validation against the parity harness:

- `cargo fmt --check`: passed.
- `cargo test --locked`: 20 passed.
- `cargo build --release --locked`: passed without warnings.
- Apps + CLI parity subsets: 6 passed, 0 failed.
- `smoke-bootstrap.sh`: passed every launch/shutdown case.
- Full parity suite: 8 passed, 15 failed. Apps and CLI had zero failures. The
  remaining failures are exclusively the deferred keybindings (3),
  notifications (3), terminal (2), textlist (3), tracker (1), and windows (3)
  page behaviors. The unmatched-enter no-op probes for terminal and textlist
  already pass through the shared dialog shell.
- The generic AT-SPI snapshot helper successfully observed the running Apps
  page (`application ilia`) and emitted a bounded accessibility tree.
- The untouched Vala reference compiled, passed its Meson test, and passed all
  23 parity probes.

History overflow retains 32 entries: the oldest entry is discarded and the
newly launched desktop ID remains last. Two intentional safety limitations
differ from the pinned Vala build: native Wayland exits while layer-shell
support is deferred, and `icon-size=0` omits icon loading and keeps stderr
clean instead of constructing Vala's invalid 0x0 blank pixbuf.
