//! Pages defined in key files: `ilia/pages/*.page` under the user's
//! configuration home, then under each shared configuration directory.

use std::collections::{HashMap, HashSet};
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
    /// A tree of entries: submenus, window-manager bindings and commands.
    Menu(Menu),
}

/// The entries of a menu page, read from its `[Entry <id>]` groups.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Menu {
    /// Every usable entry of the tree.
    pub entries: Vec<MenuEntry>,
    /// The top-level entries, by index into `entries`, in listed order.
    pub top: Vec<usize>,
}

/// One usable entry of a menu page.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuEntry {
    pub id: String,
    /// Plain text, never markup.
    pub name: String,
    /// The icon, when the entry names one; rows fall back to the page's.
    pub icon: Option<String>,
    pub kind: EntryKind,
    /// The command whose success shows the entry.
    pub when: Option<Words>,
    /// The command whose first line gives the entry's on/off state.
    pub state: Option<Words>,
    only_show_in: Vec<String>,
    not_show_in: Vec<String>,
}

/// The words of a command line, or `None` when the line cannot be split
/// into any.
pub type Words = Option<Vec<String>>;

/// What an entry is.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryKind {
    /// A submenu: its entries, by index into the menu's entries, in order.
    Submenu(Vec<usize>),
    /// A window-manager binding, named by its action, possibly qualified by
    /// its category as `Category // Action`.
    Binding(String),
    /// A command line, already split into words.
    Exec(Words),
}

impl MenuEntry {
    /// Whether the entry shows under the window manager `manager`: `i3`,
    /// `sway` or `other`.
    pub fn shows_in(&self, manager: &str) -> bool {
        shows_in(&self.only_show_in, &self.not_show_in, manager)
    }
}

impl Menu {
    /// The submenus `path` names, `<entry id>[/<entry id>...]` with each id
    /// a submenu directly under the previous one (the first one at the top)
    /// and shown under the window manager `manager`; `None` when it names
    /// no such chain.
    pub fn resolve(&self, path: &str, manager: &str) -> Option<Vec<usize>> {
        let mut level = &self.top;
        let mut chain = Vec::new();
        for id in path.split('/') {
            let index = level
                .iter()
                .copied()
                .find(|&index| self.entries[index].id == id)?;
            let entry = &self.entries[index];
            let EntryKind::Submenu(children) = &entry.kind else {
                return None;
            };
            if !entry.shows_in(manager) {
                return None;
            }
            chain.push(index);
            level = children;
        }
        Some(chain)
    }

    /// The entries listed directly under the submenu at the end of
    /// `chain`, or at the top for an empty chain.
    pub fn children(&self, chain: &[usize]) -> &[usize] {
        match chain.last().map(|&index| &self.entries[index].kind) {
            Some(EntryKind::Submenu(children)) => children,
            Some(_) => &[],
            None => &self.top,
        }
    }
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
        shows_in(&self.only_show_in, &self.not_show_in, manager)
    }

    /// The menu of a menu page.
    pub fn menu(&self) -> Option<&Menu> {
        match &self.rows {
            Rows::Menu(menu) => Some(menu),
            _ => None,
        }
    }
}

/// Whether `OnlyShowIn` and `NotShowIn` values, already lower-cased, admit
/// the window manager `manager`.
fn shows_in(only_show_in: &[String], not_show_in: &[String], manager: &str) -> bool {
    let listed = |names: &[String]| names.iter().any(|name| name == manager);
    (only_show_in.is_empty() || listed(only_show_in)) && !listed(not_show_in)
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
    group_string(keyfile, GROUP, key)
}

/// The string value of `key` in `group`, read like [`string_value`].
fn group_string(keyfile: &glib::KeyFile, group: &str, key: &str) -> Option<String> {
    match keyfile.string(group, key) {
        Ok(value) => Some(value.into()),
        Err(error) if error.matches(glib::KeyFileError::InvalidValue) => keyfile
            .value(group, key)
            .ok()
            .map(|raw| unescape(raw.as_str())),
        Err(_) => None,
    }
}

