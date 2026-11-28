//! Reading the installed desktop entries so that one slow or stuck file does
//! not hold up the others.
//!
//! GLib's own readers (`g_app_info_get_all()`, the GDesktopAppInfo
//! constructors and `g_desktop_app_info_search()`) read entry files while
//! holding one process-wide lock. A file whose `open()` never returns, on a
//! hung network mount for instance, would then block every desktop-entry
//! call in the process for good. Here each file is first read outside GLib,
//! on a pool of reader threads that replaces readers which get stuck. GLib
//! only parses files that have already been read in full.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use glib::translate::{FromGlibPtrContainer, ToGlibPtr};

const GROUP: &str = "Desktop Entry";

/// The keys GLib's search matches, with its ranking: lower is better.
const SEARCH_KEYS: [(&str, u8); 5] = [
    ("Name", 1),
    ("Exec", 2),
    ("Keywords", 3),
    ("GenericName", 4),
    ("X-GNOME-FullName", 5),
];

/// Programs GLib's search does not match an Exec line by: interpreters and
/// wrappers that many entries start.
const EXEC_BLOCKLIST: [&str; 12] = [
    "bash", "env", "flatpak", "snap", "gjs", "pkexec", "python", "python2", "python3", "sh",
    "wine", "wine64",
];

/// Match types, in GLib's order: any prefix match ranks before a substring
/// match, whatever the key.
const PREFIX: u8 = 1;
const SUBSTRING: u8 = 2;

/// Readers started at once.
const READERS: usize = 4;
/// Readers there can be in all, counting stuck ones.
const MAX_READERS: usize = 64;
/// How long one read may take before its reader counts as stuck, and
/// another reader takes over the files still queued.
const STALL: Duration = Duration::from_millis(250);
/// How often stuck readers are looked for while the list is not yet due.
const POLL: Duration = Duration::from_millis(50);

/// A desktop file GLib would list, with its desktop ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFile {
    pub id: String,
    pub path: PathBuf,
}

/// The `applications` directories GLib reads, highest precedence first.
pub fn application_dirs() -> Vec<PathBuf> {
    std::iter::once(glib::user_data_dir())
        .chain(glib::system_data_dirs())
        .map(|dir| dir.join("applications"))
        .collect()
}

/// Every desktop file in `dirs`, as GLib finds them: a subdirectory adds its
/// name and a dash to the IDs of the files in it, and an ID found in an
/// earlier directory hides the same ID in later ones, whether or not that
/// earlier file can be read. Names that are not UTF-8 are skipped.
pub fn entry_files(dirs: &[PathBuf]) -> Vec<EntryFile> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        collect_entry_files(dir, "", &mut found, &mut HashMap::new());
        let ids: Vec<String> = found.iter().map(|file| file.id.clone()).collect();
        files.extend(found.into_iter().filter(|file| !seen.contains(&file.id)));
        seen.extend(ids);
    }
    files
}

fn collect_entry_files(
    dir: &Path,
    prefix: &str,
    found: &mut Vec<EntryFile>,
    positions: &mut HashMap<String, usize>,
) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    for item in listing.flatten() {
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = dir.join(name);
        if name.ends_with(".desktop") {
            let id = format!("{prefix}{name}");
            // Within one directory tree the file listed last wins, as in
            // GLib's table of names.
            match positions.get(&id) {
                Some(&index) => found[index].path = path,
                None => {
                    positions.insert(id.clone(), found.len());
                    found.push(EntryFile { id, path });
                }
            }
        } else if path.is_dir() {
            collect_entry_files(&path, &format!("{prefix}{name}-"), found, positions);
        }
    }
}

/// One word of an entry that GLib's search matches against.
#[derive(Debug)]
struct SearchTerm {
    token: String,
    category: u8,
    position: usize,
}

/// A desktop entry that GLib would list: one that is not hidden.
pub struct DesktopEntry {
    pub id: String,
    /// `None` when GLib does not accept the entry as an application (its
    /// program is missing, for one). GLib still finds it by search.
    pub app: Option<gio::DesktopAppInfo>,
    /// The actions the entry declares, read with the file.
    pub actions: Vec<DesktopAction>,
    search_terms: Vec<SearchTerm>,
}

/// One of the actions a desktop entry declares: an identifier its
/// `Actions` key names, with a `[Desktop Action <id>]` group behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopAction {
    pub id: String,
    /// The name in the user's language; never empty.
    pub name: String,
    /// The group's Exec line, the key file escapes undone; `None` when it
    /// is missing or empty.
    pub exec: Option<String>,
}

/// The actions `keyfile` declares, in the order of its `Actions` key: each
/// identifier at its first position, with a group of its own and a name.
/// Empty identifiers, identifiers without a group, groups without a name
/// or with an empty one, and groups the key does not name are skipped.
/// Whether an action needs an Exec line depends on how its application is
/// started, so that is left to the caller.
pub fn desktop_actions(keyfile: &glib::KeyFile) -> Vec<DesktopAction> {
    desktop_actions_for(keyfile, user_locales())
}

