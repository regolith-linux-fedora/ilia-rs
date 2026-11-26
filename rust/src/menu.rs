//! Menu pages: page files whose `Entries` list a tree of desktop actions.
//! Rows open submenus, run the window manager's own bindings or run
//! commands; `When` commands decide whether rows show and `State`
//! commands give them an on/off column, kept up to date while the row is
//! on screen.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::filter::{contains_all_words, fold_text, is_subsequence, query_words};
use crate::icons::load_icon_from_name;
use crate::keybindings::{self, navigate, row_under_pointer};
use crate::page_files::{EntryKind, Menu, PageFile, Words};
use crate::pages::{is_enter_key, select_first_row, select_row_near, DialogPage, FUZZY_MATCHING};
use crate::runner::scope_command;
use crate::textlist;

const HELP: &str = "This dialog presents a menu of desktop actions. The user may filter the list in the top text box, which searches every level below the current one. Enter or clicking on an item runs it or opens its submenu; Backspace with an empty filter, or Alt+Left, returns to the level above.";
const FALLBACK_ICON: &str = "applications-other";

/// What the second column shows for a submenu.
const SUBMENU_MARK: &str = "\u{203a}";
/// What joins the names of the entries from the current level down to a
/// deeper one.
const PATH_SEPARATOR: &str = " \u{203a} ";

/// How long a `When` or `State` command may run before it is stopped.
const COMMAND_TIME: Duration = Duration::from_secs(2);
/// How long the launcher, about to exit, waits for its stopped commands to
/// be collected.
const STOP_WAIT: Duration = Duration::from_millis(1500);
/// How long after a `State` command ends it runs again, while its row is
/// listed on the tab on screen.
const STATE_INTERVAL: Duration = Duration::from_secs(5);
/// The most of a `State` command's first line that is kept: far more than
/// any state word, so that a longer line still gives neither state, and
/// little enough that a command printing without end costs nothing.
const MAX_LINE: usize = 4096;

// The columns of the rows.
const ICON: u32 = 0;
const NAME: u32 = 1;
const SECOND: u32 = 2;
const STATE: u32 = 3;

/// Where a `When` command stands.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Condition {
    /// Not finished yet, or not started yet: the entry is hidden.
    Pending,
    Passed,
    Failed,
}

/// A row on show: the entries from the current level down to it, and its
/// text.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    path: Vec<usize>,
    text: String,
}

/// What a `When` or `State` command reports.
enum Report {
    /// A `When` command finished: whether it succeeded.
    When(usize, bool),
    /// A `State` command printed its first line: on, off, or neither.
    State(usize, Option<bool>),
    /// A `State` command ended, or was stopped.
    StateEnded(usize),
}

/// Where the runs of an entry's `State` command stand.
#[derive(Default)]
struct StateRuns {
    /// A run is in flight.
    running: bool,
    /// When the last run ended.
    ended: Option<Instant>,
    /// The next run, waiting for its time.
    timer: Option<glib::SourceId>,
}

pub struct MenuPage {
    id: String,
    // The tab title, as markup.
    name: String,
    icon: String,
    key: Option<char>,
    menu: Menu,
    wm_name: String,
    // The name the entries' show-in keys give the window manager.
    manager: String,
    entry: gtk::Entry,
    // The filter text, as the entry last held it while the page was in
    // view: reports that come while another tab shows match against it.
    filter: RefCell<String>,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    quit: Rc<dyn Fn()>,
    // The key text and command of each binding entry that matched.
    bindings: Vec<Option<(String, String)>>,
    conditions: RefCell<Vec<Condition>>,
    states: RefCell<Vec<Option<bool>>>,
    state_runs: RefCell<Vec<StateRuns>>,
    // Where the commands report, once they have been started.
    reports: RefCell<Option<glib::Sender<Report>>>,
    // The submenus opened, from the top.
    level: RefCell<Vec<usize>>,
    rows: RefCell<Vec<Row>>,
    // Each row's icon, by icon name; `None` when rows show no icons.
    icons: Option<RefCell<HashMap<String, Option<Pixbuf>>>>,
    icon_size: i32,
    subsequence: Cell<bool>,
    // Whether the commands of the tree have been started.
    started: Cell<bool>,
    // Set once a row has run: the launcher closes after the first choice.
    ran: Cell<bool>,
    // The entry whose row the primary button went down on, from the top:
    // rows can move before the button comes up.
    pressed: RefCell<Option<Vec<usize>>>,
    // The last press of the primary button without Shift or Ctrl: its
    // time and root position.
    last_press: Cell<Option<(u32, f64, f64)>>,
    this: Weak<MenuPage>,
}

