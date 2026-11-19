use std::ffi::OsString;

use gio::prelude::*;

/// The terminal Regolith starts programs in, as the commands page does.
const TERMINAL: &str = "x-terminal-emulator";

/// The AppInfo that launches `app_info` in its own systemd scope, when
/// systemd-run is available.
///
/// The scope's command line is rebuilt from the Exec key, so keys GLib would
/// otherwise act on are lost. For `Terminal=true` the terminal is started
/// inside the scope, as `x-terminal-emulator -e <Exec>`, so the terminal and
/// the program in it share the scope. Without x-terminal-emulator GLib picks
/// a terminal itself, which then runs outside the scope.
pub fn runner_app_info(app_info: &gio::AppInfo) -> Result<gio::AppInfo, glib::Error> {
    if glib::find_program_in_path("systemd-run").is_none() {
        // GLib launches the entry itself and honours Terminal=true.
        return Ok(app_info.clone());
    }

    let app_id = app_info
        .id()
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            app_info
                .commandline()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let commandline = app_info
        .commandline()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (commandline, flags) = if needs_terminal(app_info) {
        in_terminal(commandline, glib::find_program_in_path(TERMINAL).is_some())
    } else {
        (commandline, gio::AppInfoCreateFlags::NONE)
    };
    let unit = unit_name(&app_id);
    let wrapped = format!("systemd-run --user --scope --unit \"{unit}\" {commandline}");

    gio::AppInfo::create_from_commandline(&wrapped, Some(&app_id), flags)
}

/// A fresh scope unit name for a program started for `id`: the escaped id
/// and eight random lowercase hexadecimal digits.
fn unit_name(id: &str) -> String {
    let random = glib::uuid_string_random();
    format!("run_ilia_{}_{}.scope", systemd_escape(id), &random[..8])
}

/// The words that start a program in a fresh user scope named for `id`,
/// when systemd-run is available; the program's own words follow them.
pub fn scope_command(id: &str) -> Option<Vec<OsString>> {
    let runner = glib::find_program_in_path("systemd-run")?;
    Some(vec![
        runner.into_os_string(),
        "--user".into(),
        "--scope".into(),
        format!("--unit={}", unit_name(id)).into(),
        "--".into(),
    ])
}

/// The program and arguments that start a desktop entry's `argv`, without a
/// shell: inside x-terminal-emulator for `Terminal=true`, and the whole of
/// it inside the entry's own systemd scope when `systemd_run` (the scope
/// runner found on PATH) is given.
pub fn desktop_argv(
    app_id: &str,
    argv: Vec<String>,
    terminal: bool,
    systemd_run: Option<&std::path::Path>,
) -> Vec<String> {
    let argv = if terminal {
        [TERMINAL.to_owned(), "-e".to_owned()]
            .into_iter()
            .chain(argv)
            .collect()
    } else {
        argv
    };
    match systemd_run {
        Some(systemd_run) => [
            systemd_run.to_string_lossy().into_owned(),
            "--user".to_owned(),
            "--scope".to_owned(),
            "--unit".to_owned(),
            unit_name(app_id),
        ]
        .into_iter()
        .chain(argv)
        .collect(),
        None => argv,
    }
}

/// Whether the desktop entry asks to run in a terminal (`Terminal=true`).
fn needs_terminal(app_info: &gio::AppInfo) -> bool {
    app_info
        .downcast_ref::<gio::DesktopAppInfo>()
        .is_some_and(|desktop| desktop.boolean("Terminal"))
}

/// The scope's program for a `Terminal=true` entry: the terminal running the
/// entry's command line, or, without x-terminal-emulator, the command line
/// with GLib asked to add a terminal.
fn in_terminal(commandline: String, has_terminal: bool) -> (String, gio::AppInfoCreateFlags) {
    if has_terminal {
        (
            format!("{TERMINAL} -e {commandline}"),
            gio::AppInfoCreateFlags::NONE,
        )
    } else {
        (commandline, gio::AppInfoCreateFlags::NEEDS_TERMINAL)
    }
}

