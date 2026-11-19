//! Pages defined in key files: `ilia/pages/*.page` under the user's
//! configuration home, then under each shared configuration directory.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The group holding every setting of a page file.
const GROUP: &str = "Ilia Page";
const SUFFIX: &[u8] = b".page";
/// The icon of a page file without an `Icon` key.
pub const DEFAULT_ICON: &str = "applications-other";

/// Where the rows of a page come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Rows {
    /// Desktop file ids, in the order the rows appear.
    Apps(Vec<String>),
    /// Fixed entries, each a label and a value divided at the first tab.
    Items(Vec<String>),
    /// A shell command whose output lines are the entries.
    Command(String),
}

/// What choosing a text row does.
#[derive(Clone, Debug, PartialEq)]
pub enum Exec {
    /// Print the row's value.
    Print,
    /// Run these words, the row's value substituted for `%s`.
    Words(Vec<String>),
    /// The `Exec` value could not be split into words: choosing does nothing.
    Invalid,
}

/// A usable page file.
#[derive(Clone, Debug)]
pub struct PageFile {
    pub path: PathBuf,
    /// The file name, by which file pages are ordered.
    pub file_name: OsString,
    /// The file name without `.page`, as written.
    pub id: String,
    pub name: String,
    pub icon: String,
    /// The lower-case letter that selects the page with Alt.
    pub key: Option<char>,
    only_show_in: Vec<String>,
    not_show_in: Vec<String>,
    pub rows: Rows,
    pub exec: Exec,
    /// Whether rows are ordered by how often they were chosen.
    pub frequency: bool,
}

impl PageFile {
    /// Whether the page shows under the window manager `manager`: `i3`,
    /// `sway` or `other`.
    pub fn shows_in(&self, manager: &str) -> bool {
        let listed = |names: &[String]| names.iter().any(|name| name == manager);
        (self.only_show_in.is_empty() || listed(&self.only_show_in)) && !listed(&self.not_show_in)
    }
}

/// The folders searched for page files, the personal one first.
fn page_dirs() -> Vec<PathBuf> {
    let blank = |value: &OsString| value.as_bytes().iter().all(u8::is_ascii_whitespace);
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !blank(value))
        .map(PathBuf::from)
        .unwrap_or_else(|| glib::home_dir().join(".config"));
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|value| !blank(value))
        .unwrap_or_else(|| OsString::from("/etc/xdg"));
    std::iter::once(config_home)
        .chain(std::env::split_paths(&config_dirs).filter(|dir| !dir.as_os_str().is_empty()))
        .map(|dir| dir.join("ilia").join("pages"))
        .collect()
}

/// Reads every page file the configuration folders hold. Each problem is
/// printed as one line on standard error; the usable pages are returned in
/// byte-wise order of their file names.
pub fn discover(reserved: &[&str]) -> Vec<PageFile> {
    let (pages, warnings) = discover_in(&page_dirs(), reserved);
    for warning in warnings {
        eprintln!("{warning}");
    }
    pages
}

/// The page files of `dirs`, searched in order, and the warnings they
/// produce. An identity found in an earlier folder wins over the same
/// identity in a later one; only winning files are read.
pub fn discover_in(dirs: &[PathBuf], reserved: &[&str]) -> (Vec<PageFile>, Vec<String>) {
    let mut claimed = HashSet::new();
    let mut winners = Vec::new();
    let mut warnings = Vec::new();
    for dir in dirs {
        let mut in_dir = HashSet::new();
        for (file_name, path) in page_file_names(dir) {
            let id = match identity(file_name.as_bytes(), reserved) {
                Ok(id) => id.to_owned(),
                Err(reason) => {
                    warnings.push(skipped(&path, reason));
                    continue;
                }
            };
            let folded = id.to_ascii_lowercase();
            if !in_dir.insert(folded.clone()) {
                warnings.push(skipped(
                    &path,
                    "another file in the same folder has the same name in another letter case",
                ));
                continue;
            }
            if claimed.insert(folded) {
                winners.push((file_name, path, id));
            }
        }
    }
    let mut pages = Vec::new();
    for (file_name, path, id) in winners {
        match parse(&path, file_name, id, &mut warnings) {
            Ok(page) => pages.push(page),
            Err(reason) => warnings.push(skipped(&path, &reason)),
        }
    }
    pages.sort_by(|a, b| a.file_name.as_bytes().cmp(b.file_name.as_bytes()));
    (pages, warnings)
}

