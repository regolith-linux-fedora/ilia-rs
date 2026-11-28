//! The program a desktop entry starts, as the Applications page's path strip
//! names it: worked out from the entry's `Exec` line the way the system
//! would run it, through `env` wrappers and the search path.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// What separates a found link from its resolved target in the strip.
pub const LINK_MARKER: &str = " → ";
/// The prefix of an entry started over the session bus.
pub const BUS_PREFIX: &str = "D-Bus: ";
/// The suffix of a program that is not there to run.
pub const NOT_FOUND_SUFFIX: &str = "(not found)";

/// The search path used when the environment has none, as `execvp` does.
const DEFAULT_SEARCH_PATH: &str = "/bin:/usr/bin";

/// The strip's text for a desktop entry: `D-Bus: <name>` for an entry
/// started over the session bus, otherwise the path of the program its
/// command line runs (with the target of a link), or the program word and
/// ` (not found)`. Empty when the entry names no program.
pub fn describe_app(app: &gio::DesktopAppInfo) -> String {
    if let Some(bus) = bus_description(app) {
        return bus;
    }
    // GLib already undid the key file escapes (\s, \n, \t, \r, \\).
    let Some(commandline) = app.string("Exec") else {
        return String::new();
    };
    describe_on_path(&commandline)
}

/// What the strip shows for an action of `app` whose Exec line is
/// `commandline`: as for the app, with the action's line in place of the
/// app's. An app started over the session bus starts its actions there too.
pub fn describe_action(app: &gio::DesktopAppInfo, commandline: &str) -> String {
    if let Some(bus) = bus_description(app) {
        return bus;
    }
    if commandline.is_empty() {
        return String::new();
    }
    describe_on_path(commandline)
}

/// The strip's text for an app started over the session bus.
fn bus_description(app: &gio::DesktopAppInfo) -> Option<String> {
    if !app.boolean("DBusActivatable") {
        return None;
    }
    let name = app.filename().as_deref().and_then(bus_name_of_file)?;
    Some(format!("{BUS_PREFIX}{name}"))
}

fn describe_on_path(commandline: &str) -> String {
    let search_path = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_SEARCH_PATH.to_owned());
    describe_commandline(commandline, &search_path)
}

/// The strip's text for an unescaped `Exec` value, with `search_path` as the
/// launcher's own search path.
pub fn describe_commandline(commandline: &str, search_path: &str) -> String {
    let words: Vec<String> = split_commandline(commandline)
        .iter()
        .map(|word| remove_field_codes(word))
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        return String::new();
    }
    let (program, path_override) = unwrap_env(&words);
    let search_path = path_override.unwrap_or(search_path);
    match locate(program, search_path) {
        Some(found) => describe_found(&found),
        None => format!("{program} {NOT_FOUND_SUFFIX}"),
    }
}

/// The part of the strip's text the copy key puts on the clipboard: the
/// path before any link target. `None` when there is nothing to copy: no
/// program, a session-bus entry, or a program that was not found.
pub fn copied_path(text: &str) -> Option<&str> {
    if text.is_empty() || text.starts_with(BUS_PREFIX) || text.ends_with(NOT_FOUND_SUFFIX) {
        return None;
    }
    Some(text.split(LINK_MARKER).next().unwrap_or(text))
}

/// The strip's text as drawn on its one line: line breaks a program path may
/// hold are shown as `↵`. Drawn as they are, they take a glyph from another
/// font, which moves the ellipsis off the line the rest of the text is on.
pub fn single_line_display(text: &str) -> String {
    text.replace(['\n', '\r'], "↵")
}

/// The bus name of a desktop file (its file name without `.desktop`), if
/// that is a valid well-known bus name.
fn bus_name_of_file(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".desktop").unwrap_or(name);
    is_well_known_bus_name(name).then(|| name.to_owned())
}

