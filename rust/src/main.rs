mod application;
mod args;
mod dialog;
mod pages;

use args::Args;

const HELP: &str = "Usage: ilia [-t stylesheet] [-n] [-a] [-p page]\n\n\t-t: specify path to custom stylesheet.\n\n\t-n: no custom styles\n\n\t-a: load all pages\n\npages:\n\t'apps' - launch desktop applications (default)\n\t'terminal' - launch a terminal command\n\t\t-q: quiet execution\n\t'notifications' - launch notifications manager\n\t'keybindings' - launch keybindings viewer\n\t'textlist' - select an item from a specified list\n\t\t-l: page label\n\t\t-i: page/item icon\n\t\t-n: no icon\n\t'windows' - navigate to a window\n\t'tracker' - search for files by content\n";
const VERSION: &str = "ilia version 0.12\n";

fn early_output(args: &Args) -> Option<&'static str> {
    if args.has("-h") || args.has("--help") {
        Some(HELP)
    } else if args.has("-v") || args.has("--version") {
        Some(VERSION)
    } else {
        None
    }
}

fn main() {
    let args = Args::parse(std::env::args());
    if let Some(output) = early_output(&args) {
        print!("{output}");
        return;
    }
    std::process::exit(application::run(args));
}

#[cfg(test)]
mod tests {
    use super::{early_output, Args, HELP, VERSION};

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
    fn ordinary_arguments_do_not_exit_before_gtk_startup() {
        assert_eq!(early_output(&Args::parse(["ilia", "-p", "apps"])), None);
    }
}