impl MenuPage {
    /// The page `file` defines, opened at the submenus `level` (from the
    /// top). `wm_name` is the running window manager: `i3`, `sway` or
    /// `Unknown`.
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        file: &PageFile,
        key: Option<char>,
        quit: Rc<dyn Fn()>,
        wm_name: &str,
        level: Vec<usize>,
    ) -> Rc<Self> {
        let menu = file.menu().cloned().unwrap_or_default();
        let manager = match wm_name {
            "i3" | "sway" => wm_name,
            _ => "other",
        };
        let bindings = match_bindings(&menu, wm_name);
        let conditions = menu
            .entries
            .iter()
            .map(|entry| match entry.when {
                Some(_) => Condition::Pending,
                None => Condition::Passed,
            })
            .collect();

        let icon_size = settings.int("icon-size");
        let view = gtk::TreeView::new();
        view.set_headers_visible(false);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        if icon_size > 0 {
            let renderer = gtk::CellRendererPixbuf::new();
            // Rows whose icon cannot be found keep the column's width.
            renderer.set_fixed_size(icon_size, icon_size);
            let column =
                gtk::TreeViewColumn::with_attributes("Icon", &renderer, &[("pixbuf", ICON as i32)]);
            view.append_column(&column);
        }
        for (column, title) in [(NAME, "Name"), (SECOND, "Kind"), (STATE, "State")] {
            let renderer = gtk::CellRendererText::new();
            let view_column =
                gtk::TreeViewColumn::with_attributes(title, &renderer, &[("text", column as i32)]);
            view_column.set_expand(column == NAME);
            view.append_column(&view_column);
        }
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let entries = menu.entries.len();
        let page = Rc::new_cyclic(|this| Self {
            id: file.id.clone(),
            name: glib::markup_escape_text(&file.name).into(),
            icon: file.icon.clone(),
            key,
            menu,
            wm_name: wm_name.to_owned(),
            manager: manager.to_owned(),
            entry,
            filter: RefCell::default(),
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            quit,
            bindings,
            conditions: RefCell::new(conditions),
            states: RefCell::new(vec![None; entries]),
            state_runs: RefCell::new((0..entries).map(|_| StateRuns::default()).collect()),
            reports: RefCell::new(None),
            level: RefCell::new(level),
            rows: RefCell::new(Vec::new()),
            icons: (icon_size > 0).then(RefCell::default),
            icon_size,
            subsequence: Cell::new(settings.boolean(FUZZY_MATCHING)),
            started: Cell::new(false),
            ran: Cell::new(false),
            pressed: RefCell::new(None),
            last_press: Cell::new(None),
            this: this.clone(),
        });
        page.connect_signals(&settings);
        page.refresh(Highlight::First);
        page
    }

    fn connect_signals(&self, settings: &gio::Settings) {
        // The commands start when the page is first shown: at start-up, or
        // when its tab is first selected. `State` commands then run again
        // while their rows are listed on screen, which ends when the tab
        // is left.
        let this = self.this.clone();
        self.root.connect_map(move |_| {
            if let Some(page) = this.upgrade() {
                page.start();
                page.schedule_states();
            }
        });
        let this = self.this.clone();
        self.root.connect_unmap(move |_| {
            if let Some(page) = this.upgrade() {
                page.schedule_states();
            }
        });
        let this = self.this.clone();
        settings.connect_changed(Some(FUZZY_MATCHING), move |settings, _| {
            let Some(page) = this.upgrade() else {
                return;
            };
            let subsequence = settings.boolean(FUZZY_MATCHING);
            if page.subsequence.replace(subsequence) != subsequence {
                page.refresh(Highlight::Keep);
            }
        });
        // Clicks are handled here, under the X11 input grab and on Wayland
        // alike: a row acts when the primary button goes down and comes up
        // on it, and the second press of a double click does nothing.
        self.view
            .add_events(gdk::EventMask::BUTTON_PRESS_MASK | gdk::EventMask::BUTTON_RELEASE_MASK);
        let this = self.this.clone();
        self.view.connect_button_press_event(move |view, event| {
            if let Some(page) = this.upgrade() {
                page.button_pressed(view, event);
            }
            glib::Propagation::Stop
        });
        let this = self.this.clone();
        self.view.connect_button_release_event(move |view, event| {
            if let Some(page) = this.upgrade() {
                page.button_released(view, event);
            }
            glib::Propagation::Stop
        });
    }

    fn button_pressed(&self, view: &gtk::TreeView, event: &gdk::EventButton) {
        if event.event_type() != gdk::EventType::ButtonPress {
            // The extra event of a double or triple click: the press before
            // it has been handled.
            return;
        }
        self.pressed.replace(None);
        if event.button() != gdk::BUTTON_PRIMARY {
            return;
        }
        let modifiers = gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK;
        let plain = !event.state().intersects(modifiers);
        let (x, y) = event.root();
        let press = (event.time(), x, y);
        let previous = self.last_press.replace(plain.then_some(press));
        // The second press of a double click (or the third of a triple one)
        // does nothing.
        let repeated = previous.is_some_and(|previous| is_repeat_press(view, previous, press));
        if plain && !repeated {
            let entry = self
                .row_at(view, event)
                .and_then(|row| self.entry_of_row(row));
            self.pressed.replace(entry);
        }
    }

    /// The row under the pointer at a button event, by position.
    fn row_at(&self, view: &gtk::TreeView, event: &gdk::EventButton) -> Option<usize> {
        let path = row_under_pointer(view, event)?;
        usize::try_from(*path.indices().first()?).ok()
    }

    /// The entry listed at `row`, as the chain of entries from the top.
    fn entry_of_row(&self, row: usize) -> Option<Vec<usize>> {
        let rows = self.rows.borrow();
        let row = rows.get(row)?;
        Some(
            self.level
                .borrow()
                .iter()
                .chain(&row.path)
                .copied()
                .collect(),
        )
    }

    fn button_released(&self, view: &gtk::TreeView, event: &gdk::EventButton) {
        if event.button() != gdk::BUTTON_PRIMARY {
            return;
        }
        let Some(pressed) = self.pressed.take() else {
            return;
        };
        let modifiers = gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK;
        if event.state().intersects(modifiers) {
            return;
        }
        // The press and the release must land on one entry, wherever its
        // row is at each moment.
        let Some(row) = self.row_at(view, event) else {
            return;
        };
        if self.entry_of_row(row).as_ref() == Some(&pressed) {
            self.activate(row);
        }
    }

    /// Starts every `When` and `State` command of the tree, the first time
    /// it is called.
    fn start(&self) {
        if self.started.get() {
            return;
        }
        let (sender, receiver) = report_channel();
        let this = self.this.clone();
        receiver.attach(None, move |report| {
            let Some(page) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            page.take_report(report);
            glib::ControlFlow::Continue
        });
        self.reports.replace(Some(sender.clone()));
        let mut failed = Vec::new();
        for (index, entry) in self.menu.entries.iter().enumerate() {
            if let Some(words) = &entry.when {
                if !run_condition(words, index, false, &sender) {
                    failed.push(index);
                }
            }
            if entry.state.is_some() {
                self.run_state(index);
            }
        }
        // Only now may refreshes start `State` commands again.
        self.started.set(true);
        for index in failed {
            self.take_report(Report::When(index, false));
        }
    }

    /// Runs the `State` command of the entry `index` once; whether it
    /// started.
    fn run_state(&self, index: usize) -> bool {
        let Some(words) = &self.menu.entries[index].state else {
            return false;
        };
        let Some(sender) = self.reports.borrow().clone() else {
            return false;
        };
        let started = run_condition(words, index, true, &sender);
        let mut runs = self.state_runs.borrow_mut();
        if started {
            runs[index].running = true;
        } else {
            // A command that cannot be started is tried again later, like
            // one that ended.
            runs[index].ended = Some(Instant::now());
        }
        started
    }

    /// Plans the next run of each `State` command: about `STATE_INTERVAL`
    /// after its last run ended, while its row is listed on the tab on
    /// screen and no run of it is in flight. The runs of rows not listed
    /// there wait until they are.
    fn schedule_states(&self) {
        if !self.started.get() {
            return;
        }
        let listed: HashSet<usize> = if self.root.is_mapped() {
            self.rows
                .borrow()
                .iter()
                .filter_map(|row| row.path.last().copied())
                .collect()
        } else {
            HashSet::new()
        };
        let now = Instant::now();
        let mut due = Vec::new();
        for (index, runs) in self.state_runs.borrow_mut().iter_mut().enumerate() {
            if self.menu.entries[index].state.is_none() {
                continue;
            }
            if !listed.contains(&index) {
                if let Some(timer) = runs.timer.take() {
                    timer.remove();
                }
                continue;
            }
            if runs.running || runs.timer.is_some() {
                continue;
            }
            let next = runs.ended.map_or(now, |ended| ended + STATE_INTERVAL);
            if next <= now {
                due.push(index);
                continue;
            }
            let this = self.this.clone();
            runs.timer = Some(glib::timeout_add_local_once(next - now, move || {
                if let Some(page) = this.upgrade() {
                    page.state_runs.borrow_mut()[index].timer = None;
                    page.schedule_states();
                }
            }));
        }
        // A command that could not start waits for its next turn.
        let mut retry = false;
        for index in due {
            retry |= !self.run_state(index);
        }
        if retry {
            self.schedule_states();
        }
    }

    fn take_report(&self, report: Report) {
        let changed_state = match report {
            Report::When(index, passed) => {
                let condition = if passed {
                    Condition::Passed
                } else {
                    Condition::Failed
                };
                if std::mem::replace(&mut self.conditions.borrow_mut()[index], condition)
                    == condition
                {
                    return;
                }
                None
            }
            // A run that gives neither on nor off leaves the column as it
            // was.
            Report::State(_, None) => return,
            Report::State(index, state) => {
                if std::mem::replace(&mut self.states.borrow_mut()[index], state) == state {
                    return;
                }
                Some(index)
            }
            Report::StateEnded(index) => {
                {
                    let mut runs = self.state_runs.borrow_mut();
                    runs[index].running = false;
                    runs[index].ended = Some(Instant::now());
                }
                self.schedule_states();
                return;
            }
        };
        // A report that lists the same rows, such as a hidden row's
        // condition settling or a new state, leaves the list, its highlight
        // and its scrolling as they are: at most a state cell changes.
        if self.matching_rows() != *self.rows.borrow() {
            self.refresh(Highlight::Keep);
        } else if let Some(index) = changed_state {
            self.show_state(index);
        }
    }

    /// Writes the state of the entry `index` into the cells of its rows.
    fn show_state(&self, index: usize) {
        let Some(store) = self
            .view
            .model()
            .and_then(|model| model.downcast::<gtk::ListStore>().ok())
        else {
            return;
        };
        let text = state_text(self.states.borrow()[index]);
        for (position, row) in self.rows.borrow().iter().enumerate() {
            if row.path.last() != Some(&index) {
                continue;
            }
            let Ok(position) = i32::try_from(position) else {
                break;
            };
            if let Some(iter) = store.iter_nth_child(None, position) {
                store.set_value(&iter, STATE, &text.to_value());
            }
        }
    }

    /// Whether the entry `index` is shown: its show-in keys admit the
    /// window manager, its `When` command succeeded, a binding matched and
    /// a submenu has an entry shown somewhere below it.
    fn is_shown(&self, index: usize) -> bool {
        let entry = &self.menu.entries[index];
        if !entry.shows_in(&self.manager) || self.conditions.borrow()[index] != Condition::Passed {
            return false;
        }
        match &entry.kind {
            EntryKind::Submenu(children) => children.iter().any(|&child| self.is_shown(child)),
            EntryKind::Binding(_) => self.bindings[index].is_some(),
            EntryKind::Exec(_) => true,
        }
    }

    /// The rows for the current level and filter text: the shown entries
    /// of the level, or, with filter words, the shown entries at every
    /// depth below it whose text matches, in depth-first listed order.
    fn matching_rows(&self) -> Vec<Row> {
        let level = self.level.borrow();
        let children = self.menu.children(&level);
        let words = query_words(&self.filter.borrow());
        let mut rows = Vec::new();
        if words.is_empty() {
            for &child in children {
                if self.is_shown(child) {
                    rows.push(Row {
                        path: vec![child],
                        text: self.menu.entries[child].name.clone(),
                    });
                }
            }
        } else {
            self.search(children, &mut Vec::new(), "", &words, &mut rows);
        }
        rows
    }

    fn search(
        &self,
        children: &[usize],
        path: &mut Vec<usize>,
        prefix: &str,
        words: &[String],
        rows: &mut Vec<Row>,
    ) {
        for &child in children {
            if !self.is_shown(child) {
                continue;
            }
            let entry = &self.menu.entries[child];
            let text = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}{PATH_SEPARATOR}{}", entry.name)
            };
            path.push(child);
            let folded = fold_text(&text);
            let matches = if self.subsequence.get() {
                words.iter().all(|word| is_subsequence(word, &folded))
            } else {
                contains_all_words(&folded, words)
            };
            if matches {
                rows.push(Row {
                    path: path.clone(),
                    text: text.clone(),
                });
            }
            if let EntryKind::Submenu(grandchildren) = &entry.kind {
                self.search(grandchildren, path, &text, words, rows);
            }
            path.pop();
        }
    }

    /// Lists the rows for the current level and filter text, highlighting
    /// as `highlight` says.
    fn refresh(&self, highlight: Highlight) {
        let selected = self
            .selected_row()
            .and_then(|row| self.rows.borrow().get(row).map(|row| row.path.clone()));
        let rows = self.matching_rows();
        let store = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        for row in &rows {
            let index = *row.path.last().expect("a row has an entry");
            let entry = &self.menu.entries[index];
            let second = match &entry.kind {
                EntryKind::Submenu(_) => SUBMENU_MARK,
                EntryKind::Binding(_) => self.bindings[index]
                    .as_ref()
                    .map_or("", |(keys, _)| keys.as_str()),
                EntryKind::Exec(_) => "",
            };
            let state = state_text(self.states.borrow()[index]);
            let icon = self.row_icon(entry.icon.as_deref().unwrap_or(&self.icon));
            store.insert_with_values(
                None,
                &[
                    (ICON, &icon),
                    (NAME, &row.text),
                    (SECOND, &second),
                    (STATE, &state),
                ],
            );
        }
        let position = match highlight {
            Highlight::First => None,
            Highlight::Keep => match selected {
                Some(path) => rows.iter().position(|row| row.path == path),
                None => None,
            },
            Highlight::Entry(index) => rows.iter().position(|row| row.path == [index]),
        };
        self.rows.replace(rows);
        self.view.set_model(Some(&store));
        *self.path.borrow_mut() = match position {
            Some(position) => select_row_near(&self.view, position as i32)
                .unwrap_or_else(gtk::TreePath::new_first),
            None => {
                self.view.selection().set_mode(gtk::SelectionMode::Single);
                select_first_row(&self.view)
            }
        };
        // Rows that came or went start or end their `State` refreshes.
        self.schedule_states();
    }

    /// The icon of a row naming `name`, when rows show icons.
    fn row_icon(&self, name: &str) -> Option<Pixbuf> {
        let icons = self.icons.as_ref()?;
        icons
            .borrow_mut()
            .entry(name.to_owned())
            .or_insert_with(|| {
                let theme = gtk::IconTheme::default()?;
                load_icon_from_name(&theme, name, self.icon_size)
                    .or_else(|| load_icon_from_name(&theme, FALLBACK_ICON, self.icon_size))
            })
            .clone()
    }

    fn selected_row(&self) -> Option<usize> {
        let (model, iter) = self.view.selection().selected()?;
        let path = model.path(&iter)?;
        usize::try_from(*path.indices().first()?).ok()
    }

    /// Acts on the listed row `row`: opens a submenu, or runs a binding or
    /// a command and closes the launcher.
    fn activate(&self, row: usize) {
        let Some(row) = self.rows.borrow().get(row).cloned() else {
            return;
        };
        let index = *row.path.last().expect("a row has an entry");
        match &self.menu.entries[index].kind {
            EntryKind::Submenu(_) => {
                self.level.borrow_mut().extend(&row.path);
                self.show_level(Highlight::First);
            }
            EntryKind::Binding(_) => {
                let Some((_, exec)) = &self.bindings[index] else {
                    return;
                };
                if !self.ran.replace(true) {
                    keybindings::run_binding(&self.wm_name, exec);
                    (self.quit)();
                }
            }
            EntryKind::Exec(words) => {
                let Some(words) = words else {
                    return;
                };
                if !self.ran.replace(true) {
                    if let Err(error) = run_exec(&self.id, words) {
                        eprintln!(
                            "ilia: cannot run the command of entry '{}' of page '{}': {error}",
                            self.menu.entries[index].id, self.id
                        );
                    }
                    (self.quit)();
                }
            }
        }
    }

    /// Shows the current level with an empty filter.
    fn show_level(&self, highlight: Highlight) {
        if self.filter.borrow().is_empty() {
            self.refresh(highlight);
        } else {
            // The change lists the level's rows, the first one highlighted.
            self.entry.set_text("");
            if !matches!(highlight, Highlight::First) {
                self.refresh(highlight);
            }
        }
    }

    /// Returns to the level above, highlighting the submenu that led in;
    /// whether there was one.
    fn climb(&self) -> bool {
        let Some(left) = self.level.borrow_mut().pop() else {
            return false;
        };
        self.show_level(Highlight::Entry(left));
        true
    }
}

