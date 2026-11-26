use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gio::prelude::*;

use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::apps::{record_launch, LAUNCH_HISTORY};
use crate::args::Args;
use crate::filter::{contains_all_words, fold_text, is_subsequence, query_words};
use crate::icons::load_icon_from_name;
use crate::keybindings::navigate;
use crate::page_files::{Exec, PageFile, Rows};
use crate::pages::{
    is_enter_key, select_first_row, select_row_near, selected_position, DialogPage, FUZZY_MATCHING,
};
use crate::runner::scope_command;

const DEFAULT_ICON: &str = "utilities-terminal";
const FALLBACK_ICON: &str = "applications-other";
const HELP: &str = "This dialog presents a filterable list of items to select from. If no item matches, enter returns the typed text.";
const PAGE_FILE_HELP: &str = "This dialog presents the items of a page file. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will choose it.";

/// Inputs up to this many lines are filtered on every key; larger ones once
/// typing pauses for `FILTER_DELAY`.
const IMMEDIATE_FILTER_LINES: usize = 10_000;
const FILTER_DELAY: Duration = Duration::from_millis(100);
/// The most input read at once by the background reader.
const READ_SIZE: usize = 1 << 16;
/// How many read lines join the page at a time.
const INGEST_SLICE_LINES: usize = 256;
/// How long lines keep joining the page before pending events, key
/// presses included, are handled.
const INGEST_SLICE_TIME: Duration = Duration::from_millis(20);
/// How long the launcher, its window gone, waits for a command a choice
/// started to finish before it exits anyway.
const LAUNCH_WAIT: Duration = Duration::from_secs(3);

/// One input line, as shown, as matched and as chosen.
struct Line {
    text: String,
    // The folded text, when it differs.
    folded: Option<String>,
    // The bytes a choice acts on, when they are not the text: bytes read
    // that are not UTF-8, which the text shows with replacement characters
    // but a choice gives back as read, or a value apart from the label.
    raw: Option<Box<[u8]>>,
    // How often the line's value was chosen, on a page ordered that way.
    count: usize,
}

impl Line {
    /// A line of standard input: shown and chosen whole.
    fn new(bytes: Vec<u8>) -> Self {
        let (text, raw) = match String::from_utf8(bytes) {
            Ok(text) => (text, None),
            Err(error) => {
                let bytes = error.into_bytes();
                let text = String::from_utf8_lossy(&bytes).into_owned();
                (text, Some(bytes.into_boxed_slice()))
            }
        };
        Self::with_raw(text, raw)
    }

    /// An entry of a page file: a label and a value divided at the first
    /// tab, or the whole entry as both. An empty label shows the value, an
    /// empty value is the label; with both empty there is no row.
    fn pair(bytes: &[u8]) -> Option<Self> {
        let (label, value) = match bytes.iter().position(|&byte| byte == b'\t') {
            Some(tab) => (&bytes[..tab], &bytes[tab + 1..]),
            None => (bytes, bytes),
        };
        let (label, value) = match (label.is_empty(), value.is_empty()) {
            (true, true) => return None,
            (true, false) => (value, value),
            (false, true) => (label, label),
            (false, false) => (label, value),
        };
        let text = String::from_utf8_lossy(label).into_owned();
        let raw = (value != text.as_bytes()).then(|| value.into());
        Some(Self::with_raw(text, raw))
    }

    fn with_raw(text: String, raw: Option<Box<[u8]>>) -> Self {
        let folded = fold_text(&text);
        let folded = (folded != text).then_some(folded);
        Self {
            text,
            folded,
            raw,
            count: 0,
        }
    }

    fn folded(&self) -> &str {
        self.folded.as_deref().unwrap_or(&self.text)
    }

    fn bytes(&self) -> &[u8] {
        self.raw.as_deref().unwrap_or(self.text.as_bytes())
    }
}

/// Whether a line of command output holds no row: empty, or white space
/// alone.
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Whether the already folded `text` matches the query `words`: every word
/// occurs in it or, with subsequence matching on, every word is a
/// subsequence of it. A word that occurs in a text is also a subsequence of
/// it.
fn line_matches(text: &str, words: &[String], subsequence: bool) -> bool {
    if subsequence {
        words.iter().all(|word| is_subsequence(word, text))
    } else {
        contains_all_words(text, words)
    }
}

/// The lines on show: those matching the query words, in list order.
#[derive(Debug, Default, PartialEq)]
struct Shown {
    words: Vec<String>,
    subsequence: bool,
    lines: Vec<u32>,
}

impl Shown {
    fn all(lines: &[Line], subsequence: bool) -> Self {
        Self {
            words: Vec::new(),
            subsequence,
            lines: (0..lines.len() as u32).collect(),
        }
    }

    /// Whether `line` belongs among the lines on show.
    fn accepts(&self, line: &Line) -> bool {
        line_matches(line.folded(), &self.words, self.subsequence)
    }