/// The page files directly inside `dir`, in byte-wise order of their names:
/// ordinary files whose names end in `.page`. A folder that cannot be read
/// has none.
fn page_file_names(dir: &Path) -> Vec<(OsString, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(OsString, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().as_bytes().ends_with(SUFFIX))
        // Symbolic links count as the file they point to.
        .filter(|entry| std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file()))
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    files
}

/// The identity of the page file named `file_name`, or why it is refused.
fn identity<'a>(file_name: &'a [u8], reserved: &[&str]) -> Result<&'a str, &'static str> {
    let stem = &file_name[..file_name.len() - SUFFIX.len()];
    if stem.is_empty() {
        return Err("the page name before .page is empty");
    }
    if !stem
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("page names may only hold ASCII letters, digits, '-' and '_'");
    }
    // Only ASCII is left.
    let stem = std::str::from_utf8(stem).unwrap_or_default();
    if reserved.iter().any(|id| id.eq_ignore_ascii_case(stem)) {
        return Err("the name belongs to a built-in page");
    }
    Ok(stem)
}

fn skipped(path: &Path, reason: &str) -> String {
    single_line(format!(
        "ilia: skipping page file '{}': {reason}",
        path.display()
    ))
}

fn warning(path: &Path, problem: &str) -> String {
    single_line(format!("ilia: page file '{}': {problem}", path.display()))
}

/// Every warning is one line, whatever a file name holds.
fn single_line(text: String) -> String {
    text.replace(['\n', '\r'], " ")
}

/// The string value of `key` in the page group. A backslash before a
/// character the key file format gives no meaning stays as written, with
/// that character, as GLib's own parser leaves it, instead of making the
/// value unreadable: a command may hold `printf '\351'`.
fn string_value(keyfile: &glib::KeyFile, key: &str) -> Option<String> {
    match keyfile.string(GROUP, key) {
        Ok(value) => Some(value.into()),
        Err(error) if error.matches(glib::KeyFileError::InvalidValue) => keyfile
            .value(GROUP, key)
            .ok()
            .map(|raw| unescape(raw.as_str())),
        Err(_) => None,
    }
}

/// A key file value with its escapes replaced: `\s`, `\n`, `\t`, `\r` and
/// `\\`. Any other backslash is kept.
fn unescape(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next() {
            Some('s') => text.push(' '),
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some('\\') => text.push('\\'),
            Some(other) => {
                text.push('\\');
                text.push(other);
            }
            None => text.push('\\'),
        }
    }
    text
}

/// The `Name[<locale>]` value for the first locale GLib lists that has
/// one, decoded like any other one-value key: an escape the key file format
/// does not define stays as written instead of hiding the translation.
fn localised_name(keyfile: &glib::KeyFile) -> Option<String> {
    glib::language_names()
        .iter()
        .find_map(|language| string_value(keyfile, &format!("Name[{language}]")))
}

