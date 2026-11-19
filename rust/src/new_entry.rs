//! A desktop entry the user adds from the Applications page's form: the
//! checks on what was typed, the file's contents, and its creation in the
//! user's applications directory.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::program_path::is_executable_file;

const GROUP: &str = "Desktop Entry";
// The longest file name stem made from a name.
const SLUG_MAX_LEN: usize = 64;
// The suffixes an icon name loses: icon names go without their extension.
const ICON_SUFFIXES: &[&str] = &[".png", ".svg", ".xpm"];
// Besides ASCII letters and digits, the characters an Exec word may hold
// without quotes. Anything else is quoted, as the Desktop Entry
// Specification asks for its reserved characters.
const EXEC_PLAIN: &str = "-_./:,+=@%";

/// The form fields a check can fail on, in the form's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Command,
    Icon,
}

/// Why no entry was written.
#[derive(Debug, PartialEq, Eq)]
pub enum EntryError {
    /// A field holds something that cannot be written.
    Invalid(Field, String),
    /// The directory or the file could not be created.
    Write(String),
}

impl EntryError {
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(_, message) | Self::Write(message) => message,
        }
    }

    /// The field to correct, `None` for a failed write.
    pub fn field(&self) -> Option<Field> {
        match self {
            Self::Invalid(field, _) => Some(*field),
            Self::Write(_) => None,
        }
    }
}

/// What the form holds, as typed.
#[derive(Default)]
pub struct EntryInput<'a> {
    pub name: &'a str,
    pub command: &'a str,
    pub icon: &'a str,
    pub comment: &'a str,
    pub keywords: &'a str,
    pub terminal: bool,
}

/// A checked entry, ready to be written.
#[derive(Debug, PartialEq, Eq)]
pub struct NewEntry {
    name: String,
    comment: String,
    exec: Vec<String>,
    icon: Option<String>,
    keywords: Vec<String>,
    terminal: bool,
}

impl NewEntry {
    /// Checks the fields in the form's order and prepares the values to
    /// write. Programs are looked up in the launcher's `PATH`, and `~/` is
    /// the launcher's home directory.
    pub fn validate(input: &EntryInput) -> Result<Self, EntryError> {
        let path = std::env::var_os("PATH");
        Self::validate_with(input, &glib::home_dir(), path.as_deref())
    }

    fn validate_with(
        input: &EntryInput,
        home: &Path,
        path: Option<&OsStr>,
    ) -> Result<Self, EntryError> {
        let name = trimmed(input.name);
        if name.is_empty() {
            return Err(EntryError::Invalid(Field::Name, "Enter a name.".to_owned()));
        }
        let invalid_command = |message| EntryError::Invalid(Field::Command, message);
        let mut exec = split_command(trimmed(input.command)).map_err(invalid_command)?;
        exec[0] = resolve_program(&exec[0], home, path).map_err(invalid_command)?;
        let icon = icon_value(trimmed(input.icon))
            .map_err(|message| EntryError::Invalid(Field::Icon, message))?;
        Ok(Self {
            name: name.to_owned(),
            comment: trimmed(input.comment).to_owned(),
            exec,
            icon,
            keywords: split_keywords(input.keywords),
            terminal: input.terminal,
        })
    }

    /// The desktop file's contents.
    pub fn to_data(&self) -> String {
        let mut data = format!("[{GROUP}]\nType=Application\n");
        data.push_str(&format!("Name={}\n", escape_value(&self.name)));
        if !self.comment.is_empty() {
            data.push_str(&format!("Comment={}\n", escape_value(&self.comment)));
        }
        data.push_str(&format!("Exec={}\n", escape_value(&exec_value(&self.exec))));
        if let Some(icon) = &self.icon {
            data.push_str(&format!("Icon={}\n", escape_value(icon)));
        }
        data.push_str(&format!("Terminal={}\n", self.terminal));
        if !self.keywords.is_empty() {
            let keywords: String = self
                .keywords
                .iter()
                .map(|keyword| format!("{};", escape_value(keyword).replace(';', "\\;")))
                .collect();
            data.push_str(&format!("Keywords={keywords}\n"));
        }
        data
    }