/// Which row a refresh highlights.
#[derive(Clone, Copy)]
enum Highlight {
    /// The first row.
    First,
    /// The row highlighted before, while it is listed; else the first.
    Keep,
    /// The row of this entry of the level, when listed; else the first.
    Entry(usize),
}

impl DialogPage for MenuPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        self.filter.replace(self.entry.text().into());
        self.refresh(Highlight::First);
    }
    fn on_entry_activated(&self) {
        if let Some(row) = self.selected_row() {
            self.activate(row);
        }
    }
    fn get_id(&self) -> &str {
        &self.id
    }
    fn get_name(&self) -> &str {
        &self.name
    }
    fn get_icon_name(&self) -> &str {
        &self.icon
    }
    fn get_help(&self) -> &str {
        HELP
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [
            ("enter", "Run Item or Open Submenu"),
            ("backspace", "Return to the Level Above"),
            ("alt+left", "Return to the Level Above"),
        ]
        .into_iter()
        .collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        let key = event.keyval();
        if is_enter_key(&key) {
            // With nothing listed, Enter does nothing.
            self.on_entry_activated();
            return true;
        }
        if key == gdk::keys::constants::BackSpace
            && !event.state().intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::MOD1_MASK
                    | gdk::ModifierType::SUPER_MASK,
            )
            && self.entry.text().is_empty()
        {
            // At the top this does nothing.
            self.climb();
            return true;
        }
        navigate(&self.view, &self.path, event)
    }
    fn capture_key_event(&self, event: &gdk::EventKey) -> bool {
        // Alt+Left returns to the level above; at the top it does what it
        // does on every page.
        let modifiers = event.state()
            & (gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::MOD1_MASK
                | gdk::ModifierType::SUPER_MASK);
        let nested = !self.level.borrow().is_empty();
        nested
            && matches!(
                event.keyval(),
                gdk::keys::constants::Left | gdk::keys::constants::KP_Left
            )
            && modifiers == gdk::ModifierType::MOD1_MASK
            && self.climb()
    }
    fn get_keybinding(&self) -> Option<char> {
        self.key
    }
    fn show(&self) {
        self.view.grab_focus();
    }
}