/// Whether `name` is a valid well-known D-Bus name: two or more non-empty
/// dot-separated elements of ASCII letters, digits, `_` and `-`, none
/// starting with a digit, 255 characters at most.
pub fn is_well_known_bus_name(name: &str) -> bool {
    if name.len() > 255 {
        return false;
    }
    let elements: Vec<&str> = name.split('.').collect();
    elements.len() >= 2
        && elements.iter().all(|element| {
            element
                .bytes()
                .next()
                .is_some_and(|first| !first.is_ascii_digit())
                && element
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
}

/// Splits an unescaped `Exec` value into words by the desktop entry rules:
/// spaces (and tabs) separate words, a double-quoted span belongs to one
/// word, and inside it a backslash makes the following `` ` ``, `$`, `"` or
/// `\` literal.
pub fn split_commandline(commandline: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut characters = commandline.chars().peekable();
    while let Some(character) = characters.next() {
        if quoted {
            match character {
                '"' => quoted = false,
                '\\' if characters
                    .peek()
                    .is_some_and(|next| matches!(next, '"' | '`' | '$' | '\\')) =>
                {
                    word.extend(characters.next());
                }
                _ => word.push(character),
            }
            continue;
        }
        match character {
            '"' => {
                quoted = true;
                in_word = true;
            }
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            _ => {
                word.push(character);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Removes the desktop entry field codes from a word and turns `%%` into
/// `%`. Other `%` sequences stay as they are.
pub fn remove_field_codes(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let mut characters = word.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            result.push(character);
            continue;
        }
        match characters.peek() {
            Some('%') => {
                characters.next();
                result.push('%');
            }
            Some('c' | 'd' | 'D' | 'f' | 'F' | 'i' | 'k' | 'm' | 'n' | 'N' | 'u' | 'U' | 'v') => {
                characters.next();
            }
            _ => result.push('%'),
        }
    }
    result
}

fn is_env(word: &str) -> bool {
    word == "env" || word.ends_with("/env")
}

/// An `env` assignment: holds `=` and starts with neither `-` nor `=`.
fn is_assignment(word: &str) -> bool {
    word.contains('=') && !word.starts_with('-') && !word.starts_with('=')
}

/// The program word behind any `env` wrappers in `words` (which must not be
/// empty), and the search path the last `PATH=` assignment gave it.
///
/// Each `env` is read in stages: removals (`-u NAME`, `--unset=NAME`), at
/// most one `--`, then assignments; the next word is its program. When no
/// word is left, or another option stands where an option may, that `env`
/// is the program.
pub fn unwrap_env(words: &[String]) -> (&str, Option<&str>) {
    let mut rest = words;
    let mut search_path = None;
    loop {
        let program = rest[0].as_str();
        if !is_env(program) {
            return (program, search_path);
        }
        let mut index = 1;
        loop {
            match rest.get(index).map(String::as_str) {
                Some("-u") if index + 1 < rest.len() => index += 2,
                Some("-u") => return (program, search_path),
                Some(word) if word.starts_with("--unset=") => index += 1,
                _ => break,
            }
        }
        let separated = rest.get(index).is_some_and(|word| word == "--");
        if separated {
            index += 1;
        }
        let mut assigned = false;
        while let Some(word) = rest.get(index).filter(|word| is_assignment(word)) {
            if let Some(value) = word.strip_prefix("PATH=") {
                search_path = Some(value);
            }
            assigned = true;
            index += 1;
        }
        match rest.get(index) {
            None => return (program, search_path),
            Some(next) if next.starts_with('-') && !separated && !assigned => {
                return (program, search_path)
            }
            Some(_) => rest = &rest[index..],
        }
    }
}

/// The path the program word runs from: an absolute word as written, a bare
/// name from the first search path directory that holds it, or `None` when
/// no file there is one the user may execute.
pub fn locate(program: &str, search_path: &str) -> Option<String> {
    if program.contains('/') {
        return is_executable_file(Path::new(program)).then(|| program.to_owned());
    }
    search_path
        .split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| {
            if directory.ends_with('/') {
                format!("{directory}{program}")
            } else {
                format!("{directory}/{program}")
            }
        })
        .find(|candidate| is_executable_file(Path::new(candidate)))
}

/// Whether `path`, following links, is a regular file the user may reach
/// and execute.
pub fn is_executable_file(path: &Path) -> bool {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string that outlives the
    // call, which only reads it.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
}

/// A found path, and the fully resolved path when it is itself a link.
fn describe_found(path: &str) -> String {
    let is_link =
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
    if !is_link {
        return path.to_owned();
    }
    match fs::canonicalize(path) {
        Ok(target) => format!("{path}{LINK_MARKER}{}", target.to_string_lossy()),
        Err(_) => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        copied_path, describe_commandline, is_well_known_bus_name, remove_field_codes,
        single_line_display, split_commandline, unwrap_env,
    };
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::{Path, PathBuf};

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn unwrapped(commandline: &str) -> (String, Option<String>) {
        let words = split_commandline(commandline);
        let (program, path) = unwrap_env(&words);
        (program.to_owned(), path.map(str::to_owned))
    }

    #[test]
    fn quotes_group_words_and_backslashes_escape_inside_them() {
        assert_eq!(
            split_commandline("prog  --flag \"two words\" %U"),
            words(&["prog", "--flag", "two words", "%U"])
        );
        assert_eq!(
            split_commandline(r#""/a/we\$ird \"name\"" x"#),
            words(&["/a/we$ird \"name\"", "x"])
        );
        assert_eq!(
            split_commandline(r#""tick\`slash\\dollar\$" "\n""#),
            words(&["tick`slash\\dollar$", "\\n"])
        );
        assert_eq!(split_commandline("a\tb"), words(&["a", "b"]));
        assert_eq!(split_commandline("\"a\tb\""), words(&["a\tb"]));
        assert_eq!(split_commandline("\"\" x"), words(&["", "x"]));
    }

    #[test]
    fn field_codes_are_removed_anywhere_and_percent_percent_kept() {
        assert_eq!(remove_field_codes("%U"), "");
        assert_eq!(remove_field_codes("pl%fainprog"), "plainprog");
        assert_eq!(remove_field_codes("%c%d%D%f%F%i%k%m%n%N%u%U%v"), "");
        assert_eq!(remove_field_codes("percent%%prog"), "percent%prog");
        assert_eq!(remove_field_codes("pct%%pl%fain"), "pct%plain");
        assert_eq!(remove_field_codes("100%x%"), "100%x%");
    }

    #[test]
    fn env_stages_lead_to_the_program() {
        assert_eq!(
            unwrapped("env -u LANG -- FOO=1 \"BAR=a b\" prog --x"),
            ("prog".to_owned(), None)
        );
        assert_eq!(
            unwrapped("env A=1 /usr/bin/env B=2 prog"),
            ("prog".to_owned(), None)
        );
        assert_eq!(
            unwrapped("env PATH=/one PATH=/two prog"),
            ("prog".to_owned(), Some("/two".to_owned()))
        );
        assert_eq!(
            unwrapped("env PATH=/one /usr/bin/env FOO=1 prog"),
            ("prog".to_owned(), Some("/one".to_owned()))
        );
        assert_eq!(unwrapped("env --unset=A -u B prog").0, "prog");
        assert_eq!(unwrapped("env -- -dash").0, "-dash");
        assert_eq!(unwrapped("env FOO=1 -dash").0, "-dash");
        assert_eq!(unwrapped("env -- -- prog").0, "--");
        assert_eq!(unwrapped("env =prog").0, "=prog");
        assert_eq!(unwrapped("env myenv prog").0, "myenv");
        assert_eq!(unwrapped("env ENV prog").0, "ENV");
    }

    #[test]
    fn env_is_the_program_without_a_following_word_or_with_other_options() {
        assert_eq!(unwrapped("env").0, "env");
        assert_eq!(unwrapped("env FOO=1").0, "env");
        assert_eq!(unwrapped("env -u").0, "env");
        assert_eq!(unwrapped("env -u LANG --unset=FOO --").0, "env");
        assert_eq!(unwrapped("env -C /tmp prog").0, "env");
        assert_eq!(unwrapped("env FOO=1 /usr/bin/env BAR=2").0, "/usr/bin/env");
        assert_eq!(
            unwrapped("env PATH=/other env FOO=1"),
            ("env".to_owned(), Some("/other".to_owned()))
        );
    }

    #[test]
    fn bus_names_need_two_valid_elements() {
        assert!(is_well_known_bus_name("org.example.App"));
        assert!(is_well_known_bus_name("org._underscore-hyphen9"));
        assert!(is_well_known_bus_name(&format!("org.{}", "a".repeat(251))));
        assert!(!is_well_known_bus_name(&format!("org.{}", "a".repeat(252))));
        for name in [
            "single",
            "org..Empty",
            ".org.Leading",
            "org.Trailing.",
            "org.Bad+character",
            "org.Nonasciié",
            "1org.First",
            "org.3x.Digit",
            "",
        ] {
            assert!(!is_well_known_bus_name(name), "{name}");
        }
    }

    #[test]
    fn line_breaks_are_drawn_as_symbols() {
        assert_eq!(single_line_display("/a/white\nprog"), "/a/white↵prog");
        assert_eq!(single_line_display("/a/white\rprog"), "/a/white↵prog");
        assert_eq!(single_line_display("/a/white\tprog"), "/a/white\tprog");
    }

    #[test]
    fn copied_path_stops_at_the_first_link_marker() {
        assert_eq!(copied_path("/usr/bin/a"), Some("/usr/bin/a"));
        assert_eq!(copied_path("/a/b → /c/d → e"), Some("/a/b"));
        assert_eq!(copied_path(""), None);
        assert_eq!(copied_path("D-Bus: org.example.App"), None);
        assert_eq!(copied_path("prog (not found)"), None);
        assert_eq!(copied_path("/bin/valid (not found)"), None);
    }

    /// A scratch directory removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("ilia-program-path-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, mode: u32) -> String {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path.to_str().unwrap().to_owned()
        }

        fn dir(&self, name: &str) -> String {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path.to_str().unwrap().to_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn programs_are_found_in_search_path_order() {
        let scratch = Scratch::new("lookup");
        let early = scratch.dir("early");
        let late = scratch.dir("late");
        scratch.file("early/plain", 0o644);
        fs::create_dir(Path::new(&early).join("folder")).unwrap();
        let plain = scratch.file("late/plain", 0o755);
        let folder = scratch.file("late/folder", 0o755);
        symlink("missing", Path::new(&early).join("dangling")).unwrap();
        let dangling = scratch.file("late/dangling", 0o755);
        let search_path = format!("{early}::{late}/");
        assert_eq!(describe_commandline("plain %U", &search_path), plain);
        assert_eq!(describe_commandline("folder", &search_path), folder);
        assert_eq!(describe_commandline("dangling", &search_path), dangling);
        assert_eq!(
            describe_commandline("nowhere", &search_path),
            "nowhere (not found)"
        );
        assert_eq!(
            describe_commandline(&format!("env PATH={early} plain"), &search_path),
            "plain (not found)"
        );
        assert_eq!(describe_commandline("%f", &search_path), "");
    }

    #[test]
    fn absolute_programs_and_links_are_shown_as_written() {
        let scratch = Scratch::new("links");
        let target = scratch.file("real/target", 0o755);
        let tools = scratch.dir("tools");
        symlink("../real/target", Path::new(&tools).join("inner")).unwrap();
        symlink("inner", Path::new(&tools).join("outer")).unwrap();
        let resolved = fs::canonicalize(&target).unwrap();
        let resolved = resolved.to_str().unwrap();
        assert_eq!(
            describe_commandline("outer", &tools),
            format!("{tools}/outer → {resolved}")
        );
        let verbatim = format!("{tools}/../real/target");
        assert_eq!(describe_commandline(&verbatim, ""), verbatim);
        let plain = scratch.file("tools/plain", 0o644);
        assert_eq!(
            describe_commandline(&format!("env {plain}"), ""),
            format!("{plain} (not found)")
        );
    }
}