    /// The lines matching `words`. When every word shown so far lies within
    /// one of the new words (as a subsequence, with subsequence matching
    /// on), the new matches are among the lines on show, and only those are
    /// checked. Each check is linear in the length of the line.
    fn filtered(&self, lines: &[Line], words: Vec<String>, subsequence: bool) -> Self {
        let lies_within = |old: &str, new: &str| {
            if subsequence {
                is_subsequence(old, new)
            } else {
                new.contains(old)
            }
        };
        let narrows = self.words.is_empty()
            || (self.subsequence == subsequence
                && self
                    .words
                    .iter()
                    .all(|old| words.iter().any(|new| lies_within(old, new))));
        let matches =
            |index: &u32| line_matches(lines[*index as usize].folded(), &words, subsequence);
        let matching = if narrows {
            self.lines.iter().copied().filter(matches).collect()
        } else {
            (0..lines.len() as u32).filter(matches).collect()
        };
        Self {
            words,
            subsequence,
            lines: matching,
        }
    }

    /// Makes room for a line inserted into the list at `position`: the
    /// lines from there on move one place down. Returns where among the
    /// lines on show a line at `position` belongs.
    fn insert_at(&mut self, position: u32) -> usize {
        let start = self.lines.partition_point(|&line| line < position);
        for line in &mut self.lines[start..] {
            *line += 1;
        }
        start
    }
}

/// Splits input into items as it arrives: an item ends at a newline, and
/// the end of the input ends a last item without one. Bytes stay bytes
/// until an item is complete, so a character split between reads is joined
/// again.
#[derive(Default)]
struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    /// The items `bytes` completes.
    fn push(&mut self, mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut items = Vec::new();
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            self.partial.extend_from_slice(&bytes[..end]);
            items.push(item(std::mem::take(&mut self.partial)));
            bytes = &bytes[end + 1..];
        }
        self.partial.extend_from_slice(bytes);
        items
    }

    /// The last item, when the input ends without a newline after it.
    fn finish(self) -> Option<Vec<u8>> {
        (!self.partial.is_empty()).then(|| item(self.partial))
    }
}

/// Like the C-string lines of the reference, an item ends at its first NUL
/// byte; `\r` is kept verbatim.
fn item(mut line: Vec<u8>) -> Vec<u8> {
    if let Some(end) = line.iter().position(|&byte| byte == 0) {
        line.truncate(end);
    }
    line
}

/// Where the lines of a list come from.
enum Source {
    /// Standard input: the text-list page.
    Stdin,
    /// The fixed entries of a page file.
    Items(Vec<String>),
    /// The output of a page file's shell command, run once the page is
    /// first shown.
    Command(String),
}

/// What choosing a line does.
enum Action {
    /// Print its value.
    Print,
    /// Run the page file's Exec words with the value.
    Exec(Vec<String>),
    /// Nothing: the Exec value could not be split into words.
    Nothing,
}

/// A page's settings: the text-list page's, or a page file's.
struct Spec {
    id: String,
    // The tab title, as markup.
    name: String,
    icon: String,
    help: &'static str,
    key: Option<char>,
    // The icon rows show; `None` shows none.
    row_icon: Option<Option<String>>,
    source: Source,
    action: Action,
    // Whether Enter with no line matching prints the typed text.
    prints_unmatched: bool,
    // The prefix of the page's launch history entries, when its rows are
    // ordered by how often they were chosen.
    history_prefix: Option<String>,
}

/// A filterable list of lines: those standard input holds (the text-list
/// page), or those a page file lists or its command prints.
pub struct TextListPage {
    spec: Spec,
    settings: gio::Settings,
    entry: gtk::Entry,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    quit: Rc<dyn Fn()>,
    // Whether lines also match by subsequence: the `fuzzy-matching`
    // setting, followed while the launcher runs.
    subsequence: Cell<bool>,
    // The lines read so far, in list order.
    lines: RefCell<Vec<Line>>,
    // How often each value was chosen, on a page ordered that way.
    counts: HashMap<String, usize>,
    // Every line, in list order: the rows for an empty query.
    all_rows: gtk::ListStore,
    // The rows the view shows: `all_rows`, or the matching lines alone.
    rows: RefCell<gtk::ListStore>,
    shown: RefCell<Shown>,
    // A refilter waiting for typing to pause.
    pending: RefCell<Option<glib::SourceId>>,
    // Whether the lines have started to arrive.
    started: Cell<bool>,
    this: Weak<TextListPage>,
}

