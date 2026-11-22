//! Starting a desktop entry from the Applications page, as its entry
//! describes it: the Exec line with its field codes filled in for a launch
//! that opens nothing, in the entry's Path, inside x-terminal-emulator for
//! `Terminal=true`, and inside its own systemd scope when systemd-run is on
//! PATH. An entry with `DBusActivatable=true` whose id is a well-known bus
//! name is activated over the session bus instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gio::prelude::*;

use crate::runner::desktop_argv;

/// How long the application has to answer `Activate`. Starting a service
/// that is not running yet is the bus's business and comes before it.
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(3);

/// Starts `desktop` and calls `done` with the outcome: at once for an entry
/// that is run, from the main loop once the application has answered for
/// one that is activated over the bus. An error names the application.
pub fn launch_desktop_app(
    desktop: &gio::DesktopAppInfo,
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    let name = desktop.name().to_string();
    if let Some(bus_name) = bus_name(desktop) {
        glib::MainContext::default().spawn_local(async move {
            let result = activate(&bus_name)
                .await
                .map_err(|error| format!("Failed to activate {name}: {}", error.message()));
            done(result);
        });
        return;
    }
    done(spawn_desktop_app(desktop).map_err(|error| format!("Failed to launch {name}: {error}")));
}

/// Runs the entry's Exec line directly, never through a shell.
fn spawn_desktop_app(desktop: &gio::DesktopAppInfo) -> Result<(), String> {
    let argv = command_line(desktop)?;
    let working_dir = desktop
        .string("Path")
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(path.as_str()));
    if find_program(&argv[0], working_dir.as_deref()).is_none() {
        return Err(format!("{} is not an executable program", argv[0]));
    }

    let app_id = desktop.id().map(|id| id.to_string()).unwrap_or_default();
    let argv = desktop_argv(
        &app_id,
        argv,
        desktop.boolean("Terminal"),
        glib::find_program_in_path("systemd-run").as_deref(),
    );
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(working_dir) = working_dir {
        // The child enters it before running anything: when it cannot,
        // nothing is started and spawn() reports why.
        command.current_dir(working_dir);
    }
    // ilia closes right away; the program is not waited for.
    command.spawn().map(drop).map_err(|error| error.to_string())
}

/// The program and arguments the entry's Exec line runs, with its field
/// codes filled in from the entry.
pub(crate) fn command_line(desktop: &gio::DesktopAppInfo) -> Result<Vec<String>, String> {
    let exec = desktop.string("Exec").unwrap_or_default();
    let fields = FieldValues {
        name: desktop.name().to_string(),
        icon: desktop
            .locale_string("Icon")
            .map(|icon| icon.to_string())
            .filter(|icon| !icon.is_empty()),
        desktop_file: desktop
            .filename()
            .map(|path| path.to_string_lossy().into_owned()),
    };
    exec_argv(&exec, &fields)
}

/// What the field codes of an Exec line stand for.
struct FieldValues {
    /// The localized name, for `%c`.
    name: String,
    /// The Icon key, for `%i`; `None` when it is missing or empty.
    icon: Option<String>,
    /// The desktop file's path, for `%k`.
    desktop_file: Option<String>,
}

/// The program and arguments of an Exec line: split with the desktop file
/// quoting rules, then each argument's field codes filled in, literally.
fn exec_argv(exec: &str, fields: &FieldValues) -> Result<Vec<String>, String> {
    let words = glib::shell_parse_argv(exec).map_err(|error| error.message().to_owned())?;
    let mut argv = Vec::new();
    for word in words {
        expand_field_codes(&word.to_string_lossy(), fields, &mut argv);
    }
    if argv.is_empty() {
        return Err("the entry has no command".to_owned());
    }
    Ok(argv)
}

/// Appends what one Exec argument becomes in a launch that opens no files
/// or addresses. A value filled in is never split or read again. A lone
/// `%i` becomes `--icon <icon>`; an argument made only of codes that stand
/// for nothing disappears.
fn expand_field_codes(word: &str, fields: &FieldValues, argv: &mut Vec<String>) {
    if word == "%i" {
        if let Some(icon) = &fields.icon {
            argv.extend(["--icon".to_owned(), icon.clone()]);
        }
        return;
    }
    let mut expanded = String::new();
    let mut chars = word.chars();
    while let Some(char) = chars.next() {
        if char != '%' {
            expanded.push(char);
            continue;
        }
        match chars.next() {
            // A % at the very end stays.
            None => expanded.push('%'),
            Some('%') => expanded.push('%'),
            Some('c') => expanded.push_str(&fields.name),
            Some('k') => expanded.push_str(fields.desktop_file.as_deref().unwrap_or_default()),
            Some('i') => expanded.push_str(fields.icon.as_deref().unwrap_or_default()),
            // %f %F %u %U with nothing to open, the deprecated codes and
            // unknown ones.
            Some(_) => {}
        }
    }
    // An empty quoted argument ("") stays.
    if !expanded.is_empty() || word.is_empty() {
        argv.push(expanded);
    }
}

/// Where `program` would be run from, if it is an executable file: looked
/// up on PATH, or, when it names a path, taken relative to the folder the
/// program runs in.
fn find_program(program: &str, working_dir: Option<&Path>) -> Option<PathBuf> {
    if program.contains('/') {
        // GLib checks a name with a slash as a path, relative to ilia's
        // own folder.
        let path = working_dir.map_or_else(|| PathBuf::from(program), |dir| dir.join(program));
        glib::find_program_in_path(path)
    } else {
        glib::find_program_in_path(program)
    }
}