/// [`desktop_actions`], with names in the language of `locales` (see
/// [`locale_forms`]).
fn desktop_actions_for(keyfile: &glib::KeyFile, locales: &[Vec<String>]) -> Vec<DesktopAction> {
    let Ok(ids) = keyfile.string_list(GROUP, "Actions") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut actions = Vec::new();
    for id in ids.iter().map(|id| id.as_str()) {
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        let group = format!("Desktop Action {id}");
        if !keyfile.has_group(&group) {
            continue;
        }
        // A translation that is there but empty counts as no name.
        let Some(name) = localized("Name", locales, |key| {
            keyfile.string(&group, key).ok().map(String::from)
        })
        .filter(|name| !name.is_empty()) else {
            continue;
        };
        let exec = keyfile
            .string(&group, "Exec")
            .ok()
            .filter(|exec| !exec.is_empty());
        actions.push(DesktopAction {
            id: id.to_owned(),
            name,
            exec: exec.map(String::from),
        });
    }
    actions
}

/// The variables that can name the user's language, in the order they are
/// consulted: the first one set and not empty is the only one used.
const LOCALE_VARIABLES: [&str; 4] = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];

/// The locales localized names are looked up for, in order, each as the
/// key suffixes to try for it (see [`key_forms`]). `variable` gives the
/// value of an environment variable. `LANGUAGE` holds a colon-separated
/// list; the other variables hold one locale.
fn locale_forms(variable: impl Fn(&str) -> Option<String>) -> Vec<Vec<String>> {
    let Some((name, value)) = LOCALE_VARIABLES.into_iter().find_map(|name| {
        variable(name)
            .filter(|value| !value.is_empty())
            .map(|value| (name, value))
    }) else {
        return Vec::new();
    };
    let locales: Vec<&str> = if name == "LANGUAGE" {
        value.split(':').collect()
    } else {
        vec![value.as_str()]
    };
    locales
        .into_iter()
        .map(key_forms)
        .filter(|forms| !forms.is_empty())
        .collect()
}

/// The key suffixes for the locale `locale`, as the Desktop Entry
/// specification orders them: `lang_COUNTRY@MODIFIER`, `lang_COUNTRY`,
/// `lang@MODIFIER`, `lang`, each only when the locale has its parts. The
/// `.ENCODING` part plays no role.
fn key_forms(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier).filter(|m| !m.is_empty())),
        None => (locale, None),
    };
    let rest = rest.split('.').next().unwrap_or_default();
    let (language, country) = match rest.split_once('_') {
        Some((language, country)) => (language, Some(country).filter(|c| !c.is_empty())),
        None => (rest, None),
    };
    if language.is_empty() {
        return Vec::new();
    }
    let mut forms = Vec::new();
    if let Some(country) = country {
        if let Some(modifier) = modifier {
            forms.push(format!("{language}_{country}@{modifier}"));
        }
        forms.push(format!("{language}_{country}"));
    }
    if let Some(modifier) = modifier {
        forms.push(format!("{language}@{modifier}"));
    }
    forms.push(language.to_owned());
    forms
}

/// The user's locales (see [`locale_forms`]), read from the environment
/// once.
fn user_locales() -> &'static [Vec<String>] {
    static LOCALES: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
    LOCALES.get_or_init(|| locale_forms(|name| std::env::var(name).ok()))
}

/// The value of the localized key `key` for `locales`: the first of
/// `key[<form>]` present, trying every form of one locale before the next
/// locale, and the plain `key` after the last. `value` gives a key's value,
/// `None` when the key is missing. A value that is present but empty ends
/// the search too.
fn localized(
    key: &str,
    locales: &[Vec<String>],
    value: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    locales
        .iter()
        .flatten()
        .find_map(|form| value(&format!("{key}[{form}]")))
        .or_else(|| value(key))
}

/// The name of the app `desktop` in the user's language, looked up as the
/// names of actions are; GLib's name when that gives none.
pub fn app_name(desktop: &gio::DesktopAppInfo) -> String {
    use gio::prelude::AppInfoExt;
    localized("Name", user_locales(), |key| {
        desktop.string(key).map(String::from)
    })
    .filter(|name| !name.is_empty())
    .or_else(|| desktop.string("Name").map(String::from))
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| desktop.name().into())
}

/// The actions the desktop file at `path` declares (see
/// [`desktop_actions`]); none when it cannot be read.
pub fn desktop_actions_of_file(path: &Path) -> Vec<DesktopAction> {
    let keyfile = glib::KeyFile::new();
    match keyfile.load_from_file(path, glib::KeyFileFlags::KEEP_TRANSLATIONS) {
        Ok(()) => desktop_actions(&keyfile),
        Err(_) => Vec::new(),
    }
}