pub fn systemd_escape(value: &str) -> String {
    let mut escaped = String::new();
    for (index, byte) in value.bytes().enumerate() {
        if index == 0 {
            if byte == b'.' {
                escaped.push_str("\\x2e");
            } else {
                escaped.push(char::from(byte));
            }
        } else if (byte > 31 && byte < 46)
            || (byte > 58 && byte < 65)
            || (byte > 90 && byte < 97 && byte != b'_')
            || byte > 122
        {
            escaped.push_str(&format!("\\x{byte:x}"));
        } else {
            escaped.push(char::from(byte));
        }
    }
    escaped.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::{desktop_argv, in_terminal, needs_terminal, systemd_escape, unit_name};
    use gio::prelude::*;
    use std::path::Path;

    // GLib rejects an entry whose Exec program is not on PATH, and another
    // test swaps PATH while it runs: name the program by its absolute path.
    fn desktop_entry(terminal: Option<&str>) -> gio::AppInfo {
        let keyfile = glib::KeyFile::new();
        let mut data =
            "[Desktop Entry]\nType=Application\nName=Probe\nExec=/usr/bin/true \"two words\"\n"
                .to_owned();
        if let Some(terminal) = terminal {
            data.push_str(&format!("Terminal={terminal}\n"));
        }
        keyfile
            .load_from_data(&data, glib::KeyFileFlags::NONE)
            .unwrap();
        gio::DesktopAppInfo::from_keyfile(&keyfile)
            .unwrap()
            .upcast()
    }

    #[test]
    fn only_terminal_entries_need_a_terminal() {
        assert!(needs_terminal(&desktop_entry(Some("true"))));
        assert!(!needs_terminal(&desktop_entry(Some("false"))));
        assert!(!needs_terminal(&desktop_entry(None)));
        let commandline =
            gio::AppInfo::create_from_commandline("probe", None, gio::AppInfoCreateFlags::NONE)
                .unwrap();
        assert!(!needs_terminal(&commandline));
    }

    #[test]
    fn terminal_entries_run_inside_x_terminal_emulator() {
        assert_eq!(
            in_terminal("probe \"two words\"".to_owned(), true),
            (
                "x-terminal-emulator -e probe \"two words\"".to_owned(),
                gio::AppInfoCreateFlags::NONE
            )
        );
        assert_eq!(
            in_terminal("probe".to_owned(), false),
            ("probe".to_owned(), gio::AppInfoCreateFlags::NEEDS_TERMINAL)
        );
    }

    #[test]
    fn escape_matches_the_vala_unit_name_rules() {
        assert_eq!(systemd_escape("alpha.desktop"), "alpha.desktop");
        assert_eq!(systemd_escape("/usr/bin/tool"), "-usr-bin-tool");
        assert_eq!(systemd_escape(".hidden name"), "\\x2ehidden\\x20name");
        assert_eq!(systemd_escape("-hyphen"), "-hyphen");
        assert_eq!(systemd_escape("a-hyphen"), "a\\x2dhyphen");
        assert_eq!(systemd_escape("a\u{1f}control"), "a\u{1f}control");
    }

    #[test]
    fn unit_names_are_fresh_for_each_launch() {
        let first = unit_name("Page-ID_2");
        let second = unit_name("Page-ID_2");
        assert_ne!(first, second);
        let random = first
            .strip_prefix("run_ilia_Page\\x2dID_2_")
            .and_then(|rest| rest.strip_suffix(".scope"))
            .unwrap();
        assert_eq!(random.len(), 8);
        assert!(random
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
    }

    fn is_unit_for(unit: &str, escaped_id: &str) -> bool {
        let Some(random) = unit
            .strip_prefix(&format!("run_ilia_{escaped_id}_"))
            .and_then(|rest| rest.strip_suffix(".scope"))
        else {
            return false;
        };
        random.len() == 8
            && random
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn scope_units_keep_the_vala_pattern_with_a_fresh_suffix() {
        let first = unit_name(".hidden name.desktop");
        assert!(
            is_unit_for(&first, "\\x2ehidden\\x20name.desktop"),
            "{first}"
        );
        let others: Vec<String> = (0..4).map(|_| unit_name("app.desktop")).collect();
        assert!(others.iter().all(|unit| is_unit_for(unit, "app.desktop")));
        assert!(others.iter().any(|unit| unit != &others[0]));
    }

    #[test]
    fn desktop_argv_wraps_terminal_and_scope_without_a_shell() {
        let argv = || {
            vec![
                "probe".to_owned(),
                "two words".to_owned(),
                "$HOME".to_owned(),
            ]
        };
        assert_eq!(desktop_argv("app.desktop", argv(), false, None), argv());
        assert_eq!(
            desktop_argv("app.desktop", argv(), true, None),
            ["x-terminal-emulator", "-e", "probe", "two words", "$HOME"]
        );

        let scoped = desktop_argv(
            "app.desktop",
            argv(),
            true,
            Some(Path::new("/usr/bin/systemd-run")),
        );
        assert_eq!(
            scoped[..4],
            ["/usr/bin/systemd-run", "--user", "--scope", "--unit"]
        );
        assert!(is_unit_for(&scoped[4], "app.desktop"), "{}", scoped[4]);
        assert_eq!(
            scoped[5..],
            ["x-terminal-emulator", "-e", "probe", "two words", "$HOME"]
        );
    }
}