    /// Writes the entry to a new file in the user's applications directory
    /// and returns its path. The file is named after the name (see `slug`),
    /// with the first `-2`, `-3`, ... suffix whose desktop id no
    /// applications directory holds. Missing directories are created 0700,
    /// the file 0644, both less the umask; nothing that exists is replaced.
    pub fn save(&self) -> Result<PathBuf, EntryError> {
        let directory = user_applications_dir();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|error| {
                EntryError::Write(format!(
                    "Cannot create {}: {}",
                    directory.display(),
                    error_text(&error)
                ))
            })?;
        let taken = desktop_ids(&applications_dirs());
        let stem = slug(&self.name);
        let data = self.to_data();
        let mut number = 1u32;
        loop {
            let id = if number == 1 {
                format!("{stem}.desktop")
            } else {
                format!("{stem}-{number}.desktop")
            };
            number += 1;
            if taken.contains(&id) {
                continue;
            }
            let path = directory.join(&id);
            match create_file(&path, data.as_bytes()) {
                Ok(()) => return Ok(path),
                // Something appeared at that path: never replace it.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(EntryError::Write(format!(
                        "Cannot write {id}: {}",
                        error_text(&error)
                    )))
                }
            }
        }
    }
}

/// An I/O error's description, without the error number Rust appends.
fn error_text(error: &io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_owned(),
        None => text,
    }
}

/// Creates `path` with `data`, failing if anything, a dangling symbolic link
/// included, exists there.
fn create_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if let Err(error) = file.write_all(data) {
        drop(file);
        // The file is ours and incomplete.
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

/// The text without the spaces, tabs and newlines at both of its ends.
pub fn trimmed(text: &str) -> &str {
    text.trim_matches(|character| matches!(character, ' ' | '\t' | '\n'))
}

/// `$XDG_DATA_HOME/applications`, or `~/.local/share/applications` when
/// `XDG_DATA_HOME` is unset or empty.
pub fn user_applications_dir() -> PathBuf {
    glib::user_data_dir().join("applications")
}

/// The applications directories desktop ids are looked up in: the user's,
/// then those of `$XDG_DATA_DIRS`.
fn applications_dirs() -> Vec<PathBuf> {
    std::iter::once(glib::user_data_dir())
        .chain(glib::system_data_dirs())
        .map(|directory| directory.join("applications"))
        .collect()
}

/// The desktop ids of every `*.desktop` file under `directories`, whatever
/// the file holds or whether it can be read. A file in a subdirectory has
/// the id of its relative path with each `/` turned into `-`.
fn desktop_ids(directories: &[PathBuf]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for directory in directories {
        collect_desktop_ids(directory, "", &mut ids, &mut Vec::new());
    }
    ids
}

// `ancestors` holds the directories being read above `directory`, so links
// back up the tree are not followed.
fn collect_desktop_ids(
    directory: &Path,
    prefix: &str,
    ids: &mut HashSet<String>,
    ancestors: &mut Vec<PathBuf>,
) {
    let Ok(canonical) = fs::canonicalize(directory) else {
        return;
    };
    if ancestors.contains(&canonical) {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    ancestors.push(canonical);
    for entry in entries.flatten() {
        // Names that are not UTF-8 cannot make the ids ilia writes.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            collect_desktop_ids(&path, &format!("{prefix}{name}-"), ids, ancestors);
        } else if name.ends_with(".desktop") {
            ids.insert(format!("{prefix}{name}"));
        }
    }
    ancestors.pop();
}

/// Splits a command line into words the way a POSIX shell quotes, with no
/// expansion of any kind. Single quotes keep everything literally; inside
/// double quotes a backslash escapes only `$`, `` ` ``, `"`, `\` and
/// newline; outside quotes it escapes any character. A backslash before a
/// newline removes both. Words are separated by unquoted spaces, tabs and
/// newlines.
pub fn split_command(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    // A quoted empty string makes a word too.
    let mut in_word = false;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\\' => match characters.next() {
                None => return Err("The command ends with a lone backslash.".to_owned()),
                Some('\n') => {}
                Some(escaped) => {
                    word.push(escaped);
                    in_word = true;
                }
            },
            '\'' => {
                in_word = true;
                loop {
                    match characters.next() {
                        None => {
                            return Err(
                                "The command has a single quote that is not closed.".to_owned()
                            )
                        }
                        Some('\'') => break,
                        Some(quoted) => word.push(quoted),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match characters.next() {
                        None => {
                            return Err(
                                "The command has a double quote that is not closed.".to_owned()
                            )
                        }
                        Some('"') => break,
                        Some('\\')
                            if matches!(characters.peek(), Some('$' | '`' | '"' | '\\' | '\n')) =>
                        {
                            if let Some(escaped) = characters.next().filter(|&c| c != '\n') {
                                word.push(escaped);
                            }
                        }
                        Some(quoted) => word.push(quoted),
                    }
                }
            }
            other => {
                word.push(other);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err("Enter a command.".to_owned());
    }
    Ok(words)
}

/// The program to write for a command's first word: the word itself, or the
/// absolute path for a word starting with `~/`. A word with a `/` must then
/// be an absolute path to an executable regular file; a word without one
/// must name an executable regular file in a `PATH` directory.
fn resolve_program(word: &str, home: &Path, path: Option<&OsStr>) -> Result<String, String> {
    if word.is_empty() {
        return Err("The command has no program.".to_owned());
    }
    let program = match word.strip_prefix("~/") {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => word.to_owned(),
    };
    if program.contains('/') {
        if !Path::new(&program).is_absolute() {
            return Err(format!("The program {word} is not an absolute path."));
        }
        if !is_executable_file(Path::new(&program)) {
            return Err(format!("The program {word} is not an executable file."));
        }
        return Ok(program);
    }
    let found = path.is_some_and(|path| {
        std::env::split_paths(path)
            .filter(|directory| !directory.as_os_str().is_empty())
            .any(|directory| is_executable_file(&directory.join(&program)))
    });
    if found {
        Ok(program)
    } else {
        Err(format!("The program {word} is not in PATH."))
    }
}

/// The Icon value: none for an empty field, an absolute path to an existing
/// file as it is, otherwise an icon name without a trailing `.png`, `.svg`
/// or `.xpm`.
fn icon_value(icon: &str) -> Result<Option<String>, String> {
    if icon.is_empty() {
        return Ok(None);
    }
    if icon.contains('/') {
        let path = Path::new(icon);
        if !path.is_absolute() || !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return Err(format!(
                "The icon {icon} is not an absolute path to an existing file."
            ));
        }
        return Ok(Some(icon.to_owned()));
    }
    let name = ICON_SUFFIXES
        .iter()
        .find_map(|suffix| icon.strip_suffix(suffix).filter(|stem| !stem.is_empty()))
        .unwrap_or(icon);
    Ok(Some(name.to_owned()))
}