/// The items of the list `key` in `group`, without the empty ones.
fn group_list(keyfile: &glib::KeyFile, group: &str, key: &str) -> Vec<String> {
    keyfile
        .string_list(group, key)
        .map(|items| {
            items
                .iter()
                .filter(|item| !item.is_empty())
                .map(|item| item.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The window manager names of the show-in list `key` in `group`, trimmed
/// and lower-cased.
fn managers(keyfile: &glib::KeyFile, group: &str, key: &str) -> Vec<String> {
    group_list(keyfile, group, key)
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// A command line split into words with the shell-like quoting page `Exec`
/// keys use, never to be handed to a shell; `None` when it cannot be split.
fn split_command(line: &str) -> Words {
    glib::shell_parse_argv(line).ok().map(|words| {
        words
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    })
}

/// Whether `id` may name a menu entry: ASCII letters, digits, `-` and `_`,
/// at least one of them.
fn is_entry_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The prefix of the group names of menu entries.
const ENTRY_GROUP: &str = "Entry ";

/// Checks every `Entries` list of a menu page file, the page's own (`top`)
/// and those of all its entry groups, reachable from the top or not and
/// whether or not their entries are usable: an id that occupies two
/// positions, or a submenu that a chain of lists leads back to, makes the
/// whole file unusable (the error).
fn check_entry_lists(keyfile: &glib::KeyFile, top: &[String]) -> Result<(), String> {
    let mut lists = vec![(None, top.to_vec())];
    for group in keyfile.groups().iter() {
        let group = group.as_str();
        if let Some(id) = group.strip_prefix(ENTRY_GROUP) {
            if keyfile.has_key(group, "Entries").unwrap_or(false) {
                lists.push((Some(id.to_owned()), group_list(keyfile, group, "Entries")));
            }
        }
    }
    let mut listed = HashSet::new();
    for id in lists.iter().flat_map(|(_, ids)| ids) {
        if !listed.insert(id.as_str()) {
            return Err(format!("entry '{id}' is listed more than once"));
        }
    }
    // With no id listed twice, every id has at most one parent, so a chain
    // of lists can only come back to where it started by going round a
    // loop: follow the parents up from each list.
    let parents: HashMap<&str, &str> = lists
        .iter()
        .filter_map(|(owner, ids)| Some((owner.as_deref()?, ids)))
        .flat_map(|(owner, ids)| ids.iter().map(move |id| (id.as_str(), owner)))
        .collect();
    for (owner, _) in &lists {
        let Some(start) = owner.as_deref() else {
            continue;
        };
        let mut current = start;
        for _ in 0..parents.len() {
            let Some(&parent) = parents.get(current) else {
                break;
            };
            if parent == start {
                return Err(format!("entry '{start}' lists a submenu that contains it"));
            }
            current = parent;
        }
    }
    Ok(())
}

/// Reads the menu of a page file whose top-level entries are `top`. An
/// entry that cannot be used is left out with a warning, its siblings
/// kept; an id listed twice, or a submenu that contains itself, makes the
/// whole file unusable (the error). Groups no list names are not read
/// beyond their `Entries` list.
fn parse_menu(
    keyfile: &glib::KeyFile,
    path: &Path,
    top: &[String],
    warnings: &mut Vec<String>,
) -> Result<Menu, String> {
    struct Reader<'a> {
        keyfile: &'a glib::KeyFile,
        path: &'a Path,
        entries: Vec<MenuEntry>,
        warnings: Vec<String>,
    }

    impl Reader<'_> {
        /// The usable entries of the list `ids`.
        fn list(&mut self, ids: &[String]) -> Vec<usize> {
            let mut indices = Vec::new();
            for id in ids {
                match self.entry(id) {
                    Ok(index) => indices.push(index),
                    Err(problem) => self.warnings.push(warning(
                        self.path,
                        &format!("entry '{id}' {problem}; it is left out"),
                    )),
                }
            }
            indices
        }

        /// Reads the entry `id`: its index, or what keeps it out.
        fn entry(&mut self, id: &str) -> Result<usize, &'static str> {
            if !is_entry_id(id) {
                return Err(
                    "has an id with characters other than ASCII letters, digits, '-' and '_'",
                );
            }
            let group = format!("{ENTRY_GROUP}{id}");
            if !self.keyfile.has_group(&group) {
                return Err("has no group of its own");
            }
            let has = |key: &str| self.keyfile.has_key(&group, key).unwrap_or(false);
            let string = |key: &str| group_string(self.keyfile, &group, key);
            let Some(name) = string("Name").filter(|name| !name.trim().is_empty()) else {
                return Err("has no Name or a blank one");
            };
            let kinds = ["Entries", "Binding", "Exec"]
                .into_iter()
                .filter(|key| has(key))
                .collect::<Vec<_>>();
            if kinds.len() != 1 {
                return Err("needs exactly one of Entries, Binding and Exec");
            }
            let command = |key: &str| string(key).map(|line| split_command(&line));
            let kind = match kinds[0] {
                "Binding" => {
                    EntryKind::Binding(string("Binding").unwrap_or_default().trim().to_owned())
                }
                "Exec" => EntryKind::Exec(string("Exec").and_then(|line| {
                    split_command(&line).map(|words| {
                        // `%%` is a literal `%`; nothing else is replaced.
                        words.iter().map(|word| word.replace("%%", "%")).collect()
                    })
                })),
                _ => {
                    // The lists hold no loop (see `check_entry_lists`), so
                    // this ends.
                    let children = group_list(self.keyfile, &group, "Entries");
                    EntryKind::Submenu(self.list(&children))
                }
            };
            let entry = MenuEntry {
                id: id.to_owned(),
                name,
                icon: string("Icon").filter(|icon| !icon.trim().is_empty()),
                kind,
                when: command("When"),
                state: command("State"),
                only_show_in: managers(self.keyfile, &group, "OnlyShowIn"),
                not_show_in: managers(self.keyfile, &group, "NotShowIn"),
            };
            if matches!(entry.kind, EntryKind::Exec(None)) {
                self.warnings.push(warning(
                    self.path,
                    &format!(
                        "entry '{id}': Exec cannot be split into words; choosing it does nothing"
                    ),
                ));
            }
            self.entries.push(entry);
            Ok(self.entries.len() - 1)
        }
    }

    check_entry_lists(keyfile, top)?;
    let mut reader = Reader {
        keyfile,
        path,
        entries: Vec::new(),
        warnings: Vec::new(),
    };
    let top = reader.list(top);
    // The warnings of a file that turns out unusable are not printed: the
    // file is skipped with one line.
    warnings.append(&mut reader.warnings);
    Ok(Menu {
        entries: reader.entries,
        top,
    })
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
    let list = |key: &str| group_list(&keyfile, GROUP, key);

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
    // A menu is a source of rows even when it lists no entry.
    let entries = keyfile
        .has_key(GROUP, "Entries")
        .unwrap_or(false)
        .then(|| list("Entries"));
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
    if entries.is_some() {
        sources.push(Rows::Menu(Menu::default()));
    }
    // Next to `Entries`, naming another source is a conflict even when its
    // value lists nothing.
    let named = |key: &str| keyfile.has_key(GROUP, key).unwrap_or(false);
    let conflicts = entries.is_some() && ["Apps", "Items", "ItemsCommand"].into_iter().any(named);
    if sources.len() != 1 || conflicts {
        return Err(
            "exactly one of Apps, Items, ItemsCommand and Entries must list rows".to_owned(),
        );
    }
    let rows = match (sources.remove(0), entries) {
        (Rows::Menu(_), Some(top)) => Rows::Menu(parse_menu(&keyfile, path, &top, warnings)?),
        (rows, _) => rows,
    };

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
    let managers = |key: &str| managers(&keyfile, GROUP, key);

    // Rows of applications launch them, and menu entries say what they
    // do: Exec and Sort mean nothing there.
    let rows_act_alone = matches!(rows, Rows::Apps(_) | Rows::Menu(_));
    let exec = match string("Exec").filter(|exec| !exec.trim().is_empty()) {
        Some(exec) if !rows_act_alone => match glib::shell_parse_argv(exec.as_str()) {
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
        Some(sort) if !rows_act_alone => {
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
    use super::{discover_in, identity, unescape, EntryKind, Exec, Menu, Rows};
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

    /// The menu of the only page `text` defines in a fresh folder, and the
    /// warnings; `None` when the file is unusable.
    fn menu(name: &str, text: &str) -> (Option<Menu>, Vec<String>) {
        let folder = Folder::new(name);
        folder.write("menu.page", text);
        let (pages, warnings) = discover_in(&[folder.path().to_owned()], RESERVED);
        (
            pages.first().and_then(|page| page.menu().cloned()),
            warnings,
        )
    }

    fn top_ids(menu: &Menu) -> Vec<&str> {
        menu.top
            .iter()
            .map(|&index| menu.entries[index].id.as_str())
            .collect()
    }

    #[test]
    fn entries_list_a_menu_in_order() {
        let (menu, warnings) = menu(
            "menu-order",
            &page(concat!(
                "Entries=;;b_2;;a-1;;sub;\nExec='unclosed\nSort=random\n",
                "[Entry a-1]\nName=A\nExec=run 'two words' 50%% %s\nIcon=icon-a\n",
                "[Entry b_2]\nName=B\nBinding=  Cat // Act  \nOnlyShowIn=; SWAY ;\n",
                "[Entry sub]\nName=Sub <b>\nEntries=\nWhen=check \"x y\"\nState=state\n",
                "[Entry unused]\nName=\n",
            )),
        );
        // Exec and Sort mean nothing on a menu page.
        assert!(warnings.is_empty(), "{warnings:?}");
        let menu = menu.unwrap();
        assert_eq!(top_ids(&menu), ["b_2", "a-1", "sub"]);
        let entry = |id: &str| menu.entries.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(
            entry("a-1").kind,
            EntryKind::Exec(Some(vec![
                "run".into(),
                "two words".into(),
                "50%".into(),
                "%s".into()
            ]))
        );
        assert_eq!(entry("a-1").icon.as_deref(), Some("icon-a"));
        assert_eq!(entry("b_2").kind, EntryKind::Binding("Cat // Act".into()));
        assert!(entry("b_2").shows_in("sway") && !entry("b_2").shows_in("i3"));
        assert_eq!(entry("sub").name, "Sub <b>");
        assert_eq!(entry("sub").kind, EntryKind::Submenu(Vec::new()));
        assert_eq!(
            entry("sub").when,
            Some(Some(vec!["check".into(), "x y".into()]))
        );
        assert_eq!(entry("sub").state, Some(Some(vec!["state".into()])));
        assert_eq!(entry("sub").icon, None);
        // Groups no list names are not read.
        assert!(menu.entries.iter().all(|entry| entry.id != "unused"));
    }

    #[test]
    fn an_empty_entries_list_is_still_a_menu_and_a_source() {
        let (empty, warnings) = menu("menu-empty", &page("Entries=;;\n"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(empty.unwrap(), Menu::default());
        // Naming another source next to Entries is a conflict, even with
        // nothing listed in it.
        for other in [
            "Items=a",
            "Apps=a.desktop",
            "ItemsCommand=echo a",
            "Items=",
            "Apps=;",
            "ItemsCommand=",
        ] {
            let (clash, warnings) = menu("menu-clash", &page(&format!("Entries=\n{other}\n")));
            assert!(clash.is_none(), "{other}");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
        }
    }

    #[test]
    fn unusable_entries_are_left_out_one_line_each() {
        let (menu, warnings) = menu(
            "menu-skip",
            &page(concat!(
                "Entries=ghost;we.ird;blank;none;two;good;sub\n",
                "[Entry we.ird]\nName=W\nExec=x\n",
                "[Entry blank]\nName=  \nExec=x\n",
                "[Entry none]\nName=N\n",
                "[Entry two]\nName=T\nEntries=\nExec=x\n",
                "[Entry good]\nName=G\nExec=x\n",
                "[Entry sub]\nName=S\nEntries=missing;child\n",
                "[Entry child]\nName=C\nExec=x\n",
            )),
        );
        let menu = menu.unwrap();
        assert_eq!(top_ids(&menu), ["good", "sub"]);
        assert_eq!(menu.resolve("sub", "i3").map(|chain| chain.len()), Some(1));
        let skipped = ["ghost", "we.ird", "blank", "none", "two", "missing"];
        assert_eq!(warnings.len(), skipped.len(), "{warnings:?}");
        for (line, id) in warnings.iter().zip(skipped) {
            assert!(
                line.contains(&format!("'{id}'")) && line.contains("menu.page"),
                "{line}"
            );
        }
    }

    #[test]
    fn repeated_ids_and_cycles_make_the_file_unusable() {
        for (tree, id) in [
            ("Entries=a;a\n[Entry a]\nName=A\nExec=x\n", "a"),
            (
                "Entries=l;r\n[Entry l]\nName=L\nEntries=d\n[Entry r]\nName=R\nEntries=d\n[Entry d]\nName=D\nExec=x\n",
                "d",
            ),
            ("Entries=a\n[Entry a]\nName=A\nEntries=b\n[Entry b]\nName=B\nEntries=a\n", "a"),
            ("Entries=a\n[Entry a]\nName=A\nEntries=ghost;bad;a\n[Entry bad]\nName=B\n", "a"),
            // Every list in the file counts: under entries that are left
            // out, and in groups no chain from the top reaches.
            ("Entries=bad;d\n[Entry bad]\nEntries=d\n[Entry d]\nName=D\nExec=x\n", "d"),
            (
                "Entries=bad;d\n[Entry bad]\nName=B\nEntries=d\nExec=x\n[Entry d]\nName=D\nExec=x\n",
                "d",
            ),
            ("Entries=d\n[Entry d]\nName=D\nExec=x\n[Entry orphan]\nEntries=d\n", "d"),
            (
                "Entries=g\n[Entry g]\nName=G\nExec=x\n[Entry x]\nName=X\nEntries=y\n[Entry y]\nName=Y\nEntries=x\n",
                "x",
            ),
            ("Entries=g\n[Entry g]\nName=G\nExec=x\n[Entry s]\nEntries=s\n", "s"),
        ] {
            let (menu, warnings) = menu("menu-reject", &page(tree));
            assert!(menu.is_none(), "{tree}");
            // One line, without the warnings of the entries read before.
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains(&format!("'{id}'")), "{warnings:?}");
        }
    }

    #[test]
    fn links_name_shown_submenus_directly_under_each_other() {
        let (menu, _) = menu(
            "menu-links",
            &page(concat!(
                "Entries=p;hidden\n",
                "[Entry p]\nName=P\nEntries=n;leaf\n",
                "[Entry n]\nName=N\nEntries=\n",
                "[Entry leaf]\nName=L\nExec=x\n",
                "[Entry hidden]\nName=H\nEntries=\nNotShowIn=i3\n",
            )),
        );
        let menu = menu.unwrap();
        let ids = |path: &str, manager: &str| {
            menu.resolve(path, manager).map(|chain| {
                chain
                    .iter()
                    .map(|&index| menu.entries[index].id.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(ids("p/n", "i3"), Some(vec!["p".to_owned(), "n".to_owned()]));
        assert_eq!(ids("n", "i3"), None);
        assert_eq!(ids("P", "i3"), None);
        assert_eq!(ids("p/leaf", "i3"), None);
        assert_eq!(ids("p/", "i3"), None);
        assert_eq!(ids("hidden", "i3"), None);
        assert_eq!(ids("hidden", "sway"), Some(vec!["hidden".to_owned()]));
        assert_eq!(menu.children(&[]), menu.top.as_slice());
    }
}