impl TextListPage {
    /// The text-list page: the lines of standard input.
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        args: &Args,
        quit: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        // `-n` drops the icon column.
        let row_icon = (!args.has("-n")).then(|| {
            // A bare `-i` clears the icon, which falls back to the generic one.
            if args.has("-i") {
                args.value("-i").map(str::to_owned)
            } else {
                Some(DEFAULT_ICON.to_owned())
            }
        });
        let spec = Spec {
            id: "textlist".to_owned(),
            name: "TextList".to_owned(),
            icon: DEFAULT_ICON.to_owned(),
            help: HELP,
            key: Some('t'),
            row_icon,
            source: Source::Stdin,
            action: Action::Print,
            prints_unmatched: true,
            history_prefix: None,
        };
        Self::build(settings, entry, spec, quit)
    }

    /// The page a page file of fixed entries or of a command's output
    /// defines.
    pub fn from_page_file(
        settings: gio::Settings,
        entry: gtk::Entry,
        file: &PageFile,
        key: Option<char>,
        quit: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let source = match &file.rows {
            Rows::Items(items) => Source::Items(items.clone()),
            Rows::Command(command) => Source::Command(command.clone()),
            Rows::Apps(_) | Rows::Menu(_) => Source::Items(Vec::new()),
        };
        let action = match &file.exec {
            Exec::Print => Action::Print,
            Exec::Words(words) => Action::Exec(words.clone()),
            Exec::Invalid => Action::Nothing,
        };
        let spec = Spec {
            id: file.id.clone(),
            // The title is plain text.
            name: glib::markup_escape_text(&file.name).into(),
            icon: file.icon.clone(),
            help: PAGE_FILE_HELP,
            key,
            row_icon: Some(Some(file.icon.clone())),
            source,
            action,
            prints_unmatched: false,
            history_prefix: file
                .frequency
                .then(|| format!("{}#", file.id.to_ascii_lowercase())),
        };
        Self::build(settings, entry, spec, quit)
    }

    fn build(
        settings: gio::Settings,
        entry: gtk::Entry,
        spec: Spec,
        quit: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let view = gtk::TreeView::new();
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        if let Some(icon) = &spec.row_icon {
            // Every row shows the same icon, so it is set on the renderer
            // rather than stored in each row.
            let icon_renderer = gtk::CellRendererPixbuf::new();
            icon_renderer
                .set_pixbuf(item_icon(icon.as_deref(), settings.int("icon-size")).as_ref());
            let icon_column = gtk::TreeViewColumn::with_attributes("Icon", &icon_renderer, &[]);
            icon_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&icon_column);
        }
        let text_renderer = gtk::CellRendererText::new();
        let text_column =
            gtk::TreeViewColumn::with_attributes("Name", &text_renderer, &[("text", 0)]);
        text_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&text_column);
        view.set_activate_on_single_click(true);

        // Lines join the list as they arrive, while the input may still be
        // open.
        let all_rows = rows_store(std::iter::empty());
        view.set_model(Some(&all_rows));
        let subsequence = settings.boolean(FUZZY_MATCHING);
        let counts = spec
            .history_prefix
            .as_deref()
            .map(|prefix| launch_counts(&settings, prefix))
            .unwrap_or_default();

        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Rc::new_cyclic(|this| Self {
            spec,
            settings: settings.clone(),
            entry,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            quit,
            subsequence: Cell::new(subsequence),
            shown: RefCell::new(Shown::all(&[], subsequence)),
            lines: RefCell::new(Vec::new()),
            counts,
            rows: RefCell::new(all_rows.clone()),
            all_rows,
            pending: RefCell::new(None),
            started: Cell::new(false),
            this: this.clone(),
        });
        page.connect_activation();
        page.follow_fuzzy_setting(&settings);
        page.set_selection();
        // A command runs only once its page is shown.
        if !matches!(page.spec.source, Source::Command(_)) {
            page.start();
        }
        page
    }

    /// Starts the lines arriving, the first time it is called.
    fn start(&self) {
        if self.started.replace(true) {
            return;
        }
        match &self.spec.source {
            Source::Stdin => read_lines_in_background(self.this.clone(), io::stdin(), false),
            Source::Items(items) => self.add_lines(
                items
                    .iter()
                    .filter_map(|item| Line::pair(item.as_bytes()))
                    .collect(),
            ),
            Source::Command(command) => match run_command(command) {
                Ok(output) => read_lines_in_background(self.this.clone(), output, true),
                Err(error) => eprintln!(
                    "ilia: cannot run the command of page '{}': {error}",
                    self.spec.id
                ),
            },
        }
    }

    /// Applies a change another program makes to `fuzzy-matching` while the
    /// launcher runs: the lines are matched again for the same filter text,
    /// keeping the highlighted line (see [`TextListPage::rematch`]).
    fn follow_fuzzy_setting(&self, settings: &gio::Settings) {
        let this = self.this.clone();
        settings.connect_changed(Some(FUZZY_MATCHING), move |settings, _| {
            let Some(page) = this.upgrade() else {
                return;
            };
            let subsequence = settings.boolean(FUZZY_MATCHING);
            if page.subsequence.replace(subsequence) != subsequence {
                page.rematch();
            }
        });
    }

    /// Adds lines that have just been read, in input order: every line to
    /// the unfiltered rows, and those matching the filter in force to the
    /// rows on show. On a page ordered by how often lines were chosen, a
    /// line goes after every line chosen as often or more; otherwise, and
    /// for lines never chosen, after every line, so the highlighted row
    /// stays where it is. Nothing highlighted yet, the first row is.
    fn add_lines(&self, new_lines: Vec<Line>) {
        {
            let mut lines = self.lines.borrow_mut();
            let mut shown = self.shown.borrow_mut();
            let rows = self.rows.borrow();
            let filtered = *rows != self.all_rows;
            for mut line in new_lines {
                if let Some(prefix) = &self.spec.history_prefix {
                    let value = String::from_utf8_lossy(line.bytes());
                    line.count = self
                        .counts
                        .get(&format!("{prefix}{value}"))
                        .copied()
                        .unwrap_or_default();
                }
                let position = lines.partition_point(|other| other.count >= line.count);
                if position == lines.len() {
                    self.all_rows.insert_with_values(None, &[(0, &line.text)]);
                    if shown.accepts(&line) {
                        shown.lines.push(position as u32);
                        if filtered {
                            rows.insert_with_values(None, &[(0, &line.text)]);
                        }
                    }
                } else {
                    self.all_rows
                        .insert_with_values(Some(position as u32), &[(0, &line.text)]);
                    let shown_position = shown.insert_at(position as u32);
                    if shown.accepts(&line) {
                        shown.lines.insert(shown_position, position as u32);
                        if filtered {
                            rows.insert_with_values(
                                Some(shown_position as u32),
                                &[(0, &line.text)],
                            );
                        }
                    }
                }
                lines.insert(position, line);
            }
        }
        if self.view.selection().selected().is_none() {
            self.set_selection();
        }
    }

    fn connect_activation(&self) {
        let this = self.this.clone();
        self.view.connect_row_activated(move |_, path, _| {
            if let Some(page) = this.upgrade() {
                page.choose_row(path);
            }
        });
    }

    /// Highlights the first row: the best match for the text typed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        *self.path.borrow_mut() = select_first_row(&self.view);
    }

    /// The index of the line shown at `path`.
    fn line_index(&self, path: &gtk::TreePath) -> Option<u32> {
        let row = usize::try_from(*path.indices().first()?).ok()?;
        self.shown.borrow().lines.get(row).copied()
    }

    /// Acts on the line shown at `path`: prints its value, or runs the
    /// page's command with it.
    fn choose_row(&self, path: &gtk::TreePath) {
        let Some(index) = self.line_index(path) else {
            return;
        };
        let value = self.lines.borrow()[index as usize].bytes().to_vec();
        match &self.spec.action {
            Action::Nothing => {}
            Action::Print => {
                self.record_choice(&value);
                print_line(&value, &self.quit);
            }
            Action::Exec(words) => {
                self.record_choice(&value);
                if let Err(error) = run_exec(&self.spec.id, words, &value) {
                    eprintln!(
                        "ilia: cannot run the command of page '{}': {error}",
                        self.spec.id
                    );
                }
                (self.quit)();
            }
        }
    }

    /// Counts a choice of `value` in the launch history, on a page ordered
    /// by how often its lines were chosen.
    fn record_choice(&self, value: &[u8]) {
        let Some(prefix) = &self.spec.history_prefix else {
            return;
        };
        let mut history: Vec<String> = self
            .settings
            .strv(LAUNCH_HISTORY)
            .iter()
            .map(ToString::to_string)
            .collect();
        let entry = format!("{prefix}{}", String::from_utf8_lossy(value));
        record_launch(&self.settings, &mut history, entry);
    }

    fn selected_path(&self) -> Option<gtk::TreePath> {
        let (model, iter) = self.view.selection().selected()?;
        model.path(&iter)
    }

    /// Shows the lines matching the current text, with the first match
    /// highlighted.
    fn refilter(&self) {
        self.update_rows();
        self.set_selection();
    }

    /// Shows the lines matching the same text again, after a change other
    /// than a new filter text. A line is the same row however many lines
    /// share its text: the highlighted line stays highlighted while it is
    /// still listed. Otherwise the line now at its position is, or the last
    /// one when the list has become shorter, or none when it is empty.
    fn rematch(&self) {
        if self.pending.borrow().is_some() {
            // The text changed too: its first match is highlighted.
            self.refilter();
            return;
        }
        let position = selected_position(&self.view);
        let line = position.and_then(|position| {
            let row = usize::try_from(position).ok()?;
            self.shown.borrow().lines.get(row).copied()
        });
        self.update_rows();
        // The lines on show are in list order.
        let kept = line.and_then(|line| self.shown.borrow().lines.binary_search(&line).ok());
        let position = kept.map_or(position.unwrap_or(0), |row| row as i32);
        *self.path.borrow_mut() =
            select_row_near(&self.view, position).unwrap_or_else(gtk::TreePath::new_first);
    }

    /// Fills the view with the lines matching the current text. The rows are
    /// built before the view gets them, so a large result costs one model
    /// change instead of a signal per row; when the matching lines stay the
    /// same, the rows on show are kept.
    fn update_rows(&self) {
        if let Some(timer) = self.pending.take() {
            timer.remove();
        }
        let words = query_words(self.entry.text().as_str());
        let subsequence = self.subsequence.get();
        let stale = {
            let shown = self.shown.borrow();
            words != shown.words || subsequence != shown.subsequence
        };
        if stale {
            let lines = self.lines.borrow();
            let shown = self.shown.borrow().filtered(&lines, words, subsequence);
            let current = self.rows.borrow().clone();
            let rows = if shown.words.is_empty() {
                self.all_rows.clone()
            } else if current != self.all_rows && shown.lines == self.shown.borrow().lines {
                current.clone()
            } else {
                rows_store(shown.lines.iter().map(|index| &lines[*index as usize]))
            };
            *self.shown.borrow_mut() = shown;
            if rows != current {
                *self.rows.borrow_mut() = rows.clone();
                self.view.set_model(Some(&rows));
            }
        }
    }

    /// Applies a refilter still waiting for typing to pause, before a key
    /// acts on the rows.
    fn flush(&self) {
        if self.pending.borrow().is_some() {
            self.refilter();
        }
    }
}