fn parse(
    path: &Path,
    file_name: OsString,
    id: String,
    warnings: &mut Vec<String>,
) -> Result<PageFile, String> {
    let keyfile = glib::KeyFile::new();
    keyfile
        .load_from_file(path, glib::KeyFileFlags::NONE)
        .map_err(|error| format!("not a key file: {}", error.message()))?;
    if !keyfile.has_group(GROUP) {
        return Err(format!("no [{GROUP}] group"));
    }
    let string = |key: &str| string_value(&keyfile, key);
    let list = |key: &str| -> Vec<String> {
        keyfile
            .string_list(GROUP, key)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| !item.is_empty())
                    .map(|item| item.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    let name = string("Name")
        .filter(|name| !name.trim().is_empty())
        .ok_or("Name is missing or blank")?;
    // A blank translation does not hide the plain name.
    let name = localised_name(&keyfile)
        .filter(|localised| !localised.trim().is_empty())
        .unwrap_or(name);

    let apps = list("Apps");
    let items = list("Items");
    let command = string("ItemsCommand").filter(|command| !command.trim().is_empty());
    let mut sources = Vec::new();
    if !apps.is_empty() {
        sources.push(Rows::Apps(apps));
    }
    if !items.is_empty() {
        sources.push(Rows::Items(items));
    }
    if let Some(command) = command {
        sources.push(Rows::Command(command));
    }
    if sources.len() != 1 {
        return Err("exactly one of Apps, Items and ItemsCommand must list rows".to_owned());
    }
    let rows = sources.remove(0);

    let icon = string("Icon")
        .filter(|icon| !icon.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ICON.to_owned());
    let key = string("Key").and_then(|key| {
        let mut letters = key.chars();
        match (letters.next(), letters.next()) {
            (Some(letter), None) if letter.is_ascii_alphabetic() => {
                Some(letter.to_ascii_lowercase())
            }
            _ => {
                warnings.push(warning(
                    path,
                    "Key must be a single ASCII letter; it is ignored",
                ));
                None
            }
        }
    });
    let managers = |key: &str| -> Vec<String> {
        list(key)
            .iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect()
    };

    // Rows of applications launch them: Exec and Sort mean nothing there.
    let is_apps = matches!(rows, Rows::Apps(_));
    let exec = match string("Exec").filter(|exec| !exec.trim().is_empty()) {
        Some(exec) if !is_apps => match glib::shell_parse_argv(exec.as_str()) {
            Ok(words) => Exec::Words(
                words
                    .iter()
                    .map(|word| word.to_string_lossy().into_owned())
                    .collect(),
            ),
            Err(_) => {
                warnings.push(warning(
                    path,
                    "Exec cannot be split into words; choosing a row does nothing",
                ));
                Exec::Invalid
            }
        },
        _ => Exec::Print,
    };
    let frequency = match string("Sort") {
        Some(sort) if !is_apps => {
            let frequency = sort.trim().eq_ignore_ascii_case("frequency");
            if !frequency {
                warnings.push(warning(
                    path,
                    "Sort may only be 'frequency'; rows keep their order",
                ));
            }
            frequency
        }
        _ => false,
    };

    Ok(PageFile {
        path: path.to_owned(),
        file_name,
        id,
        name,
        icon,
        key,
        only_show_in: managers("OnlyShowIn"),
        not_show_in: managers("NotShowIn"),
        rows,
        exec,
        frequency,
    })
}

#[cfg(test)]
mod tests {
    use super::{discover_in, identity, unescape, Exec, Rows};
    use std::path::{Path, PathBuf};

    const RESERVED: &[&str] = &["apps", "terminal", "windows"];

    /// A fresh folder for one test, removed when it ends.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ilia-page-files-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn page(extra: &str) -> String {
        format!("[Ilia Page]\nName=Page\n{extra}")
    }

    #[test]
    fn identities_are_plain_ascii_names_other_than_the_built_in_pages() {
        assert_eq!(identity(b"Good_42-ok.page", RESERVED), Ok("Good_42-ok"));
        assert!(identity(b".page", RESERVED).is_err());
        assert!(identity(b"bad.id.page", RESERVED).is_err());
        assert!(identity(b"bad space.page", RESERVED).is_err());
        assert!(identity("é.page".as_bytes(), RESERVED).is_err());
        assert!(identity(b"APPS.page", RESERVED).is_err());
        assert!(identity(b"Windows.page", RESERVED).is_err());
        assert_eq!(identity(b"apps2.page", RESERVED), Ok("apps2"));
    }

    #[test]
    fn only_lower_case_page_files_directly_inside_count() {
        let folder = Folder::new("discovery");
        folder.write("one.page", &page("Items=one\n"));
        folder.write("upper.PAGE", &page("Items=x\n"));
        folder.write("backup.page~", &page("Items=x\n"));
        std::fs::create_dir(folder.path().join("dir.page")).unwrap();
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "one");
        assert_eq!(pages[0].rows, Rows::Items(vec!["one".to_owned()]));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn earlier_folders_win_and_unusable_winners_are_not_replaced() {
        let personal = Folder::new("personal");
        let shared = Folder::new("shared");
        personal.write("Dup.page", "broken");
        shared.write("dup.page", &page("Items=shared\n"));
        shared.write("only.page", &page("Items=shared\n"));
        let missing = personal.path().join("missing");
        let (pages, warnings) = discover_in(
            &[
                personal.path().to_owned(),
                missing,
                shared.path().to_owned(),
            ],
            RESERVED,
        );
        assert_eq!(
            pages
                .iter()
                .map(|page| page.id.as_str())
                .collect::<Vec<_>>(),
            ["only"]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Dup.page"), "{warnings:?}");
    }

    #[test]
    fn same_folder_case_collisions_keep_the_bytewise_first_name() {
        let folder = Folder::new("collision");
        let loser = folder.write("dup.page", "broken");
        folder.write("Dup.page", &page("Items=winner\n"));
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "Dup");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&loser.display().to_string()));
    }

    #[test]
    fn exactly_one_row_source_makes_a_usable_page() {
        let folder = Folder::new("sources");
        folder.write("none.page", &page("Items=;;\nApps=;\nItemsCommand=  \n"));
        folder.write("two.page", &page("Items=a\nItemsCommand=echo b\n"));
        folder.write("noname.page", "[Ilia Page]\nName=  \nItems=a\n");
        folder.write("nogroup.page", "[Other]\nName=X\nItems=a\n");
        folder.write(
            "cmd.page",
            &page("ItemsCommand=echo b\nUnknown=1\n[Other]\nItems=x\n"),
        );
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].rows, Rows::Command("echo b".to_owned()));
        assert_eq!(warnings.len(), 4, "{warnings:?}");
    }

    #[test]
    fn settings_are_read_with_their_fallbacks() {
        let folder = Folder::new("settings");
        let file = folder.write(
            "a.page",
            &page(
                "Items=x\nKey=Q\nOnlyShowIn=;; SWAY ;\nNotShowIn=Other\n\
                 Exec=probe \"two words\" %s\nSort=  FrEqUeNcY \n",
            ),
        );
        let bad = folder.write(
            "b.page",
            &page("Items=x\nKey=q1\nExec=probe \"unclosed\nSort=random\nIcon=custom\n"),
        );
        let apps = folder.write("c.page", &page("Apps=a.desktop\nExec=\"bad\nSort=random\n"));
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        assert_eq!(pages.len(), 3);
        let a = &pages[0];
        assert_eq!(a.path, file);
        assert_eq!(a.key, Some('q'));
        assert_eq!(a.icon, super::DEFAULT_ICON);
        assert!(a.shows_in("sway") && !a.shows_in("i3") && !a.shows_in("other"));
        assert_eq!(
            a.exec,
            Exec::Words(vec!["probe".into(), "two words".into(), "%s".into()])
        );
        assert!(a.frequency);
        let b = &pages[1];
        assert_eq!((b.key, &b.exec, b.frequency), (None, &Exec::Invalid, false));
        assert_eq!(b.icon, "custom");
        assert!(b.shows_in("other"));
        let c = &pages[2];
        assert_eq!((&c.exec, c.frequency), (&Exec::Print, false));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings
            .iter()
            .all(|line| line.contains(&bad.display().to_string())));
        assert!(!warnings
            .iter()
            .any(|line| line.contains(&apps.display().to_string())));
    }

    #[test]
    fn unknown_escapes_stay_as_written() {
        assert_eq!(unescape(r"printf 'caf\351\n'"), "printf 'caf\\351\n'");
        assert_eq!(unescape(r"a\sb\tc\\d\"), "a b\tc\\d\\");
        let folder = Folder::new("escapes");
        folder.write("raw.page", &page("ItemsCommand=printf 'caf\\351\\n'\n"));
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            pages[0].rows,
            Rows::Command("printf 'caf\\351\n'".to_owned())
        );
    }
}
