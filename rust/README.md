# Ilia Rust foundation

Build from this directory with `cargo build --release --locked`. The binary
is `target/release/ilia`. `src/args.rs` implements the Vala argument grammar,
`src/application.rs` owns application startup and styling, `src/dialog.rs`
builds the shell, and `src/pages.rs` contains replaceable page stubs.

Run the CLI parity tests from the repository root with
`ILIA_BINARY=$PWD/rust/target/release/ilia python3 -m pytest parity/test_cli.py -v`.
Run `rust/smoke-bootstrap.sh` after the release build for the headless startup
and shutdown checks. It requires Xvfb, xdotool, and the installed Ilia schema.

The foundation deliberately exits on native Wayland because layer-shell is
deferred. The full parity suite therefore still reports page-model and action
failures; CLI, X11 startup, page selection, and shutdown are implemented here.

Reference run against the parity suite: 7 tests passed and 16 failed.
Every failure was in deferred page behavior: application launching, terminal
execution, text-list selection, notification and WM IPC, or Tracker lookup.
The CLI subset passed 3/3 and the bootstrap smoke probe passed every launch and
shutdown case.