impl DialogPage for TextListPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        if self.lines.borrow().len() <= IMMEDIATE_FILTER_LINES {
            self.refilter();
            return;
        }
        // Large inputs are filtered once typing pauses, so a burst of keys
        // costs one pass over the lines.
        if let Some(timer) = self.pending.take() {
            timer.remove();
        }
        let this = self.this.clone();
        let timer = glib::timeout_add_local_once(FILTER_DELAY, move || {
            if let Some(page) = this.upgrade() {
                page.pending.take();
                page.refilter();
            }
        });
        self.pending.replace(Some(timer));
    }
    fn on_entry_activated(&self) {
        self.flush();
        if let Some(path) = self.selected_path() {
            self.choose_row(&path);
        }
    }
    fn get_id(&self) -> &str {
        &self.spec.id
    }
    fn get_name(&self) -> &str {
        &self.spec.name
    }
    fn get_icon_name(&self) -> &str {
        &self.spec.icon
    }
    fn get_help(&self) -> &str {
        self.spec.help
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Select Item")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        let enter = is_enter_key(&event.keyval());
        if enter || event.state().contains(gdk::ModifierType::CONTROL_MASK) {
            self.flush();
        }
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        if enter
            && self.spec.prints_unmatched
            && self.shown.borrow().lines.is_empty()
            && !self.entry.text().is_empty()
        {
            print_line(self.entry.text().as_bytes(), &self.quit);
            return true;
        }
        false
    }
    fn get_keybinding(&self) -> Option<char> {
        self.spec.key
    }
    fn show(&self) {
        self.start();
        self.flush();
        self.view.grab_focus();
    }
}