/// The well-known bus name `desktop` is activated at: its id without
/// `.desktop`, for an entry with `DBusActivatable=true` whose id is one.
pub(crate) fn bus_name(desktop: &gio::DesktopAppInfo) -> Option<String> {
    if !desktop.boolean("DBusActivatable") {
        return None;
    }
    let id = desktop.id()?;
    let name = id.strip_suffix(".desktop")?;
    is_well_known_name(name).then(|| name.to_owned())
}

fn is_well_known_name(name: &str) -> bool {
    !name.starts_with(':') && gio::dbus_is_name(name)
}

/// The application's object path: `/` and its bus name with `.` as `/` and
/// `-` as `_`.
fn object_path(bus_name: &str) -> String {
    format!("/{}", bus_name.replace('.', "/").replace('-', "_"))
}

/// Has the bus start the application if its name is unowned, then calls
/// `org.freedesktop.Application.Activate` on it and waits for the answer.
async fn activate(bus_name: &str) -> Result<(), glib::Error> {
    let connection = gio::bus_get_future(gio::BusType::Session).await?;
    let (owned,) = connection
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(bus_name,).to_variant()),
            Some(glib::VariantTy::new("(b)").unwrap()),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await?
        .get::<(bool,)>()
        .unwrap_or_default();
    if !owned {
        // Answered once the service the bus started owns the name, or the
        // bus gave up on it. The bus refuses a name without a service file
        // even when it is owned, hence the check above.
        connection
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "StartServiceByName",
                Some(&(bus_name, 0u32).to_variant()),
                Some(glib::VariantTy::new("(u)").unwrap()),
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await?;
    }
    let platform_data: HashMap<String, glib::Variant> = HashMap::new();
    connection
        .call_future(
            Some(bus_name),
            &object_path(bus_name),
            "org.freedesktop.Application",
            "Activate",
            Some(&(platform_data,).to_variant()),
            Some(glib::VariantTy::UNIT),
            gio::DBusCallFlags::NO_AUTO_START,
            ACTIVATE_TIMEOUT.as_millis() as i32,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{exec_argv, find_program, is_well_known_name, object_path, FieldValues};
    use std::path::Path;

    fn fields() -> FieldValues {
        FieldValues {
            name: "It's \"$HOME\" 100% %c \\ a".to_owned(),
            icon: Some("/opt/zz icon.png".to_owned()),
            desktop_file: Some("/data/applications/my app.desktop".to_owned()),
        }
    }

    fn argv(exec: &str, fields: &FieldValues) -> Vec<String> {
        exec_argv(exec, fields).unwrap()
    }

    #[test]
    fn values_are_filled_in_literally_as_whole_arguments() {
        let fields = fields();
        assert_eq!(
            argv("probe %c --class=%c %i %k", &fields),
            [
                "probe",
                "It's \"$HOME\" 100% %c \\ a",
                "--class=It's \"$HOME\" 100% %c \\ a",
                "--icon",
                "/opt/zz icon.png",
                "/data/applications/my app.desktop",
            ]
        );
    }

    #[test]
    fn codes_with_nothing_to_open_leave_no_argument() {
        let fields = fields();
        assert_eq!(
            argv("probe %f %F %u %U %d %D %n %N %v %m %z end", &fields),
            ["probe", "end"]
        );
        assert_eq!(
            argv("probe --file=%f --u=%U%F end%", &fields),
            ["probe", "--file=", "--u=", "end%"]
        );
        assert_eq!(
            argv("probe \"\" %%c 100%%", &fields),
            ["probe", "", "%c", "100%"]
        );
    }

    #[test]
    fn missing_or_empty_icon_and_file_fill_in_nothing() {
        let fields = FieldValues {
            name: "Name".to_owned(),
            icon: None,
            desktop_file: None,
        };
        assert_eq!(argv("probe %i %k end", &fields), ["probe", "end"]);
        assert_eq!(argv("probe --k=%k", &fields), ["probe", "--k="]);
    }

    #[test]
    fn quoting_and_escapes_follow_the_desktop_file_rules() {
        let fields = fields();
        assert_eq!(
            argv(
                r#""/a dir/probe" "two words" "a\"b" "a'b" \$HOME \\ \`x end"#,
                &fields
            ),
            [
                "/a dir/probe",
                "two words",
                "a\"b",
                "a'b",
                "$HOME",
                "\\",
                "`x",
                "end"
            ]
        );
        assert!(exec_argv("", &fields).is_err());
        assert!(exec_argv("%f %U", &fields).is_err());
        assert!(exec_argv("probe \"unterminated", &fields).is_err());
    }

    #[test]
    fn only_well_known_names_are_activated() {
        for name in [
            "org.example.App",
            "org.example.Zz-Started",
            "org.example._Boundary-App",
        ] {
            assert!(is_well_known_name(name), "{name}");
        }
        for name in [
            "app", "org.1Bad", "org..Bad", ":1.22", "org.Bad!", "org.Bad.", "",
        ] {
            assert!(!is_well_known_name(name), "{name}");
        }
    }

    #[test]
    fn object_paths_come_from_the_bus_name() {
        assert_eq!(object_path("org.example.Foo-Bar"), "/org/example/Foo_Bar");
        assert_eq!(
            object_path("org.example._Boundary-App"),
            "/org/example/_Boundary_App"
        );
    }

    #[test]
    fn programs_with_a_path_are_found_from_the_working_directory() {
        assert_eq!(
            find_program("/bin/sh", None).as_deref(),
            Some(Path::new("/bin/sh"))
        );
        assert_eq!(
            find_program("bin/sh", Some(Path::new("/"))).as_deref(),
            Some(Path::new("/bin/sh"))
        );
        assert!(find_program("/nonexistent/program", None).is_none());
        assert!(find_program("/", None).is_none());
        assert!(find_program("/etc/hostname-not-a-program", None).is_none());
    }
}