impl DesktopEntry {
    /// How well the entry matches every one of `words`, ranked as GLib
    /// ranks search results (lower is better); `None` when a word does not
    /// match.
    fn rank(&self, words: &[String]) -> Option<(u8, u8, usize)> {
        words.iter().try_fold((0, 0, 0), |worst, word| {
            let best = self
                .search_terms
                .iter()
                .filter_map(|term| {
                    let at = term.token.find(word.as_str())?;
                    let match_type = if at == 0 { PREFIX } else { SUBSTRING };
                    Some((match_type, term.category, term.position))
                })
                .min()?;
            Some((
                worst.0.max(best.0),
                worst.1.max(best.1),
                worst.2.max(best.2),
            ))
        })
    }
}

/// A desktop file read and parsed outside GLib, on a reader thread.
pub struct ReadFile {
    file: EntryFile,
    actions: Vec<DesktopAction>,
    search_terms: Vec<SearchTerm>,
}

/// Reads and parses one desktop file: the only step that can wait on the
/// file system. `None` when the file cannot be read, is not a valid key file
/// or is hidden, which are the files GLib leaves out.
pub fn read_entry(file: EntryFile) -> Option<ReadFile> {
    let bytes = read_regular_file(&file.path)?;
    let keyfile = glib::KeyFile::new();
    keyfile
        .load_from_bytes(
            &glib::Bytes::from_owned(bytes),
            glib::KeyFileFlags::KEEP_TRANSLATIONS,
        )
        .ok()?;
    if keyfile.boolean(GROUP, "Hidden").unwrap_or(false) {
        return None;
    }
    let search_terms = search_terms(&keyfile);
    let actions = desktop_actions(&keyfile);
    Some(ReadFile {
        file,
        actions,
        search_terms,
    })
}

impl ReadFile {
    /// Builds the entry GLib lists for the file. GLib reads the file again,
    /// under its lock; it has just been read in full, so this does not wait
    /// on a stalled file. Build entries on one thread: with GLib's lock,
    /// building on several threads at once is slower, not faster.
    pub fn into_entry(self) -> DesktopEntry {
        let app = gio::DesktopAppInfo::from_filename(&self.file.path);
        DesktopEntry {
            id: self.file.id,
            app,
            actions: self.actions,
            search_terms: self.search_terms,
        }
    }
}

/// Reads one desktop file and builds its entry, on the calling thread.
pub fn load_entry(file: EntryFile) -> Option<DesktopEntry> {
    read_entry(file).map(ReadFile::into_entry)
}

/// The file's contents, if it is a regular file: GLib's key-file loader
/// rejects anything else after opening it.
fn read_regular_file(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The words GLib's search index holds for an entry, from the entry's
/// strings for the current locale.
fn search_terms(keyfile: &glib::KeyFile) -> Vec<SearchTerm> {
    let mut terms = Vec::new();
    for (key, category) in SEARCH_KEYS {
        let Ok(value) = keyfile.locale_string(GROUP, key, None) else {
            continue;
        };
        let value = if key == "Exec" {
            match exec_program(&value) {
                Some(program) => program,
                None => continue,
            }
        } else {
            value.as_str()
        };
        let (tokens, alternates) = tokenize_and_fold(value);
        terms.extend(
            tokens
                .into_iter()
                .chain(alternates)
                .enumerate()
                .map(|(position, token)| SearchTerm {
                    token,
                    category,
                    position,
                }),
        );
    }
    terms
}

/// The program an Exec line starts, without its directory, unless GLib's
/// search ignores that program.
fn exec_program(exec: &str) -> Option<&str> {
    let program = exec.split([' ', '\t', '\n']).next().unwrap_or_default();
    let program = program.rsplit('/').next().unwrap_or(program);
    (!EXEC_BLOCKLIST.contains(&program)).then_some(program)
}

/// GLib's search words for `text`, normalised and case-folded, and the
/// ASCII spellings of those that have one.
fn tokenize_and_fold(text: &str) -> (Vec<String>, Vec<String>) {
    let mut alternates = std::ptr::null_mut();
    // SAFETY: the text is a valid C string for the duration of the call.
    // GLib returns two newly allocated NULL-terminated string arrays, and
    // ownership of both is taken here.
    let (tokens, alternates): (Vec<glib::GString>, Vec<glib::GString>) = unsafe {
        let tokens = glib::ffi::g_str_tokenize_and_fold(
            text.to_glib_none().0,
            std::ptr::null(),
            &mut alternates,
        );
        (
            FromGlibPtrContainer::from_glib_full(tokens),
            FromGlibPtrContainer::from_glib_full(alternates),
        )
    };
    (
        tokens.into_iter().map(String::from).collect(),
        alternates.into_iter().map(String::from).collect(),
    )
}

/// The entry `g_desktop_app_info_search()` would rank first for `query`.
/// GLib leaves the order of equally good matches open; here the lowest
/// desktop ID comes first.
pub fn best_match<'a>(
    entries: impl IntoIterator<Item = &'a DesktopEntry>,
    query: &str,
) -> Option<&'a DesktopEntry> {
    let (words, _) = tokenize_and_fold(query);
    if words.is_empty() {
        return None;
    }
    entries
        .into_iter()
        .filter_map(|entry| Some((entry.rank(&words)?, entry)))
        .min_by(|(a_rank, a), (b_rank, b)| a_rank.cmp(b_rank).then_with(|| a.id.cmp(&b.id)))
        .map(|(_, entry)| entry)
}