/// How often each launch history entry starting with `prefix` was recorded,
/// by entry.
fn launch_counts(settings: &gio::Settings, prefix: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for entry in settings.strv(LAUNCH_HISTORY).iter() {
        if entry.as_str().starts_with(prefix) {
            *counts.entry(entry.to_string()).or_default() += 1;
        }
    }
    counts
}

thread_local! {
    /// The page commands started, never waited for until the launcher
    /// quits, so their process groups stay theirs.
    static COMMANDS: RefCell<Vec<Child>> = const { RefCell::new(Vec::new()) };
    /// The commands choices started.
    static LAUNCHED: RefCell<Vec<Child>> = const { RefCell::new(Vec::new()) };
}

/// Starts a page file's command through `/bin/sh -c`, in a process group of
/// its own, with standard input from `/dev/null`, and returns its output.
fn run_command(command: &str) -> io::Result<std::process::ChildStdout> {
    // Processes the command leaves behind become the launcher's children,
    // so that it can wait for them once they are ended.
    // SAFETY: PR_SET_CHILD_SUBREAPER takes integer arguments only and
    // touches no memory of this process.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no output pipe"))?;
    COMMANDS.with(|commands| commands.borrow_mut().push(child));
    Ok(output)
}

/// Ends every page command, with every process in its group, and waits
/// for them.
pub fn stop_commands() {
    COMMANDS.with(|commands| {
        for child in commands.borrow_mut().drain(..) {
            // The group is the shell's: it has not been waited for, so its
            // id cannot have been reused.
            let Ok(group) = libc::pid_t::try_from(child.id()) else {
                continue;
            };
            // SAFETY: killpg takes plain integers and touches no memory.
            unsafe { libc::killpg(group, libc::SIGKILL) };
            // The shell, and the processes it started, which the launcher
            // adopts as they lose their parent.
            loop {
                let mut status = 0;
                // SAFETY: `status` is a live, writable c_int for the call.
                if unsafe { libc::waitpid(-group, &mut status, 0) } < 0
                    && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    });
}

/// Keeps a command a choice on another page started, so that the launcher
/// waits for it like for its own (see [`wait_for_launched`]).
pub fn remember_launched(child: Child) {
    LAUNCHED.with(|launched| launched.borrow_mut().push(child));
}

/// Waits, for at most `LAUNCH_WAIT`, until the commands choices started
/// have finished, so that a quick one is done when the launcher exits.
pub fn wait_for_launched() {
    let deadline = std::time::Instant::now() + LAUNCH_WAIT;
    LAUNCHED.with(|launched| {
        for child in launched.borrow_mut().iter_mut() {
            while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
}

/// The command a choice of `value` runs: the Exec words, in each of which
/// every `%s` is the value and `%%` a `%`, other `%` signs staying as they
/// are. When no word holds `%s`, the value is a last word of its own.
fn exec_words(words: &[String], value: &[u8]) -> Vec<Vec<u8>> {
    let mut substituted = false;
    let mut command: Vec<Vec<u8>> = words
        .iter()
        .map(|word| {
            let mut out = Vec::new();
            let mut rest = word.as_bytes();
            while let Some(percent) = rest.iter().position(|&byte| byte == b'%') {
                out.extend_from_slice(&rest[..percent]);
                match rest.get(percent + 1).copied() {
                    Some(b's') => {
                        out.extend_from_slice(value);
                        substituted = true;
                        rest = &rest[percent + 2..];
                    }
                    Some(b'%') => {
                        out.push(b'%');
                        rest = &rest[percent + 2..];
                    }
                    _ => {
                        out.push(b'%');
                        rest = &rest[percent + 1..];
                    }
                }
            }
            out.extend_from_slice(rest);
            out
        })
        .collect();
    if !substituted {
        command.push(value.to_vec());
    }
    command
}

/// Starts the command a choice of `value` runs on the page `id`, in a fresh
/// scope when the scope runner is available, never through a shell.
fn run_exec(id: &str, words: &[String], value: &[u8]) -> io::Result<()> {
    let command = exec_words(words, value).into_iter().map(OsString::from_vec);
    let command: Vec<OsString> = match scope_command(id) {
        Some(scope) => scope.into_iter().chain(command).collect(),
        None => command.collect(),
    };
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()?;
    LAUNCHED.with(|launched| launched.borrow_mut().push(child));
    Ok(())
}

/// Reads `input` on a thread of its own, so the window stays responsive
/// while it is open, and hands each read's complete lines, already folded,
/// to the page. Page file output is divided into labels and values, and its
/// blank lines skipped.
#[allow(deprecated)]
fn read_lines_in_background(
    page: Weak<TextListPage>,
    mut input: impl Read + Send + 'static,
    pairs: bool,
) {
    let (sender, receiver) = glib::MainContext::channel::<Vec<Line>>(glib::Priority::DEFAULT_IDLE);
    let to_line = move |bytes: Vec<u8>| {
        if !pairs {
            Some(Line::new(bytes))
        } else if is_blank(&bytes) {
            None
        } else {
            Line::pair(&bytes)
        }
    };
    let reader = std::thread::Builder::new()
        .name("ilia-input".into())
        .spawn(move || {
            let mut splitter = LineSplitter::default();
            let mut buffer = vec![0; READ_SIZE];
            loop {
                let read = match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let lines: Vec<Line> = splitter
                    .push(&buffer[..read])
                    .into_iter()
                    .filter_map(to_line)
                    .collect();
                if !lines.is_empty() && sender.send(lines).is_err() {
                    return;
                }
            }
            if let Some(last) = splitter.finish().and_then(to_line) {
                let _ = sender.send(vec![last]);
            }
        });
    if let Err(error) = reader {
        eprintln!("Failed to read standard input: {error}");
        return;
    }
    // The channel only queues what was read; rows join the page in short
    // slices from an idle source, so key presses (Escape included) are
    // handled between slices however much output is waiting.
    let pending: Rc<RefCell<VecDeque<Line>>> = Rc::default();
    let draining = Rc::new(Cell::new(false));
    receiver.attach(None, move |lines| {
        if page.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        pending.borrow_mut().extend(lines);
        if !draining.replace(true) {
            let page = page.clone();
            let pending = pending.clone();
            let draining = draining.clone();
            glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
                let Some(page) = page.upgrade() else {
                    draining.set(false);
                    return glib::ControlFlow::Break;
                };
                let started = Instant::now();
                loop {
                    let batch: Vec<Line> = {
                        let mut pending = pending.borrow_mut();
                        let take = pending.len().min(INGEST_SLICE_LINES);
                        pending.drain(..take).collect()
                    };
                    if batch.is_empty() {
                        draining.set(false);
                        return glib::ControlFlow::Break;
                    }
                    page.add_lines(batch);
                    if started.elapsed() >= INGEST_SLICE_TIME {
                        return glib::ControlFlow::Continue;
                    }
                }
            });
        }
        glib::ControlFlow::Continue
    });
}

/// Reads newline-terminated items until EOF, as bytes.
#[cfg(test)]
fn read_items(mut reader: impl Read) -> Vec<Vec<u8>> {
    let mut splitter = LineSplitter::default();
    let mut items = Vec::new();
    let mut buffer = [0; 3];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => items.extend(splitter.push(&buffer[..read])),
        }
    }
    items.extend(splitter.finish());
    items
}

/// A list of `lines` for the view.
fn rows_store<'a>(lines: impl Iterator<Item = &'a Line>) -> gtk::ListStore {
    let store = gtk::ListStore::new(&[String::static_type()]);
    for line in lines {
        store.insert_with_values(None, &[(0, &line.text)]);
    }
    store
}

