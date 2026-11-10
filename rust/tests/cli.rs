use std::process::{Command, Output};

const HELP: &[u8] = b"Usage: ilia [-t stylesheet] [-n] [-a] [-p page]\n\n\t-t: specify path to custom stylesheet.\n\n\t-n: no custom styles\n\n\t-a: load all pages\n\npages:\n\t'apps' - launch desktop applications (default)\n\t'terminal' - launch a terminal command\n\t\t-q: quiet execution\n\t'notifications' - launch notifications manager\n\t'keybindings' - launch keybindings viewer\n\t'textlist' - select an item from a specified list\n\t\t-l: page label\n\t\t-i: page/item icon\n\t\t-n: no icon\n\t'windows' - navigate to a window\n\t'tracker' - search for files by content\n";
const VERSION: &[u8] = b"ilia version 0.19\n";
const EMPTY: &[u8] = b"";

fn invoke(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ilia"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("GDK_BACKEND")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_SESSION_TYPE")
        .output()
        .expect("ilia should execute")
}

fn observable_result(output: &Output) -> (Option<i32>, &[u8], &[u8]) {
    (output.status.code(), &output.stdout, &output.stderr)
}

#[test]
fn short_help_succeeds_without_display_or_dbus() {
    let output = invoke(&["-h"]);
    assert_eq!(observable_result(&output), (Some(0), HELP, EMPTY));
}

#[test]
fn long_help_succeeds_without_display_or_dbus() {
    let output = invoke(&["--help"]);
    assert_eq!(observable_result(&output), (Some(0), HELP, EMPTY));
}

#[test]
fn short_version_succeeds_without_display_or_dbus() {
    let output = invoke(&["-v"]);
    assert_eq!(observable_result(&output), (Some(0), VERSION, EMPTY));
}

#[test]
fn long_version_succeeds_without_display_or_dbus() {
    let output = invoke(&["--version"]);
    assert_eq!(observable_result(&output), (Some(0), VERSION, EMPTY));
}

#[test]
fn help_precedes_version() {
    let output = invoke(&["-h", "-v"]);
    assert_eq!(observable_result(&output), (Some(0), HELP, EMPTY));
}
