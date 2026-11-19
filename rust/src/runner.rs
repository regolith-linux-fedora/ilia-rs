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
    use super::{in_terminal, needs_terminal, systemd_escape, unit_name};
    use gio::prelude::*;

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
}