/// The keywords of a comma-separated list, each trimmed, empty ones dropped.
fn split_keywords(text: &str) -> Vec<String> {
    text.split(',')
        .map(trimmed)
        .filter(|keyword| !keyword.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The file name stem for a name: A-Z lowered, each run of characters other
/// than a-z and 0-9 turned into one `-`, `-` stripped at both ends, at most
/// 64 characters, and "app" when nothing is left.
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for character in name.chars().map(|character| character.to_ascii_lowercase()) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(character);
        } else {
            gap = true;
        }
    }
    // Only ASCII is left: bytes are characters.
    slug.truncate(SLUG_MAX_LEN);
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "app".to_owned()
    } else {
        slug.to_owned()
    }
}

/// The Exec value that runs exactly these words: each word quoted when it
/// needs to be, and every `%` doubled so none is taken for a field code.
fn exec_value(words: &[String]) -> String {
    words
        .iter()
        .map(|word| quote_exec_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_exec_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || EXEC_PLAIN.contains(character));
    let mut quoted = String::new();
    if !plain {
        quoted.push('"');
    }
    for character in word.chars() {
        match character {
            '%' => quoted.push_str("%%"),
            '"' | '`' | '$' | '\\' if !plain => {
                quoted.push('\\');
                quoted.push(character);
            }
            other => quoted.push(other),
        }
    }
    if !plain {
        quoted.push('"');
    }
    quoted
}