/// The key text and command of the binding each binding entry names, when
/// one matches among the rows of the Keybindings page in this session.
fn match_bindings(menu: &Menu, wm_name: &str) -> Vec<Option<(String, String)>> {
    let has_bindings = menu
        .entries
        .iter()
        .any(|entry| matches!(entry.kind, EntryKind::Binding(_)));
    let bindings = if has_bindings && matches!(wm_name, "i3" | "sway") {
        keybindings::page_bindings(wm_name).unwrap_or_default()
    } else {
        Vec::new()
    };
    menu.entries
        .iter()
        .map(|entry| match &entry.kind {
            EntryKind::Binding(name) => keybindings::find_binding(&bindings, name)
                .map(|binding| (binding.keys.clone(), binding.exec.clone())),
            _ => None,
        })
        .collect()
}

/// Whether a press of the primary button at `current` (time, root x, root
/// y) is the second press of a double click that began at `previous`.
fn is_repeat_press(
    view: &gtk::TreeView,
    previous: (u32, f64, f64),
    current: (u32, f64, f64),
) -> bool {
    let (time, distance) = WidgetExt::screen(view)
        .and_then(|screen| gtk::Settings::for_screen(&screen))
        .map_or((400, 5), |settings| {
            (
                settings.gtk_double_click_time(),
                settings.gtk_double_click_distance(),
            )
        });
    let elapsed = current.0.wrapping_sub(previous.0);
    let (dx, dy) = (
        (current.1 - previous.1).abs(),
        (current.2 - previous.2).abs(),
    );
    i64::from(elapsed) <= i64::from(time) && dx <= f64::from(distance) && dy <= f64::from(distance)
}