/// What [`read_all`] hands over.
pub enum Delivery<T> {
    /// The list: everything read by the time it was due.
    List(Vec<T>),
    /// An item whose read was still stuck when the list was handed over.
    Late(T),
}

/// The read a reader is working on.
struct Busy {
    since: Instant,
    /// Another reader has been started to take over from this one.
    replaced: bool,
}

#[derive(Default)]
struct Reader {
    busy: Mutex<Option<Busy>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Pool<I, T> {
    queue: Arc<Mutex<VecDeque<I>>>,
    read: Arc<dyn Fn(I) -> Option<T> + Send + Sync>,
    results: mpsc::Sender<Option<T>>,
    readers: Vec<Arc<Reader>>,
}

impl<I: Send + 'static, T: Send + 'static> Pool<I, T> {
    /// Starts another reader. Returns false if its thread cannot be created.
    fn spawn(&mut self) -> bool {
        let reader = Arc::new(Reader::default());
        let state = reader.clone();
        let queue = self.queue.clone();
        let read = self.read.clone();
        let results = self.results.clone();
        let spawned = std::thread::Builder::new()
            .name("ilia-entries".to_owned())
            .spawn(move || loop {
                let Some(item) = lock(&queue).pop_front() else {
                    break;
                };
                *lock(&state.busy) = Some(Busy {
                    since: Instant::now(),
                    replaced: false,
                });
                let result = read(item);
                *lock(&state.busy) = None;
                if results.send(result).is_err() {
                    break;
                }
            });
        if spawned.is_ok() {
            self.readers.push(reader);
        }
        spawned.is_ok()
    }