/// A string as a desktop entry value. GLib's own escaping leaves tabs
/// as they are, which desktop-file-validate refuses.
fn escape_value(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for (index, character) in text.chars().enumerate() {
        match character {
            ' ' if index == 0 => escaped.push_str("\\s"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '\\' => escaped.push_str("\\\\"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn words(line: &str) -> Vec<String> {
        split_command(line).unwrap()
    }

    #[test]
    fn trimming_removes_only_spaces_tabs_and_newlines() {
        assert_eq!(trimmed(" \t\nname\n\t "), "name");
        assert_eq!(trimmed("\u{a0}name\r"), "\u{a0}name\r");
        assert_eq!(trimmed(" \t\n"), "");
    }

    #[test]
    fn command_lines_split_like_shell_quoting() {
        assert_eq!(words("probe 'a b' \"c d\""), ["probe", "a b", "c d"]);
        assert_eq!(
            words("probe\n--first\t\n--second"),
            ["probe", "--first", "--second"]
        );
        assert_eq!(
            words(r#"probe "a\qb" "\$H" "\`x\`" "a\"b" "a\\b""#),
            ["probe", r"a\qb", "$H", "`x`", "a\"b", r"a\b"]
        );
        assert_eq!(
            words(r"probe a\qb a\ b \# \~"),
            ["probe", "aqb", "a b", "#", "~"]
        );
        assert_eq!(
            words("probe '\\$`\";#~*' 'one\\\ntwo'"),
            ["probe", "\\$`\";#~*", "one\\\ntwo"]
        );
        assert_eq!(
            words("probe a\"b c\"'d e'f \"\" '' x\\\ny \"z\\\nw\""),
            ["probe", "ab cd ef", "", "", "xy", "zw"]
        );
        assert_eq!(words("probe one\\\n two"), ["probe", "one", "two"]);
        assert_eq!(
            words("probe $HOME ~ *.txt ; $(echo) `echo`"),
            ["probe", "$HOME", "~", "*.txt", ";", "$(echo)", "`echo`"]
        );
    }

    #[test]
    fn broken_command_lines_are_refused() {
        assert!(split_command("").is_err());
        assert!(split_command("probe 'open").is_err());
        assert!(split_command("probe \"open").is_err());
        assert!(split_command("probe trailing\\").is_err());
        assert_eq!(words("''"), [""]);
    }

    #[test]
    fn exec_values_read_back_as_the_same_words() {
        let original: Vec<String> = [
            "probe",
            "a b",
            "$HOME",
            "back\\slash",
            "say \"hi\"",
            "100%",
            "`x`",
            "a;b",
            "é ü",
            "",
            "#tag",
            "~/x",
            "%f",
            "it's",
            " lead ",
            "kept\nline",
            "tab\there",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        let entry = NewEntry {
            name: "Probe".to_owned(),
            comment: String::new(),
            exec: original.clone(),
            icon: None,
            keywords: Vec::new(),
            terminal: false,
        };
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(&entry.to_data(), glib::KeyFileFlags::NONE)
            .unwrap();
        let exec = keyfile.string(GROUP, "Exec").unwrap();
        let parsed = glib::shell_parse_argv(exec.as_str()).unwrap();
        let parsed: Vec<String> = parsed
            .iter()
            .map(|word| word.to_str().unwrap().replace("%%", "%"))
            .collect();
        assert_eq!(parsed, original);
    }

    #[test]
    fn names_comments_and_keywords_read_back_verbatim() {
        let entry = NewEntry {
            name: "Tab\tName\nWith = Signs; And \\ Back".to_owned(),
            comment: "First\tline; tail \\".to_owned(),
            exec: vec!["/usr/bin/true".to_owned()],
            icon: Some("mark".to_owned()),
            keywords: vec!["alpha;beta".to_owned(), "gamma ;".to_owned()],
            terminal: true,
        };
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(&entry.to_data(), glib::KeyFileFlags::NONE)
            .unwrap();
        assert_eq!(keyfile.string(GROUP, "Name").unwrap(), entry.name.as_str());
        assert_eq!(
            keyfile.string(GROUP, "Comment").unwrap(),
            entry.comment.as_str()
        );
        assert_eq!(keyfile.string(GROUP, "Icon").unwrap(), "mark");
        assert_eq!(keyfile.string(GROUP, "Terminal").unwrap(), "true");
        let keywords: Vec<String> = keyfile
            .string_list(GROUP, "Keywords")
            .unwrap()
            .iter()
            .map(|keyword| keyword.to_string())
            .collect();
        assert_eq!(keywords, entry.keywords);
    }

    #[test]
    fn icons_lose_one_image_suffix_and_paths_must_exist() {
        assert_eq!(icon_value(""), Ok(None));
        assert_eq!(icon_value("mark.png"), Ok(Some("mark".to_owned())));
        assert_eq!(icon_value("mark.png.svg"), Ok(Some("mark.png".to_owned())));
        assert_eq!(icon_value("mark.PNG"), Ok(Some("mark.PNG".to_owned())));
        assert_eq!(icon_value(".xpm"), Ok(Some(".xpm".to_owned())));
        assert!(icon_value("icons/mark.png").is_err());
        assert!(icon_value("/nonexistent-icon.png").is_err());
        assert!(icon_value("/").is_err());
    }

    #[test]
    fn keywords_split_at_commas() {
        assert_eq!(split_keywords(" ,\t,\n, "), Vec::<String>::new());
        assert_eq!(
            split_keywords("alpha;beta, gamma ; , delta"),
            ["alpha;beta", "gamma ;", "delta"]
        );
    }

    #[test]
    fn slugs_keep_ascii_letters_and_digits() {
        assert_eq!(slug("Harbor Sketch"), "harbor-sketch");
        assert_eq!(slug("Café — Draft!"), "caf-draft");
        assert_eq!(slug("---AA__99...ZZ---"), "aa-99-zz");
        assert_eq!(slug("日本語"), "app");
        // The Kelvin sign lowercases to "k" only under Unicode rules.
        assert_eq!(slug("!!!\u{212a}\u{130}!!!"), "app");
        assert_eq!(slug(&"M".repeat(100)), "m".repeat(64));
        assert_eq!(slug(&format!("{}!tail", "A".repeat(63))), "a".repeat(63));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("ilia-new-entry-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn programs_resolve_through_home_and_path() {
        let home = scratch_dir("programs");
        let bin = home.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(bin.join("plain"), "plain").unwrap();
        fs::create_dir(bin.join("folder")).unwrap();
        let path = bin.clone().into_os_string();
        let path = Some(path.as_os_str());

        assert_eq!(resolve_program("tool", &home, path), Ok("tool".to_owned()));
        assert_eq!(
            resolve_program("~/bin/tool", &home, path),
            Ok(bin.join("tool").to_string_lossy().into_owned())
        );
        let absolute = bin.join("tool").to_string_lossy().into_owned();
        assert_eq!(
            resolve_program(&absolute, &home, path),
            Ok(absolute.clone())
        );
        for refused in [
            "",
            "plain",
            "folder",
            "missing",
            "bin/tool",
            "~/bin/plain",
            "~/bin",
        ] {
            assert!(resolve_program(refused, &home, path).is_err(), "{refused}");
        }
        assert!(resolve_program("tool", &home, None).is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn validation_reports_the_first_wrong_field() {
        let home = scratch_dir("validation");
        let input = EntryInput {
            name: " \t",
            command: "missing-program",
            icon: "relative/icon.png",
            ..EntryInput::default()
        };
        let error = NewEntry::validate_with(&input, &home, None).unwrap_err();
        assert_eq!(error.field(), Some(Field::Name));
        let input = EntryInput {
            name: "Name",
            ..input
        };
        let error = NewEntry::validate_with(&input, &home, None).unwrap_err();
        assert_eq!(error.field(), Some(Field::Command));
        let input = EntryInput {
            command: "/bin/sh -c true",
            ..input
        };
        let error = NewEntry::validate_with(&input, &home, None).unwrap_err();
        assert_eq!(error.field(), Some(Field::Icon));
        let input = EntryInput {
            icon: "",
            keywords: "a, b",
            ..input
        };
        let entry = NewEntry::validate_with(&input, &home, None).unwrap();
        assert_eq!(entry.exec, ["/bin/sh", "-c", "true"]);
        assert_eq!(entry.keywords, ["a", "b"]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn desktop_ids_cover_nested_and_unreadable_files() {
        let root = scratch_dir("ids");
        fs::create_dir_all(root.join("ember/nested")).unwrap();
        fs::write(root.join("plain.desktop"), "").unwrap();
        fs::write(root.join("ember/desk-2.desktop"), "").unwrap();
        fs::write(root.join("ember/nested/x.desktop"), "").unwrap();
        fs::write(root.join("notes.txt"), "").unwrap();
        let ids = desktop_ids(&[root.clone(), root.join("missing")]);
        let mut ids: Vec<_> = ids.into_iter().collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "ember-desk-2.desktop",
                "ember-nested-x.desktop",
                "plain.desktop"
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn files_are_created_once_and_never_replace_anything() {
        let root = scratch_dir("create");
        let path = root.join("entry.desktop");
        create_file(&path, b"first").unwrap();
        let error = create_file(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"first");
        let link = root.join("link.desktop");
        std::os::unix::fs::symlink(root.join("missing-target"), &link).unwrap();
        assert!(create_file(&link, b"data").is_err());
        assert!(!root.join("missing-target").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