/// What the third column shows for a state.
fn state_text(state: Option<bool>) -> &'static str {
    match state {
        Some(true) => "On",
        Some(false) => "Off",
        None => "",
    }
}

/// The state a `State` command's first line gives: `on`, `true` or `1`
/// for on, `off`, `false` or `0` for off, trimmed and in any letter case.
fn state_of(line: &[u8]) -> Option<bool> {
    let line = String::from_utf8_lossy(line);
    match line.trim().to_lowercase().as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Starts an entry's command, `words`, directly (never through a shell or
/// a scope), in the launcher's working directory, with standard input from
/// `/dev/null`, in a process group of its own, which the launcher's
/// guardian ends if the launcher goes away first. A thread waits for it,
/// stops it with its group after `COMMAND_TIME` and reports through
/// `sender`: the first line of a `State` command (`state`) and its end,
/// else whether the command succeeded. Returns whether the command
/// started. Called on the main thread only.
#[allow(deprecated)]
fn run_condition(words: &Words, entry: usize, state: bool, sender: &glib::Sender<Report>) -> bool {
    let Some((program, args)) = words.as_deref().and_then(<[String]>::split_first) else {
        return false;
    };
    // Processes a command leaves behind become the launcher's children, so
    // that they can be collected once their group is stopped.
    // SAFETY: PR_SET_CHILD_SUBREAPER takes integer arguments only and
    // touches no memory of this process.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let guardian = guardian();
    let mut commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(if state { Stdio::piped() } else { Stdio::null() })
        .process_group(0);
    if let Some(guardian) = guardian {
        // The command makes its group known to the guardian itself, before
        // it runs: a launcher killed at any moment leaves none unknown.
        // SAFETY: the hook runs in the new process before exec and only
        // makes async-signal-safe calls on its own stack.
        unsafe {
            command.pre_exec(move || {
                tell_guardian(guardian, Guard::Started, libc::getpid());
                Ok(())
            });
        }
    }
    let child = command.spawn();
    let Ok(mut child) = child else {
        if let Some(guardian) = guardian {
            // The process that announced itself is gone without running.
            tell_guardian(guardian, Guard::Failed, 0);
        }
        return false;
    };
    // Process ids are positive `pid_t` values.
    let pid = child.id() as libc::pid_t;
    if let Some(guardian) = guardian {
        tell_guardian(guardian, Guard::Running, pid);
    }
    let output = child.stdout.take().map(OwnedFd::from);
    // The child is collected by its watcher, never through `Child`.
    drop(child);
    commands.running.push(pid);
    commands.watchers += 1;
    drop(commands);
    let reports = sender.clone();
    let deadline = Instant::now() + COMMAND_TIME;
    let watcher = std::thread::Builder::new()
        .name("ilia-menu-command".into())
        .spawn(move || {
            // The first line counts as soon as it arrives.
            let status = watch(pid, output, deadline, |line| {
                let _ = reports.send(Report::State(entry, state_of(&line)));
            });
            stop_group(pid);
            if state {
                let _ = reports.send(Report::StateEnded(entry));
            } else {
                let passed = status.is_some_and(|status| status.success());
                let _ = reports.send(Report::When(entry, passed));
            }
            let mut commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
            commands.watchers -= 1;
            STOPPED.notify_all();
        });
    if watcher.is_err() {
        stop_group(pid);
        let mut commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
        commands.watchers -= 1;
        drop(commands);
        if state {
            let _ = sender.send(Report::StateEnded(entry));
        }
    }
    true
}

/// The `When` and `State` commands not yet collected, by process (and
/// group) id, and how many watchers are still at work.
struct Commands {
    running: Vec<libc::pid_t>,
    watchers: usize,
}

static COMMANDS: Mutex<Commands> = Mutex::new(Commands {
    running: Vec::new(),
    watchers: 0,
});
static STOPPED: Condvar = Condvar::new();

/// Waits until the command `pid` exits or `deadline` passes, and returns
/// its exit status when it exited in time. Meanwhile the first line of
/// `output` goes to `on_line`, without its newline, as soon as it is read.
/// The command is left uncollected, so that its process and group ids stay
/// its own.
fn watch(
    pid: libc::pid_t,
    output: Option<OwnedFd>,
    deadline: Instant,
    mut on_line: impl FnMut(Vec<u8>),
) -> Option<std::process::ExitStatus> {
    // SAFETY: pidfd_open takes integer arguments and returns a new
    // descriptor or -1.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    // SAFETY: a descriptor pidfd_open just returned, owned by nothing else.
    let pidfd = (pidfd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(pidfd) });
    let mut reader = output.map(LineReader::new);
    let mut line = None;
    loop {
        if let Some(status) = exit_status(pid) {
            // What the command printed before exiting counts.
            if line.is_none() {
                if let Some(first) = reader.as_mut().and_then(LineReader::finish) {
                    on_line(first);
                }
            }
            return Some(status);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let mut wait = deadline - now;
        if pidfd.is_none() {
            wait = wait.min(Duration::from_millis(20));
        }
        let mut fds = Vec::new();
        if let Some(pidfd) = &pidfd {
            fds.push(libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(reader) = reader
            .as_ref()
            .filter(|reader| line.is_none() && !reader.ended)
        {
            fds.push(libc::pollfd {
                fd: reader.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = i32::try_from(wait.as_millis().max(1)).unwrap_or(i32::MAX);
        // SAFETY: `fds` holds `fds.len()` valid pollfd structures.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if line.is_none() {
            if let Some(reader) = reader.as_mut() {
                line = reader.read_line();
                if reader.ended && line.is_none() {
                    line = reader.finish();
                }
                if let Some(first) = &line {
                    on_line(first.clone());
                }
            }
        }
    }
}

/// Reads the first line of a command's output without blocking.
struct LineReader {
    fd: OwnedFd,
    buffer: Vec<u8>,
    ended: bool,
}

impl LineReader {
    fn new(fd: OwnedFd) -> Self {
        // SAFETY: fcntl on a descriptor this reader owns.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Self {
            fd,
            buffer: Vec::new(),
            ended: false,
        }
    }

    /// Reads what is waiting: the first line, once it is complete. A line
    /// longer than `MAX_LINE` counts as complete there.
    fn read_line(&mut self) -> Option<Vec<u8>> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                return Some(self.buffer[..end].to_vec());
            }
            if self.buffer.len() >= MAX_LINE {
                return Some(self.buffer[..MAX_LINE].to_vec());
            }
            if self.ended {
                return None;
            }
            // SAFETY: `chunk` is a live, writable buffer of the length given.
            let read =
                unsafe { libc::read(self.fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            match read {
                0 => self.ended = true,
                read if read > 0 => self.buffer.extend_from_slice(&chunk[..read as usize]),
                _ => match io::Error::last_os_error().kind() {
                    io::ErrorKind::Interrupted => {}
                    // Nothing waiting yet.
                    io::ErrorKind::WouldBlock => return None,
                    // Nothing more will come.
                    _ => self.ended = true,
                },
            }
        }
    }

    /// The first line once nothing more will be read in time: a complete
    /// line, or what the output ended with.
    fn finish(&mut self) -> Option<Vec<u8>> {
        self.read_line()
            .or_else(|| (self.ended && !self.buffer.is_empty()).then(|| self.buffer.clone()))
    }
}

/// The exit status of the child `pid` once it has exited, leaving it
/// uncollected.
fn exit_status(pid: libc::pid_t) -> Option<std::process::ExitStatus> {
    // SAFETY: an all-zero siginfo_t is a valid value, and waitid writes
    // into the live structure passed.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0 {
        return None;
    }
    // SAFETY: waitid filled in the child fields when it found a child.
    let (child, status) = unsafe { (info.si_pid(), info.si_status()) };
    if child == 0 {
        return None;
    }
    let raw = match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        _ => status & 0x7f,
    };
    Some(std::process::ExitStatus::from_raw(raw))
}

/// Stops every process of the group of the command `pid` and collects
/// those that are the launcher's children: the command, and what it left
/// behind, which the subreaper setting hands to the launcher. Until the
/// command is collected its id names no other group.
fn stop_group(pid: libc::pid_t) {
    {
        let mut commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
        // The command is not collected yet, so the group id is still its.
        // SAFETY: killpg takes plain integers and touches no memory.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        commands.running.retain(|&running| running != pid);
        // Once collected, the id may name another group: the guardian must
        // leave it alone from now on.
        if let Some(guardian) = started_guardian() {
            tell_guardian(guardian, Guard::Stopped, pid);
        }
    }
    loop {
        let mut status = 0;
        // SAFETY: `status` is a live, writable c_int for the call.
        if unsafe { libc::waitpid(-pid, &mut status, 0) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            break;
        }
    }
}

/// Stops every `When` and `State` command still running, with its process
/// group, and waits a moment for them to be collected: none outlives the
/// launcher. However the launcher ends, its guardian does the same.
pub fn stop_commands() {
    let mut commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
    for &pid in &commands.running {
        // SAFETY: killpg takes plain integers and touches no memory.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        if let Some(guardian) = started_guardian() {
            tell_guardian(guardian, Guard::Stopped, pid);
        }
    }
    let deadline = Instant::now() + STOP_WAIT;
    while commands.watchers > 0 {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        commands = STOPPED
            .wait_timeout(commands, deadline - now)
            .map(|(commands, _)| commands)
            .unwrap_or_else(|error| error.into_inner().0);
    }
}

