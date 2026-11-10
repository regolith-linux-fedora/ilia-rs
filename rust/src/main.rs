mod application;
mod apps;
mod args;
mod dialog;
mod filter;
mod icons;
mod input_sources;
mod ipc;
mod keybindings;
mod notifications;
mod pages;
mod rofication;
mod runner;
mod sparql;
mod terminal;
mod textlist;
mod tracker;
mod windows;

use std::ffi::OsString;

use args::Args;

const HELP: &str = "Usage: ilia [-t stylesheet] [-n] [-a] [-p page]\n\n\t-t: specify path to custom stylesheet.\n\n\t-n: no custom styles\n\n\t-a: load all pages\n\npages:\n\t'apps' - launch desktop applications (default)\n\t'terminal' - launch a terminal command\n\t\t-q: quiet execution\n\t'notifications' - launch notifications manager\n\t'keybindings' - launch keybindings viewer\n\t'textlist' - select an item from a specified list\n\t\t-l: page label\n\t\t-i: page/item icon\n\t\t-n: no icon\n\t'windows' - navigate to a window\n\t'tracker' - search for files by content\n";
const VERSION: &str = "ilia version 0.19\n";

fn early_output(args: &Args) -> Option<&'static str> {
    if args.has("-h") || args.has("--help") {
        Some(HELP)
    } else if args.has("-v") || args.has("--version") {
        Some(VERSION)
    } else {
        None
    }
}

/// Arguments need not be UTF-8 (a label taken from a file name, say), so
/// they are decoded lossily instead of refusing to start.
fn parse_args(raw: impl IntoIterator<Item = OsString>) -> Args {
    Args::parse(
        raw.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    )
}

fn main() {
    let args = parse_args(std::env::args_os());
    if let Some(output) = early_output(&args) {
        print!("{output}");
        return;
    }
    std::process::exit(application::run(args));
}

#[cfg(test)]
mod tests {
    use super::{early_output, parse_args, Args, HELP, VERSION};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn short_help_selects_exact_help_output() {
        assert_eq!(early_output(&Args::parse(["ilia", "-h"])), Some(HELP));
    }

    #[test]
    fn long_help_selects_exact_help_output() {
        assert_eq!(early_output(&Args::parse(["ilia", "--help"])), Some(HELP),);
    }

    #[test]
    fn short_version_selects_exact_version_output() {
        assert_eq!(early_output(&Args::parse(["ilia", "-v"])), Some(VERSION),);
    }

    #[test]
    fn long_version_selects_exact_version_output() {
        assert_eq!(
            early_output(&Args::parse(["ilia", "--version"])),
            Some(VERSION),
        );
    }

    #[test]
    fn help_takes_precedence_over_version() {
        assert_eq!(early_output(&Args::parse(["ilia", "-v", "-h"])), Some(HELP),);
    }

    #[test]
    fn non_utf8_arguments_are_decoded_lossily() {
        let args = parse_args([
            OsString::from("ilia"),
            OsString::from("-l"),
            OsString::from_vec(b"lab\xe9l".to_vec()),
            OsString::from("-p"),
            OsString::from("textlist"),
        ]);
        assert_eq!(args.value("-l"), Some("lab\u{fffd}l"));
        assert_eq!(args.value("-p"), Some("textlist"));
        assert_eq!(early_output(&args), None);
    }

    #[test]
    fn ordinary_arguments_do_not_exit_before_gtk_startup() {
        assert_eq!(early_output(&Args::parse(["ilia", "-p", "apps"])), None);
    }
}