    /// Counts the readers stuck at `now`, starting a reader in place of each
    /// newly stuck one while files are still queued.
    fn replace_stuck(&mut self, now: Instant) -> usize {
        let mut stuck = 0;
        let mut newly_stuck = 0;
        for reader in &self.readers {
            if let Some(busy) = lock(&reader.busy).as_mut() {
                if now.duration_since(busy.since) >= STALL {
                    stuck += 1;
                    if !std::mem::replace(&mut busy.replaced, true) {
                        newly_stuck += 1;
                    }
                }
            }
        }
        if !lock(&self.queue).is_empty() {
            for _ in 0..newly_stuck {
                if self.readers.len() >= MAX_READERS || !self.spawn() {
                    break;
                }
            }
        }
        stuck
    }
}

/// Applies `read` to every item on a pool of reader threads, then `build`
/// to each result on the calling thread as it arrives, and hands the built
/// items to `deliver` there. The list comes once every item has been read
/// or, from `deadline` on, once the only reads left are stuck ones. Items
/// those reads return later come one at a time. Returns when every read has
/// finished, which may be never.
pub fn read_all<I, R, T>(
    items: Vec<I>,
    read: impl Fn(I) -> Option<R> + Send + Sync + 'static,
    mut build: impl FnMut(R) -> Option<T>,
    deadline: Instant,
    mut deliver: impl FnMut(Delivery<T>),
) where
    I: Send + 'static,
    R: Send + 'static,
{
    let total = items.len();
    let (results, received) = mpsc::channel();
    let mut pool = Pool {
        queue: Arc::new(Mutex::new(items.into())),
        read: Arc::new(read),
        results,
        readers: Vec::new(),
    };
    for _ in 0..READERS.min(total) {
        pool.spawn();
    }
    if pool.readers.is_empty() {
        // No thread could be started: read everything here.
        let items: Vec<I> = lock(&pool.queue).drain(..).collect();
        deliver(Delivery::List(
            items
                .into_iter()
                .filter_map(|item| (pool.read)(item))
                .filter_map(build)
                .collect(),
        ));
        return;
    }

    let mut list = Vec::new();
    let mut listed = false;
    let mut finished = 0;
    while finished < total {
        // Once the list is out nothing is queued: just wait for stuck reads.
        let next = if listed {
            received.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            received.recv_timeout(POLL)
        };
        match next {
            Ok(result) => {
                finished += 1;
                match result.and_then(&mut build) {
                    Some(item) if listed => deliver(Delivery::Late(item)),
                    Some(item) => list.push(item),
                    None => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if listed {
            continue;
        }
        let now = Instant::now();
        let stuck = pool.replace_stuck(now);
        let only_stuck_left =
            now >= deadline && lock(&pool.queue).is_empty() && finished + stuck == total;
        if finished == total || only_stuck_left {
            deliver(Delivery::List(std::mem::take(&mut list)));
            listed = true;
        }
    }
    if !listed {
        deliver(Delivery::List(list));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        application_dirs, best_match, desktop_actions, desktop_actions_for, entry_files,
        exec_program, key_forms, load_entry, locale_forms, read_all, search_terms, Delivery,
        DesktopEntry, EntryFile,
    };
    use crate::launch::{bus_name, command_line};
    use gio::prelude::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    /// A directory removed again when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ilia-entries-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ids(files: &[EntryFile]) -> Vec<(String, PathBuf)> {
        let mut ids: Vec<_> = files
            .iter()
            .map(|file| (file.id.clone(), file.path.clone()))
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn entry_files_follow_glib_ids_and_masking() {
        let scratch = Scratch::new("files");
        let high = scratch.0.join("high");
        let low = scratch.0.join("low");
        let shared = scratch.write("high/shared.desktop", "broken");
        let own = scratch.write("high/kde/own.desktop", "");
        scratch.write("high/notes.txt", "");
        scratch.write("low/shared.desktop", "");
        scratch.write("low/kde-own.desktop", "");
        let only_low = scratch.write("low/only-low.desktop", "");
        let nested = scratch.write("low/a/b/deep.desktop", "");
        let files = entry_files(&[high, low, scratch.0.join("missing")]);
        assert_eq!(
            ids(&files),
            vec![
                ("a-b-deep.desktop".to_owned(), nested),
                ("kde-own.desktop".to_owned(), own),
                ("only-low.desktop".to_owned(), only_low),
                ("shared.desktop".to_owned(), shared),
            ]
        );
    }

    fn entry(id: &str, lines: &str) -> DesktopEntry {
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(
                &format!("[Desktop Entry]\nType=Application\n{lines}"),
                glib::KeyFileFlags::KEEP_TRANSLATIONS,
            )
            .unwrap();
        DesktopEntry {
            id: id.to_owned(),
            app: None,
            actions: desktop_actions(&keyfile),
            search_terms: search_terms(&keyfile),
        }
    }

    fn actions(lines: &str) -> Vec<(String, String, Option<String>)> {
        entry("actions.desktop", lines)
            .actions
            .into_iter()
            .map(|action| (action.id, action.name, action.exec))
            .collect()
    }

    fn action(id: &str, name: &str, exec: Option<&str>) -> (String, String, Option<String>) {
        (id.to_owned(), name.to_owned(), exec.map(str::to_owned))
    }

    #[test]
    fn actions_follow_the_actions_key_and_need_a_group_and_a_name() {
        let found = actions(
            "Name=App\nExec=app\nActions=z;;ghost;a;z;noname;empty;bare;\n\
             [Desktop Action a]\nName=Alpha\nExec=app --a\n\
             [Desktop Action z]\nName=Zulu\nExec=app --z\n\
             [Desktop Action noname]\nExec=app\n\
             [Desktop Action empty]\nName=\nExec=app\n\
             [Desktop Action bare]\nName=Bare\nExec=\n\
             [Desktop Action unlisted]\nName=Unlisted\nExec=app\n",
        );
        assert_eq!(
            found,
            [
                action("z", "Zulu", Some("app --z")),
                action("a", "Alpha", Some("app --a")),
                action("bare", "Bare", None),
            ]
        );
        assert!(actions("Name=App\nExec=app\n").is_empty());
        // Key file escapes in Exec are undone, as for the entry's own line.
        assert_eq!(
            actions("Name=App\nActions=x;\n[Desktop Action x]\nName=X\nExec=app\\sa\\\\b\n"),
            [action("x", "X", Some("app a\\b"))]
        );
    }

    fn locales(variables: &[(&str, &str)]) -> Vec<Vec<String>> {
        locale_forms(|name| {
            variables
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn locale_forms_follow_the_desktop_entry_specification() {
        assert_eq!(
            key_forms("sr_RS.UTF-8@latin"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(key_forms("de_AT.UTF-8"), ["de_AT", "de"]);
        assert_eq!(key_forms("sr@latin"), ["sr@latin", "sr"]);
        assert_eq!(key_forms("de"), ["de"]);
        assert!(key_forms("").is_empty());
        // The first variable set and not empty decides alone.
        assert_eq!(
            locales(&[("LANGUAGE", ""), ("LC_ALL", "de_AT.UTF-8"), ("LANG", "fr")]),
            [["de_AT", "de"]]
        );
        assert_eq!(locales(&[("LC_MESSAGES", "sr"), ("LANG", "fr")]), [["sr"]]);
        // LANGUAGE is a list; empty items are skipped.
        assert_eq!(
            locales(&[("LANGUAGE", "zz::de_AT:sr"), ("LC_ALL", "fr")]),
            [vec!["zz"], vec!["de_AT", "de"], vec!["sr"]]
        );
        assert!(locales(&[]).is_empty());
    }

    fn names_for(variables: &[(&str, &str)]) -> Vec<String> {
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(
                "[Desktop Entry]\nName=App\nActions=full;country;language;empty;\n\
                 [Desktop Action full]\nName=Default Full\nName[sr]=Language Full\n\
                 Name[sr@latin]=Modifier Full\nName[sr_RS]=Country Full\n\
                 Name[sr_RS@latin]=Full Full\nName[de_AT]=Austria Full\nExec=app\n\
                 [Desktop Action country]\nName=Default Country\nName[sr@latin]=Modifier Country\n\
                 Name[sr_RS]=Country Country\nExec=app\n\
                 [Desktop Action language]\nName=Default Language\nName[sr]=Language Language\n\
                 Exec=app\n\
                 [Desktop Action empty]\nName=Default Empty\nName[sr_RS@latin]=\n\
                 Name[sr]=Serbian Empty\nExec=app\n",
                glib::KeyFileFlags::KEEP_TRANSLATIONS,
            )
            .unwrap();
        desktop_actions_for(&keyfile, &locales(variables))
            .into_iter()
            .map(|action| action.name)
            .collect()
    }

    #[test]
    fn action_names_follow_the_locale_order() {
        // The country outranks the modifier; an empty translation drops
        // the action.
        assert_eq!(
            names_for(&[("LANG", "sr_RS.UTF-8@latin")]),
            ["Full Full", "Country Country", "Language Language"]
        );
        assert_eq!(
            names_for(&[("LANG", "sr_ME.UTF-8@latin")]),
            [
                "Modifier Full",
                "Modifier Country",
                "Language Language",
                "Serbian Empty"
            ]
        );
        // A later list entry only when no form of the earlier one is there.
        assert_eq!(
            names_for(&[("LANGUAGE", "zz:de_AT:sr"), ("LANG", "sr_RS@latin")]),
            [
                "Austria Full",
                "Default Country",
                "Language Language",
                "Serbian Empty"
            ]
        );
        assert_eq!(
            names_for(&[]),
            [
                "Default Full",
                "Default Country",
                "Default Language",
                "Default Empty"
            ]
        );
    }

    fn best<'a>(entries: &'a [DesktopEntry], query: &str) -> Option<&'a str> {
        best_match(entries, query).map(|entry| entry.id.as_str())
    }

    #[test]
    fn search_ranks_like_glib() {
        let entries = [
            entry("calc.desktop", "Name=Calculator\nExec=gnome-calculator\n"),
            entry("office.desktop", "Name=LibreOffice Calc\nExec=localc\n"),
            entry(
                "term.desktop",
                "Name=Terminal\nExec=/usr/bin/xterm -e sh\nKeywords=shell;console;\n",
            ),
            entry("script.desktop", "Name=Tool\nExec=python3 tool.py\n"),
            entry(
                "editor.desktop",
                "Name=Editor\nGenericName=Text Editor\nExec=gedit\n",
            ),
        ];
        // Earlier words rank first: "Calculator" before "LibreOffice Calc".
        assert_eq!(best(&entries, "calc"), Some("calc.desktop"));
        // A prefix match in any key ranks before a substring match in Name.
        assert_eq!(best(&entries, "xterm"), Some("term.desktop"));
        assert_eq!(best(&entries, "she"), Some("term.desktop"));
        // Every word has to match; case does not matter.
        assert_eq!(best(&entries, "LIBREOFFICE calc"), Some("office.desktop"));
        assert_eq!(best(&entries, "libreoffice zzz"), None);
        // Interpreters in Exec are not matched; GenericName is.
        assert_eq!(best(&entries, "python3"), None);
        assert_eq!(best(&entries, "text"), Some("editor.desktop"));
        assert_eq!(best(&entries, ""), None);
        assert_eq!(best(&entries, "..."), None);
    }

    #[test]
    fn exec_matching_uses_the_program_name() {
        assert_eq!(exec_program("/usr/bin/firefox %u"), Some("firefox"));
        assert_eq!(exec_program("env FOO=1 app"), None);
        assert_eq!(exec_program(" app"), Some(""));
    }

    #[test]
    fn entries_load_unless_unreadable_invalid_or_hidden() {
        let scratch = Scratch::new("load");
        let file = |name: &str, text: &str| EntryFile {
            id: name.to_owned(),
            path: scratch.write(name, text),
        };
        let shown = load_entry(file(
            "shown.desktop",
            "[Desktop Entry]\nType=Application\nName=Shown\nExec=/usr/bin/true\n",
        ))
        .unwrap();
        assert_eq!(shown.id, "shown.desktop");
        assert_eq!(shown.app.unwrap().name(), "Shown");
        let missing = load_entry(file(
            "missing.desktop",
            "[Desktop Entry]\nType=Application\nName=Missing\nExec=/nonexistent/program\n",
        ))
        .unwrap();
        assert!(missing.app.is_none(), "GLib rejects a missing program");
        assert!(!missing.search_terms.is_empty(), "but still finds it");
        assert!(load_entry(file(
            "hidden.desktop",
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=/usr/bin/true\nHidden=true\n",
        ))
        .is_none());
        assert!(load_entry(file("invalid.desktop", "not a key file")).is_none());
        std::fs::create_dir(scratch.0.join("dir.desktop")).unwrap();
        assert!(load_entry(EntryFile {
            id: "dir.desktop".to_owned(),
            path: Path::new(&scratch.0).join("dir.desktop"),
        })
        .is_none());
    }

    /// Set in the child process [`entries_and_search_agree_with_glib`]
    /// starts: the fixture directory.
    const GLIB_FIXTURE: &str = "ILIA_ENTRIES_GLIB_FIXTURE";

    const APP: &str = "[Desktop Entry]\nType=Application\nExec=true\n";

    /// GLib reads the data directories once per process, so the comparison
    /// runs in a child process of this test binary whose environment names
    /// the fixture's directories alone.
    #[test]
    fn entries_and_search_agree_with_glib() {
        let scratch = Scratch::new("glib");
        let icon = scratch.write("icons/blue.png", "");
        let home =
            |name: &str, text: &str| scratch.write(&format!("home/applications/{name}"), text);
        home(
            "viewer.desktop",
            &format!("{APP}Name=HangimageX Viewer\nIcon={}\n", icon.display()),
        );
        home("tools.desktop", &format!("{APP}Name=HangimageX Tools\n"));
        home("kde/rev.desktop", &format!("{APP}Name=HangrevX Editor\n"));
        home(
            "org.example.HangbusX.desktop",
            &format!("{APP}Name=HangbusX Service\nDBusActivatable=true\n"),
        );
        home(
            "fields.desktop",
            "[Desktop Entry]\nType=Application\nName=Field Probe\nIcon=fixture-icon\n\
             Exec=true %k %c %i\n",
        );
        home(
            "hidden.desktop",
            &format!("{APP}Name=Hidden Fixture\nHidden=true\n"),
        );
        home(
            "missing.desktop",
            "[Desktop Entry]\nType=Application\nName=Missing Program\nExec=/nonexistent/x\n",
        );
        home(
            "quiet.desktop",
            &format!("{APP}Name=Quiet Fixture\nNoDisplay=true\n"),
        );
        home("broken.desktop", "not a key file");
        let high =
            |name: &str, text: &str| scratch.write(&format!("high/applications/{name}"), text);
        high("viewer.desktop", &format!("{APP}Name=Masked Viewer\n"));
        high("kde-rev.desktop", &format!("{APP}Name=Masked Editor\n"));
        high("calc.desktop", &format!("{APP}Name=Calculator\n"));
        high("office.desktop", &format!("{APP}Name=LibreOffice Calc\n"));
        high(
            "term.desktop",
            "[Desktop Entry]\nType=Application\nName=Terminal\nKeywords=shell;console;\n\
             Exec=/usr/bin/true -e sh\n",
        );
        let low = |name: &str, text: &str| scratch.write(&format!("low/applications/{name}"), text);
        low("calc.desktop", &format!("{APP}Name=Masked Calculator\n"));
        low(
            "script.desktop",
            "[Desktop Entry]\nType=Application\nName=Tool\nExec=python3 tool.py\n",
        );
        low(
            "editor.desktop",
            &format!("{APP}Name=Editor\nGenericName=Text Editor\n"),
        );
        low("a/b/deep.desktop", &format!("{APP}Name=Deep Fixture\n"));
        // One desktop id from two files of one directory tree.
        low("x-y.desktop", &format!("{APP}Name=Twin One\n"));
        low("x/y.desktop", &format!("{APP}Name=Twin Two\n"));

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "entries::tests::isolated_glib_comparison",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(GLIB_FIXTURE, &scratch.0)
            .env("HOME", &scratch.0)
            .env("XDG_DATA_HOME", scratch.0.join("home"))
            .env(
                "XDG_DATA_DIRS",
                format!(
                    "{}:{}",
                    scratch.0.join("high").display(),
                    scratch.0.join("low").display()
                ),
            )
            .env("XDG_CONFIG_HOME", scratch.0.join("config"))
            .env("XDG_CONFIG_DIRS", scratch.0.join("config-dirs"))
            .env("LANG", "C.UTF-8")
            .env_remove("LANGUAGE")
            .env_remove("LC_ALL")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The comparison itself; does nothing unless started by
    /// [`entries_and_search_agree_with_glib`].
    #[test]
    fn isolated_glib_comparison() {
        let Some(root) = std::env::var_os(GLIB_FIXTURE).map(PathBuf::from) else {
            return;
        };
        let entries: Vec<DesktopEntry> = entry_files(&application_dirs())
            .into_iter()
            .filter_map(load_entry)
            .collect();

        // The entries GLib lists, by desktop id and file, hidden and masked
        // ones left out.
        let mut ours = Vec::new();
        for entry in &entries {
            if let Some(app) = &entry.app {
                assert_eq!(app.id().as_deref(), Some(entry.id.as_str()));
                ours.push((entry.id.clone(), app.filename()));
            }
        }
        ours.sort();
        let mut glib: Vec<_> = gio::AppInfo::all()
            .into_iter()
            .filter_map(|app| app.downcast::<gio::DesktopAppInfo>().ok())
            .map(|app| (app.id().unwrap().to_string(), app.filename()))
            .collect();
        glib.sort();
        assert_eq!(ours, glib);
        assert!(ours.iter().any(|(id, _)| id == "kde-rev.desktop"));
        assert!(ours.iter().any(|(id, _)| id == "a-b-deep.desktop"));

        // A window's app name finds the entry GLib's search ranks first,
        // also when it holds several words, in any order.
        let best = |query: &str| best_match(&entries, query).map(|entry| entry.id.clone());
        assert_eq!(best("hangimagex-viewer").as_deref(), Some("viewer.desktop"));
        assert_eq!(best("editor-hangrevx").as_deref(), Some("kde-rev.desktop"));
        assert_eq!(best("org.example.HangbusX"), None);
        for query in [
            "hangimagex-viewer",
            "editor-hangrevx",
            "hangimagex",
            "viewer",
            "org.example.HangbusX",
            "hangbusx",
            "calc",
            "LIBREOFFICE calc",
            "libreoffice zzz",
            "true",
            "text",
            "shell",
            "she",
            "python3",
            "tool",
            "deep",
            "masked",
            "quiet",
            "missing",
            "hidden",
            "twin",
            "x-y",
            "",
            "...",
        ] {
            let first = gio::DesktopAppInfo::search(query)
                .into_iter()
                .next()
                .unwrap_or_default();
            let first: Vec<String> = first.iter().map(ToString::to_string).collect();
            match best(query) {
                Some(id) => assert!(first.contains(&id), "{query:?}: {id} not in {first:?}"),
                None => assert!(first.is_empty(), "{query:?}: GLib finds {first:?}"),
            }
        }

        // What a launch of these entries uses: the desktop file for %k, and
        // the bus name of a D-Bus-activatable entry.
        let app = |id: &str| {
            let entry = entries.iter().find(|entry| entry.id == id).unwrap();
            entry.app.clone().unwrap()
        };
        let fields = root.join("home/applications/fields.desktop");
        assert_eq!(
            command_line(&app("fields.desktop")).unwrap(),
            [
                "true",
                fields.to_str().unwrap(),
                "Field Probe",
                "--icon",
                "fixture-icon"
            ]
        );
        assert_eq!(
            bus_name(&app("org.example.HangbusX.desktop")).as_deref(),
            Some("org.example.HangbusX")
        );
        assert_eq!(bus_name(&app("viewer.desktop")), None);
    }

    /// Runs [`read_all`] on numbers on another thread. Reads of `stuck`
    /// numbers wait until `release` is set; `slow` numbers take 300 ms.
    fn read_numbers(
        count: u32,
        stuck: &'static [u32],
        slow: &'static [u32],
        deadline: Duration,
        release: Arc<AtomicBool>,
    ) -> (Instant, mpsc::Receiver<(Instant, Delivery<u32>)>) {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        std::thread::spawn(move || {
            read_all(
                (0..count).collect(),
                move |item| {
                    if stuck.contains(&item) {
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    } else if slow.contains(&item) {
                        std::thread::sleep(Duration::from_millis(300));
                    }
                    (item % 10 != 9).then_some(item)
                },
                Some,
                start + deadline,
                |delivery| sender.send((Instant::now(), delivery)).unwrap(),
            );
        });
        (start, receiver)
    }

    fn sorted(mut items: Vec<u32>) -> Vec<u32> {
        items.sort_unstable();
        items
    }

    const WAIT: Duration = Duration::from_secs(20);

    #[test]
    fn reads_finishing_before_the_deadline_are_waited_for() {
        let release = Arc::new(AtomicBool::new(true));
        let (_, deliveries) = read_numbers(40, &[], &[3, 5, 7], Duration::from_secs(5), release);
        let Ok((_, Delivery::List(list))) = deliveries.recv_timeout(WAIT) else {
            panic!("no list");
        };
        let expected: Vec<u32> = (0..40).filter(|item| item % 10 != 9).collect();
        assert_eq!(sorted(list), expected);
        assert!(deliveries.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn stuck_reads_do_not_hold_up_the_list_after_the_deadline() {
        let release = Arc::new(AtomicBool::new(false));
        // More stuck reads than readers started at first.
        let stuck: &[u32] = &[0, 1, 2, 3, 4, 5];
        let deadline = Duration::from_millis(600);
        let (start, deliveries) = read_numbers(30, stuck, &[], deadline, release.clone());
        let Ok((at, Delivery::List(list))) = deliveries.recv_timeout(WAIT) else {
            panic!("no list");
        };
        assert!(at >= start + deadline, "listed before the deadline");
        let expected: Vec<u32> = (6..30).filter(|item| item % 10 != 9).collect();
        assert_eq!(sorted(list), expected);
        release.store(true, Ordering::SeqCst);
        let mut late = Vec::new();
        for _ in stuck {
            match deliveries.recv_timeout(WAIT) {
                Ok((_, Delivery::Late(item))) => late.push(item),
                _ => panic!("late item missing: {late:?}"),
            }
        }
        assert_eq!(sorted(late), stuck.to_vec());
    }

    #[test]
    fn an_empty_catalogue_is_listed_at_once() {
        let release = Arc::new(AtomicBool::new(true));
        let (_, deliveries) = read_numbers(0, &[], &[], Duration::from_secs(60), release);
        let Ok((_, Delivery::List(list))) = deliveries.recv_timeout(Duration::from_secs(2)) else {
            panic!("no list");
        };
        assert!(list.is_empty());
    }
}