/// What the guardian is told about a command's process group.
#[derive(Clone, Copy)]
#[repr(i32)]
enum Guard {
    /// Sent by a command about to run: its group may need ending.
    Started = 1,
    /// The command started is running, as this group.
    Running = 2,
    /// The command announced last could not run after all.
    Failed = 3,
    /// The launcher stopped this group itself: leave the id alone.
    Stopped = 4,
}

/// The launcher's end of its connection to the guardian, once started:
/// `None` inside when it could not be started.
static GUARDIAN: OnceLock<Option<libc::c_int>> = OnceLock::new();

/// How many groups the guardian can hold at once.
const GUARDED_GROUPS: usize = 4096;

/// The guardian, started the first time: a small process of its own that
/// outlives the launcher only to end the process groups of the commands
/// still running when the launcher goes away, however it ends, even by a
/// signal that cannot be caught. It notices by its connection to the
/// launcher closing. Called on the main thread.
fn guardian() -> Option<libc::c_int> {
    *GUARDIAN.get_or_init(start_guardian)
}

/// The guardian, if it was started.
fn started_guardian() -> Option<libc::c_int> {
    GUARDIAN.get().copied().flatten()
}

fn start_guardian() -> Option<libc::c_int> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a live array of the two descriptors socketpair fills
    // in. Both ends close on exec, so the commands do not hold them.
    let paired = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if paired != 0 {
        return None;
    }
    let [launcher, guardian] = fds;
    // SAFETY: the new process only runs `guard`, which makes
    // async-signal-safe calls and never returns.
    match unsafe { libc::fork() } {
        0 => unsafe { guard(guardian) },
        -1 => {
            // SAFETY: closing the descriptors just created.
            unsafe {
                libc::close(launcher);
                libc::close(guardian);
            }
            None
        }
        _ => {
            // SAFETY: closing the guardian's end, which only it uses.
            unsafe { libc::close(guardian) };
            Some(launcher)
        }
    }
}