fn item_icon(icon: Option<&str>, icon_size: i32) -> Option<Pixbuf> {
    let theme = gtk::IconTheme::default()?;
    icon.filter(|icon| !icon.is_empty())
        .and_then(|icon| load_icon_from_name(&theme, icon, icon_size))
        .or_else(|| load_icon_from_name(&theme, FALLBACK_ICON, icon_size))
}

fn print_line(line: &[u8], quit: &Rc<dyn Fn()>) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(line);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
    drop(stdout);
    quit();
}

#[cfg(test)]
mod tests {
    use super::{exec_words, is_blank, read_items, Line, LineSplitter, Shown};
    use crate::filter::query_words;

    const FIXTURE: &str = "Alpha\nNeedle Item\nOmega\n";

    fn lines(items: &[&str]) -> Vec<Line> {
        items
            .iter()
            .map(|item| Line::new(item.as_bytes().to_vec()))
            .collect()
    }

    // The texts of the lines a query shows, starting from every line.
    fn shown<'a>(lines: &'a [Line], query: &str) -> Vec<&'a str> {
        texts(
            lines,
            &Shown::all(lines, false).filtered(lines, query_words(query), false),
        )
    }

    // The same with subsequence matching on.
    fn shown_by_subsequence<'a>(lines: &'a [Line], query: &str) -> Vec<&'a str> {
        texts(
            lines,
            &Shown::all(lines, true).filtered(lines, query_words(query), true),
        )
    }

    fn texts<'a>(lines: &'a [Line], shown: &Shown) -> Vec<&'a str> {
        shown
            .lines
            .iter()
            .map(|index| lines[*index as usize].text.as_str())
            .collect()
    }

    #[test]
    fn items_are_read_line_by_line_to_eof() {
        assert_eq!(
            read_items(FIXTURE.as_bytes()),
            [&b"Alpha"[..], b"Needle Item", b"Omega"]
        );
        assert_eq!(read_items(&b"a\n\nb"[..]), [&b"a"[..], b"", b"b"]);
        assert_eq!(read_items(&b"dos\r\n"[..]), [b"dos\r"]);
        assert_eq!(read_items(&b"nul\0tail\n"[..]), [b"nul"]);
        assert_eq!(read_items(&b"caf\xe9\n"[..]), [b"caf\xe9"]);
        assert!(read_items(&b""[..]).is_empty());
        assert_eq!(read_items(&b"last"[..]), [b"last"]);
        assert_eq!(read_items(&b"x\n\0tail"[..]), [&b"x"[..], b""]);
    }

    #[test]
    fn items_complete_only_at_a_newline_or_the_end() {
        let mut splitter = LineSplitter::default();
        // "é" is C3 A9: the first read ends inside it.
        assert_eq!(splitter.push(b"one\ncaf\xc3"), [b"one"]);
        assert!(splitter.push(b"\xa9 au").is_empty());
        assert_eq!(splitter.push(b" lait\ntw"), ["café au lait".as_bytes()]);
        assert_eq!(splitter.push(b"o\n\nthree"), [&b"two"[..], b""]);
        assert_eq!(splitter.finish(), Some(b"three".to_vec()));
        assert_eq!(LineSplitter::default().finish(), None);
    }

    #[test]
    fn the_filter_in_force_accepts_new_lines() {
        let items = lines(&["alpha one", "beta"]);
        let shown = Shown::all(&items, false).filtered(&items, query_words("one"), false);
        assert!(shown.accepts(&Line::new(b"ONE more".to_vec())));
        assert!(!shown.accepts(&Line::new(b"o n e".to_vec())));
        let shown = Shown::all(&items, true).filtered(&items, query_words("one"), true);
        assert!(shown.accepts(&Line::new(b"o n e".to_vec())));
        assert!(Shown::all(&items, false).accepts(&Line::new(Vec::new())));
    }

    #[test]
    fn switching_subsequence_matching_checks_every_line_again() {
        let items = lines(&["firefox", "ffx", "other"]);
        let plain = Shown::all(&items, false).filtered(&items, query_words("ffx"), false);
        assert_eq!(plain.lines, [1]);
        let fuzzy = plain.filtered(&items, query_words("ffx"), true);
        assert_eq!(fuzzy.lines, [0, 1]);
        assert_eq!(fuzzy.filtered(&items, query_words("ffx"), false).lines, [1]);
    }

    #[test]
    fn lines_that_are_not_utf8_print_back_as_read() {
        let line = Line::new(b"CAF\xe9-latin1.txt".to_vec());
        assert_eq!(line.text, "CAF\u{fffd}-latin1.txt");
        assert_eq!(line.folded(), "caf\u{fffd}-latin1.txt");
        assert_eq!(line.bytes(), b"CAF\xe9-latin1.txt");
        let line = Line::new("Café".as_bytes().to_vec());
        assert_eq!(line.bytes(), "Café".as_bytes());
        assert!(line.raw.is_none());
    }

    #[test]
    fn filter_matches_every_word_ignoring_case() {
        let items = lines(&["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, ""), ["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, "   "), ["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, "  NEEDLE "), ["Needle Item"]);
        assert_eq!(shown(&items, "le it"), ["Needle Item"]);
        assert!(shown(&items, "needle item x").is_empty());
        // Whitespace separates words; it is not matched itself.
        assert!(shown(&items, "needleitem").is_empty());
        assert_eq!(shown(&items, "item NEEDLE"), ["Needle Item"]);
        let items = lines(&["Éléphant Café à Paris", "Café elsewhere"]);
        assert_eq!(shown(&items, "CAFÉ ÉLÉ"), ["Éléphant Café à Paris"]);
        assert_eq!(shown(&items, "cafe ele"), ["Éléphant Café à Paris"]);
        let items = lines(&["Straße", "ﬁle", "plain"]);
        assert_eq!(shown(&items, "STRASSE"), ["Straße"]);
        assert_eq!(shown(&items, "file"), ["ﬁle"]);
    }

    #[test]
    fn subsequence_matches_keep_input_order() {
        let items = lines(&["alpha firefox", "ffx exactly", "Fix fax", "fox"]);
        assert_eq!(
            shown_by_subsequence(&items, "ffx"),
            ["alpha firefox", "ffx exactly", "Fix fax"]
        );
        assert_eq!(shown(&items, "ffx"), ["ffx exactly"]);
        // Every word must match, each on its own.
        assert_eq!(shown_by_subsequence(&items, "ffx fox"), ["alpha firefox"]);
        assert!(shown_by_subsequence(&items, "ffx zz").is_empty());
        assert_eq!(shown_by_subsequence(&lines(&["Café"]), "CFE"), ["Café"]);
    }

    #[test]
    fn typing_on_narrows_subsequence_matches() {
        let items = lines(&["qzx first", "qx plain", "quick only", "unrelated"]);
        let q = Shown::all(&items, true).filtered(&items, query_words("q"), true);
        assert_eq!(q.lines, [0, 1, 2]);
        // "q" is a subsequence of "qx": the lines on show hold every match.
        let qx = q.filtered(&items, query_words("qx"), true);
        assert_eq!(qx.lines, [0, 1]);
        // "qx" is no subsequence of "xq": every line is checked again.
        assert_eq!(qx.filtered(&items, query_words("un"), true).lines, [2, 3]);
    }

    #[test]
    fn selection_follows_first_visible_row() {
        let items = lines(&["Alpha", "Needle Item", "Omega"]);
        let first = |query: &str| shown(&items, query).first().copied();
        assert_eq!(first(""), Some("Alpha"));
        assert_eq!(first("needle"), Some("Needle Item"));
        assert_eq!(first("OMEGA"), Some("Omega"));
        assert_eq!(first("a"), Some("Alpha"));
        assert_eq!(first("unmatched free text"), None);
        assert_eq!(shown(&[], "").first(), None);
    }

    #[test]
    fn typing_on_narrows_and_editing_widens_the_rows() {
        let items = lines(&["needle a", "needle b", "hay", "Needle backup", "b needle"]);
        let all = Shown::all(&items, false);
        let needle = all.filtered(&items, query_words("needle"), false);
        assert_eq!(needle.lines, [0, 1, 3, 4]);
        let narrowed = needle.filtered(&items, query_words("needle b"), false);
        assert_eq!(narrowed.lines, [1, 3, 4]);
        // Removing a word, or changing it to one the shown rows were not
        // filtered for, checks every line again.
        assert_eq!(
            narrowed.filtered(&items, query_words("hay"), false).lines,
            [2]
        );
        assert_eq!(
            narrowed.filtered(&items, query_words(""), false).lines,
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            narrowed
                .filtered(&items, query_words("needle backup"), false)
                .lines,
            [3]
        );
    }

    #[test]
    fn page_entries_divide_label_and_value_at_the_first_tab() {
        let pair =
            |entry: &[u8]| Line::pair(entry).map(|line| (line.text.clone(), line.bytes().to_vec()));
        assert_eq!(
            pair(b"Mail\tmail.example.org"),
            Some(("Mail".into(), b"mail.example.org".to_vec()))
        );
        assert_eq!(
            pair(b"label\tvalue\tmore"),
            Some(("label".into(), b"value\tmore".to_vec()))
        );
        assert_eq!(
            pair(b"  padded  "),
            Some(("  padded  ".into(), b"  padded  ".to_vec()))
        );
        assert_eq!(pair(b"\tvalue"), Some(("value".into(), b"value".to_vec())));
        assert_eq!(pair(b"label\t"), Some(("label".into(), b"label".to_vec())));
        assert_eq!(pair(b"\t"), None);
        assert_eq!(
            pair(b"caf\xe9"),
            Some(("caf\u{fffd}".into(), b"caf\xe9".to_vec()))
        );
        assert!(is_blank(b"") && is_blank(b" \t\r") && !is_blank(b" x "));
    }

    #[test]
    fn exec_words_take_the_whole_value() {
        let words = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        let value = b"a 'b' $(x); end";
        assert_eq!(
            exec_words(&words(&["probe", "pre%s:%s", "%%", "%q", "%"]), value),
            [
                &b"probe"[..],
                b"prea 'b' $(x); end:a 'b' $(x); end",
                b"%",
                b"%q",
                b"%"
            ]
        );
        assert_eq!(
            exec_words(&words(&["probe", "50%%s"]), b"v"),
            [&b"probe"[..], b"50%s", b"v"]
        );
        assert_eq!(
            exec_words(&words(&["probe"]), b"caf\xe9"),
            [&b"probe"[..], b"caf\xe9"]
        );
    }

    #[test]
    fn lines_inserted_ahead_move_the_lines_on_show() {
        let mut shown = Shown {
            lines: vec![0, 2, 3],
            ..Shown::default()
        };
        assert_eq!(shown.insert_at(2), 1);
        assert_eq!(shown.lines, [0, 3, 4]);
        assert_eq!(shown.insert_at(9), 3);
        assert_eq!(shown.lines, [0, 3, 4]);
    }
}