/// Tells the guardian `what` about the group `pid`. Safe to call between
/// fork and exec.
fn tell_guardian(guardian: libc::c_int, what: Guard, pid: libc::pid_t) {
    let record: [i32; 2] = [what as i32, pid];
    // SAFETY: send reads the live record of the length given; with
    // MSG_NOSIGNAL a guardian gone raises no SIGPIPE.
    unsafe {
        libc::send(
            guardian,
            record.as_ptr().cast(),
            std::mem::size_of_val(&record),
            libc::MSG_NOSIGNAL,
        )
    };
}

/// The guardian's life, in the process forked from the launcher: it
/// collects the groups it is told about until its connection `fd` closes,
/// which happens when the launcher is gone, then ends those still running.
/// Only async-signal-safe calls, nothing allocated: the launcher has other
/// threads.
///
/// # Safety
///
/// To be called only in a process just forked, which it ends.
unsafe fn guard(fd: libc::c_int) -> ! {
    // A group of its own, so that what ends the launcher's group spares it,
    // the signal dispositions it should have, and nothing else held open:
    // the launcher's display connection and pipes are not its business.
    libc::setpgid(0, 0);
    for signal in 1..32 {
        if signal != libc::SIGKILL && signal != libc::SIGSTOP {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    let mut mask: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut mask);
    libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
    let last = libc::c_uint::MAX;
    let fd_number = fd as libc::c_uint;
    let closed = (fd_number == 0 || libc::syscall(libc::SYS_close_range, 0, fd_number - 1, 0) == 0)
        && libc::syscall(libc::SYS_close_range, fd_number + 1, last, 0) == 0;
    if !closed {
        for other in 0..1024 {
            if other != fd {
                libc::close(other);
            }
        }
    }
    libc::prctl(libc::PR_SET_NAME, c"ilia-guardian".as_ptr(), 0, 0, 0);

    fn add(groups: &mut [libc::pid_t; GUARDED_GROUPS], count: &mut usize, pid: libc::pid_t) {
        if pid > 0 && *count < GUARDED_GROUPS && !groups[..*count].contains(&pid) {
            groups[*count] = pid;
            *count += 1;
        }
    }
    let mut groups = [0 as libc::pid_t; GUARDED_GROUPS];
    let mut count = 0;
    // The process announced but not yet confirmed running.
    let mut announced: libc::pid_t = 0;
    loop {
        let mut record = [0i32; 2];
        let read = libc::recv(
            fd,
            record.as_mut_ptr().cast(),
            std::mem::size_of_val(&record),
            0,
        );
        if read < 0 && *libc::__errno_location() == libc::EINTR {
            continue;
        }
        if read <= 0 {
            // The launcher, and every process that held its end, is gone.
            break;
        }
        if read as usize != std::mem::size_of_val(&record) {
            continue;
        }
        let [what, pid] = record;
        match what {
            1 => {
                if announced != 0 {
                    add(&mut groups, &mut count, announced);
                }
                announced = pid;
            }
            2 => {
                add(&mut groups, &mut count, pid);
                if announced == pid {
                    announced = 0;
                }
            }
            3 => announced = 0,
            4 => {
                if announced == pid {
                    announced = 0;
                }
                if let Some(at) = groups[..count].iter().position(|&group| group == pid) {
                    count -= 1;
                    groups[at] = groups[count];
                }
            }
            _ => {}
        }
    }
    if announced != 0 {
        add(&mut groups, &mut count, announced);
    }
    for &group in &groups[..count] {
        libc::killpg(group, libc::SIGKILL);
    }
    libc::_exit(0)
}

#[allow(deprecated)]
fn report_channel() -> (glib::Sender<Report>, glib::Receiver<Report>) {
    glib::MainContext::channel(glib::Priority::DEFAULT)
}

/// Starts an `Exec` entry's words the way a page's `Exec` starts its
/// command: in a fresh scope named for the page `id` when the scope runner
/// is on `PATH`, else directly; never through a shell.
fn run_exec(id: &str, words: &[String]) -> io::Result<()> {
    let words = words.iter().map(OsString::from);
    let command: Vec<OsString> = match scope_command(id) {
        Some(scope) => scope.into_iter().chain(words).collect(),
        None => words.collect(),
    };
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()?;
    textlist::remember_launched(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{state_of, stop_group, watch, LineReader, MAX_LINE};
    use crate::keybindings::{find_binding, PageBinding};

    /// A pipe: its read end, and its write end as a file.
    fn pipe() -> (OwnedFd, std::fs::File) {
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a live array of the two descriptors pipe fills in.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors were just created and are owned here only.
        unsafe {
            (
                OwnedFd::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    /// Whether the process `pid` still runs (a zombie does not).
    fn alive(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| Some(stat.rsplit_once(')')?.1.split_whitespace().next()? != "Z"))
            .unwrap_or(false)
    }

    /// Starts `script` with `/bin/sh` in a process group of its own, its
    /// output piped. The caller collects it with `stop_group`, as
    /// `run_condition` does.
    #[allow(clippy::zombie_processes)]
    fn spawn(script: &str) -> (i32, OwnedFd) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let output = OwnedFd::from(child.stdout.take().unwrap());
        (child.id() as i32, output)
    }

    #[test]
    fn the_line_reader_gives_the_first_line_once_complete() {
        let (read, mut write) = pipe();
        let mut reader = LineReader::new(read);
        assert_eq!(reader.read_line(), None);
        write.write_all(b" on").unwrap();
        assert_eq!(reader.read_line(), None);
        write.write_all(b" \nsecond\n").unwrap();
        assert_eq!(reader.read_line().as_deref(), Some(&b" on "[..]));

        // Output that ends without a newline counts once it has ended.
        let (read, mut write) = pipe();
        let mut reader = LineReader::new(read);
        write.write_all(b"off").unwrap();
        drop(write);
        assert_eq!(reader.read_line(), None);
        assert!(reader.ended);
        assert_eq!(reader.finish().as_deref(), Some(&b"off"[..]));
    }

    #[test]
    fn the_line_reader_keeps_no_more_than_a_long_first_line_needs() {
        let (read, mut write) = pipe();
        let mut reader = LineReader::new(read);
        let long = vec![b'x'; MAX_LINE + 1000];
        // The pipe holds 64 KiB, more than one write here.
        write.write_all(&long).unwrap();
        let line = reader.read_line().unwrap();
        assert_eq!(line.len(), MAX_LINE);
        assert_eq!(state_of(&line), None);
        assert!(reader.buffer.len() < MAX_LINE + 4096 + 1);
    }

    #[test]
    fn a_first_line_counts_before_the_command_ends_and_the_deadline_holds() {
        let (pid, output) = spawn("echo on; exec sleep 30");
        let mut lines = Vec::new();
        let start = Instant::now();
        let status = watch(
            pid,
            Some(output),
            start + Duration::from_millis(500),
            |line| lines.push(line),
        );
        let waited = start.elapsed();
        assert_eq!(status, None, "the command ran past its deadline");
        assert!(waited >= Duration::from_millis(500), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(lines, [b"on".to_vec()]);
        // Still uncollected and running until its group is stopped.
        assert!(alive(pid));
        stop_group(pid);
        assert!(!alive(pid));
    }

    #[test]
    fn a_command_that_exits_gives_its_status_and_its_last_partial_line() {
        let (pid, output) = spawn("printf 'false'; exit 3");
        let mut lines = Vec::new();
        let status = watch(
            pid,
            Some(output),
            Instant::now() + Duration::from_secs(10),
            |line| lines.push(line),
        );
        assert_eq!(status.and_then(|status| status.code()), Some(3));
        assert_eq!(lines, [b"false".to_vec()]);
        stop_group(pid);
    }

    #[test]
    fn stopping_a_command_stops_its_whole_group() {
        let (pid, output) = spawn("sleep 30 & echo $!; wait");
        let mut lines = Vec::new();
        watch(
            pid,
            Some(output),
            Instant::now() + Duration::from_millis(300),
            |line| lines.push(line),
        );
        let left: i32 = String::from_utf8(lines.remove(0))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(left));
        stop_group(pid);
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(left) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(left), "the command's own child outlived its group");
    }

    #[test]
    fn state_lines_are_trimmed_and_folded() {
        for line in ["on", " TRUE ", "1", "On\r"] {
            assert_eq!(state_of(line.as_bytes()), Some(true), "{line:?}");
        }
        for line in ["off", "False", " 0 "] {
            assert_eq!(state_of(line.as_bytes()), Some(false), "{line:?}");
        }
        for line in ["", "perhaps", "on off", "yes"] {
            assert_eq!(state_of(line.as_bytes()), None, "{line:?}");
        }
    }

    #[test]
    fn bindings_match_by_action_or_category_in_page_order() {
        let binding = |category: &str, label: &str, exec: &str| PageBinding {
            category: category.to_owned(),
            label: label.to_owned(),
            keys: format!("{category} keys"),
            exec: exec.to_owned(),
        };
        let page = [
            binding("Alpha", "Snap Area", "exec a"),
            binding("Session", "Rest Now", ""),
            binding("Zulu", "Snap Area", "exec z"),
        ];
        let found = |name: &str| find_binding(&page, name).map(|binding| binding.exec.as_str());
        assert_eq!(found("snap area"), Some("exec a"));
        assert_eq!(found("  zUlU  //  Snap Area  "), Some("exec z"));
        assert_eq!(found("Zulu//Snap Area"), Some("exec z"));
        assert_eq!(found("Snap  Area"), None);
        // A description no binding line follows never matches.
        assert_eq!(found("Rest Now"), None);
        assert_eq!(found("Session // Rest Now"), None);
        assert_eq!(found("Capture // Snap Area"), None);
    }
}
