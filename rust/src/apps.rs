use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk::atk::prelude::AtkObjectExt;
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::pango;
use gtk::prelude::*;

use crate::add_app_form::AddAppForm;
use crate::clipboard_keeper;
use crate::entries::{self, Delivery, DesktopAction, DesktopEntry};
use crate::filter::{contains_all_words, fold_text, query_words, summed_span, words_in_fields};
use crate::keybindings::navigate;
use crate::launch::{bus_name, launch_desktop_action, launch_desktop_app};
use crate::new_workspace;
use crate::page_files::{PageFile, Rows};
use crate::pages::{
    is_enter_key, rebuild_keeping_selection, select_first_row, select_row_near, DialogPage,
    FUZZY_MATCHING,
};
use crate::program_path;

const ICON: u32 = 0;
const NAME: u32 = 1;
const APP_INFO: u32 = 2;
// The index of the row's app in the page's `AppList`.
const INDEX: u32 = 3;
// On the Applications page: the star of a favorite, empty for other apps
// and for actions' rows.
const FAVORITE: u32 = 4;
/// The mark of a favorite's row, in a cell of its own after the name.
const STAR: &str = "\u{2605}";
// On the Applications page: for an action's row, the index of the action
// among its app's; -1 for an app's row.
const ACTION: u32 = 5;
// On the Applications page: for an action's row, the length in bytes of
// the app name and the ": " that begin its text; 0 for an app's row.
const PREFIX: u32 = 6;
/// How much further right than an app's name the text of an action's row
/// begins, in pixels.
const ACTION_INDENT: i32 = 16;
/// The cue at the end of the row of an app with actions.
const ACTION_CUE: &str = "\u{203a}";
const CUE_WIDTH: i32 = 24;
const HISTORY_MAX_LEN: usize = 32;
/// The launch history the pages share: desktop ids, and the choices on page
/// files ordered by frequency.
pub const LAUNCH_HISTORY: &str = "app-launch-counts";
const HIDDEN_APPS: &str = "hidden-apps";
const FAVORITE_APPS: &str = "favorite-apps";
const MATCH_FIELDS: &str = "match-fields";
// How often the path strip is worked out again while the page is shown, so
// that it follows changes to the files it names.
const PROGRAM_PATH_RECHECK: Duration = Duration::from_millis(500);
// Icons decoded at the same time on GTK's worker threads.
const ICON_LOAD_PARALLELISM: usize = 4;
/// From the start on, entries whose read is stuck no longer hold up the
/// list: it comes without them, and they join it once they have been read.
const LIST_DEADLINE: Duration = Duration::from_secs(10);

/// A desktop entry read on a worker thread, on its way to the main thread.
struct ReadEntry(DesktopEntry);

// SAFETY: a reader thread creates the entry's objects and moves the only
// references to them to the main thread without using them again, so no
// object is ever used from two threads at once. GObject reference counting
// is atomic, and a GDesktopAppInfo is not tied to the thread or main context
// it was created in.
unsafe impl Send for ReadEntry {}

/// What the desktop-entry watchers are told.
pub enum EntriesUpdate<'a> {
    /// The list has arrived: every entry read so far.
    Listed(&'a [Rc<DesktopEntry>]),
    /// An entry read after the list arrived. `all` includes it.
    Added {
        entry: &'a Rc<DesktopEntry>,
        all: &'a [Rc<DesktopEntry>],
    },
}

type Watcher = Rc<dyn Fn(EntriesUpdate<'_>)>;

/// The entries read so far, kept on the main thread, and who wants them.
#[derive(Default)]
struct Prefetch {
    started: bool,
    listed: bool,
    entries: Vec<Rc<DesktopEntry>>,
    watchers: Vec<Watcher>,
}

thread_local! {
    static PREFETCH: RefCell<Prefetch> = RefCell::default();
}

/// Starts reading the desktop files on worker threads, once, so that the
/// parse overlaps with GTK's own start-up (theme CSS, fontconfig). Call it
/// on the main thread, early: the list deadline counts from here. GLib
/// translates the entries' names and keywords as it reads them, so the
/// locale must be set first.
pub fn prefetch_apps() {
    if PREFETCH.with(|prefetch| std::mem::replace(&mut prefetch.borrow_mut().started, true)) {
        return;
    }
    let deadline = Instant::now() + LIST_DEADLINE;
    // Create GIO's default VFS here, before the workers exist. Otherwise a
    // worker's first GFile (a desktop entry whose Icon= is an absolute path)
    // may create it while GTK's start-up holds GIO's module lock to choose
    // the settings backend. The keyfile backend needs the VFS too, and the
    // VFS needs that lock, so each thread waits for the other and the window
    // never shows (seen on Wayland, where GDK reads its settings first).
    let _ = gio::Vfs::default();
    let spawned = std::thread::Builder::new()
        .name("ilia-apps".to_owned())
        .spawn(move || {
            let files = entries::entry_files(&entries::application_dirs());
            let build = |file: entries::ReadFile| Some(ReadEntry(file.into_entry()));
            entries::read_all(files, entries::read_entry, build, deadline, |delivery| {
                let mut delivery = Some(delivery);
                glib::idle_add_full(glib::Priority::HIGH, move || {
                    if let Some(delivery) = delivery.take() {
                        deliver(delivery);
                    }
                    glib::ControlFlow::Break
                });
            });
        });
    if let Err(error) = spawned {
        eprintln!("Cannot read the desktop files in the background: {error}");
        let files = entries::entry_files(&entries::application_dirs());
        let list = files.into_iter().filter_map(entries::load_entry);
        deliver(Delivery::List(list.map(ReadEntry).collect()));
    }
}

/// Whether the desktop entries are being read, or have been.
pub fn entries_requested() -> bool {
    PREFETCH.with(|prefetch| prefetch.borrow().started)
}

fn deliver(delivery: Delivery<ReadEntry>) {
    // `Some(None)`: the list. `Some(Some(entry))`: an entry read after it.
    // `None`: nothing to tell yet.
    let news = PREFETCH.with(|prefetch| {
        let mut prefetch = prefetch.borrow_mut();
        match delivery {
            Delivery::List(list) => {
                let list = list.into_iter().map(|ReadEntry(entry)| Rc::new(entry));
                prefetch.entries.extend(list);
                prefetch.listed = true;
                Some(None)
            }
            Delivery::Late(ReadEntry(entry)) => {
                let entry = Rc::new(entry);
                prefetch.entries.push(entry.clone());
                // Before the list it simply becomes part of it.
                prefetch.listed.then_some(Some(entry))
            }
        }
    });
    let Some(added) = news else {
        return;
    };
    let (entries, watchers) = PREFETCH.with(|prefetch| {
        let prefetch = prefetch.borrow();
        (prefetch.entries.clone(), prefetch.watchers.clone())
    });
    for watcher in &watchers {
        watcher(match &added {
            None => EntriesUpdate::Listed(&entries),
            Some(entry) => EntriesUpdate::Added {
                entry,
                all: &entries,
            },
        });
    }
}

/// Calls `watcher` with the desktop entries once they have been listed, at
/// once if they have, and again for each entry read after that.
pub fn watch_entries(watcher: impl Fn(EntriesUpdate<'_>) + 'static) {
    prefetch_apps();
    let watcher: Watcher = Rc::new(watcher);
    let listed = PREFETCH.with(|prefetch| {
        let mut prefetch = prefetch.borrow_mut();
        prefetch.watchers.push(watcher.clone());
        prefetch.listed.then(|| prefetch.entries.clone())
    });
    if let Some(entries) = listed {
        watcher(EntriesUpdate::Listed(&entries));
    }
}

pub struct ApplicationsPage {
    entry: gtk::Entry,
    // A row for each app, at the top level, with the rows of its actions
    // that are not hidden beneath it, and a row of its own for each of its
    // actions, at the top level too. The filter decides which are listed.
    model: gtk::TreeStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    // The column of the action cue, at the end of an app's row.
    cue_column: gtk::TreeViewColumn,
    root: gtk::Widget,
    // At the left of the strip below the list, the program of the
    // highlighted app.
    program_path: gtk::Label,
    // The program path's complete text, as its accessible name gives it.
    program_text: RefCell<String>,
    // At the right of the strip, the + button, and the form it opens over
    // the list.
    add_button: gtk::EventBox,
    form: Rc<AddAppForm>,
    // What had the keyboard focus before the form was opened.
    focus_before_form: RefCell<Option<gtk::Widget>>,
    icon_size: i32,
    settings: gio::Settings,
    launch_history: Rc<RefCell<Vec<String>>>,
    // Launches per desktop id, from the history as it was at start.
    launch_counts: HashMap<String, usize>,
    apps: Rc<AppList>,
    // Set once the desktop entries are in the model.
    apps_loaded: Cell<bool>,
    // The search text of the first Enter or Ctrl+Enter pressed before the
    // list arrived, and where to launch: its best match is launched once the
    // list has.
    pending_enter: RefCell<Option<(String, Workspace)>>,
    // Decodes the icons of rows added after the list arrived.
    icon_loader: RefCell<Option<Rc<IconLoader>>>,
    // The desktop ids this run's Ctrl+Delete hides appended to
    // `hidden-apps`, latest last, less those Ctrl+z has reversed.
    hides: RefCell<Vec<String>>,
    quit: Rc<dyn Fn()>,
    // The window manager Ctrl+Enter asks for a new workspace.
    wm_name: String,
    // An app was started: ilia is closing. A click can activate a row twice.
    launched: Cell<bool>,
    // The model row of each app, by its index.
    app_rows: RefCell<Vec<gtk::TreeIter>>,
    // Set while the page moves the highlight itself, which never closes
    // the app whose actions are shown.
    moving: Cell<bool>,
    // A primary button was pressed on an action cue: its release starts
    // nothing.
    cue_pressed: Cell<bool>,
    // The accessible name cell last announced as expanded for an app's row
    // whose actions are shown but all narrowed away.
    empty_group_cell: RefCell<Option<gtk::atk::Object>>,
}

/// Where an app is launched: on the workspace shown (Enter), or on the next
/// free one (Ctrl+Enter).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Workspace {
    Current,
    NextFree,
}

impl ApplicationsPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        wm_name: &str,
        quit: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let launch_history: Vec<String> = settings
            .strv(LAUNCH_HISTORY)
            .iter()
            .map(ToString::to_string)
            .collect();
        let mut launch_counts = HashMap::new();
        for id in &launch_history {
            *launch_counts.entry(id.clone()).or_insert(0) += 1;
        }
        let icon_size = settings.int("icon-size");

        let model = gtk::TreeStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
            u32::static_type(),
            String::static_type(),
            i32::static_type(),
            u32::static_type(),
        ]);
        // The rows come once the desktop entries have been read (see
        // `finish_loading`); the window is built and shown in the meantime.
        let apps = Rc::new(AppList::new(Vec::new(), AppSettings::read(&settings)));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let apps_for_filter = apps.clone();
        filter.set_visible_func(move |tree_model, iter| {
            is_row_listed(&apps_for_filter, tree_model, iter)
        });

        install_sort(&model, &apps);
        model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);

        let (view, scrolled, cue_column) = applications_view(&filter, icon_size);

        // The form covers the list while it is shown; the list stays as it
        // is underneath.
        let form = AddAppForm::new();
        let overlay = gtk::Overlay::new();
        overlay.add(&scrolled);
        overlay.add_overlay(form.widget());

        // One line below the list: the program of the highlighted app from
        // its left edge, the + button at its right end.
        let program_path = program_path_label();
        let add_button = add_app_button();
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        strip.style_context().add_class("app_strip");
        strip.pack_start(&program_path, true, true, 0);
        strip.pack_end(&add_button, false, false, 0);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.pack_start(&overlay, true, true, 0);
        root.pack_start(&strip, false, false, 0);

        let page = Rc::new(Self {
            entry,
            model,
            filter,
            view,
            cue_column,
            root: root.upcast(),
            program_path,
            program_text: RefCell::default(),
            add_button,
            form,
            focus_before_form: RefCell::default(),
            icon_size,
            settings,
            launch_history: Rc::new(RefCell::new(launch_history)),
            launch_counts,
            apps,
            apps_loaded: Cell::new(false),
            pending_enter: RefCell::default(),
            icon_loader: RefCell::default(),
            hides: RefCell::default(),
            quit,
            wm_name: wm_name.to_owned(),
            launched: Cell::new(false),
            app_rows: RefCell::default(),
            moving: Cell::new(false),
            cue_pressed: Cell::new(false),
            empty_group_cell: RefCell::default(),
        });
        open_group_state::follow(&page);
        page.connect_activation();
        page.connect_form();
        page.connect_program_path();
        page.follow_settings();
        page.set_selection();

        // The window is built and shown while the desktop files are still
        // being read.
        let weak = Rc::downgrade(&page);
        watch_entries(move |update| {
            let Some(page) = weak.upgrade() else {
                return;
            };
            match update {
                EntriesUpdate::Listed(entries) => page.finish_loading(entries),
                EntriesUpdate::Added { entry, .. } => page.add_late_entry(entry),
            }
        });
        page
    }

    /// Lists the desktop entries read in time for the list: a row for each
    /// app to show, hidden ones included, which the filter leaves out (one
    /// that stops being hidden while the launcher runs comes back as it
    /// was). The rows are the objects GLib read: creating new ones from
    /// their ids would parse each desktop file a second time.
    fn finish_loading(self: &Rc<Self>, entries: &[Rc<DesktopEntry>]) {
        let blank = blank_icon(self.icon_size);
        // An app added with the form before the list arrived is listed
        // already.
        let mut listed: HashSet<String> = (0..self.apps.len())
            .map(|index| self.apps.id(index))
            .collect();
        for entry in entries {
            let Some(desktop) = entry.app.as_ref() else {
                continue;
            };
            if let Some(id) = shown_id(desktop).filter(|id| listed.insert(id.clone())) {
                self.add_row(id, desktop, &entry.actions, blank.as_ref());
            }
        }
        self.set_selection();
        self.apps_loaded.set(true);
        // That Enter acts once, on this list, for the text typed before it,
        // whatever the search box holds now. When nothing matches that text
        // nothing is started and the window stays open.
        if let Some((text, workspace)) = self.pending_enter.take() {
            if let Some(row) = self.apps.best_row(&text) {
                self.execute_row(row, workspace);
            }
        }

        // Icons once the list is on screen. The model must only be accessed
        // from the GTK main thread: walking it from another thread races with
        // the re-sort done on every keystroke.
        if self.icon_size > 0 {
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(page) = weak.upgrade() {
                    let loader = IconLoader::start(&page.model, &page.filter, page.icon_size);
                    *page.icon_loader.borrow_mut() = loader;
                }
            });
        }
    }

    /// Adds an entry that could only be read after the list arrived. The
    /// sorted model puts it in its place; the highlighted row, the search
    /// text and the filter stay as they are, and an Enter pressed before the
    /// list arrived has already been handled.
    fn add_late_entry(&self, entry: &DesktopEntry) {
        let Some(desktop) = entry.app.as_ref() else {
            return;
        };
        let Some(id) = shown_id(desktop).filter(|id| self.apps.index_of(id).is_none()) else {
            return;
        };
        let row = self.add_row(
            id,
            desktop,
            &entry.actions,
            blank_icon(self.icon_size).as_ref(),
        );
        // Before the loader starts, it finds the row in the model itself.
        if let Some(loader) = self.icon_loader.borrow().as_ref() {
            loader.enqueue(row);
        }
    }

    /// Adds the app `desktop`, whose desktop id is `id` and whose entry
    /// declares the actions `actions`, to the list, and rows for it and its
    /// actions to the model. Returns the app's row.
    fn add_row(
        &self,
        id: String,
        desktop: &gio::DesktopAppInfo,
        actions: &[DesktopAction],
        blank: Option<&Pixbuf>,
    ) -> gtk::TreeIter {
        let launches = |key: &str| self.launch_counts.get(key).copied().unwrap_or_default();
        let name = entries::app_name(desktop);
        let app = App::new(id.clone(), desktop, launches(&id)).with_actions(
            &name,
            actions,
            bus_name(desktop).is_some(),
            launches,
        );
        let action_names: Vec<String> = app
            .actions
            .iter()
            .map(|action| action.action.name.clone())
            .collect();
        let index = self.apps.push(app);
        let row = insert_row(&self.model, desktop, index, blank);
        set_star(&self.model, &row, self.apps.is_favorite(index));
        self.app_rows.borrow_mut().push(row);
        for (action, action_name) in action_names.iter().enumerate() {
            insert_action_row(&self.model, None, desktop, index, action, action_name);
        }
        self.sync_actions_beneath(index);
        row
    }

    /// Makes the rows beneath the row of the app with the index `index`
    /// those of its actions that are not hidden, in their order.
    fn sync_actions_beneath(&self, index: usize) {
        let Some(row) = self.app_rows.borrow().get(index).cloned() else {
            return;
        };
        let Some(desktop) = app_value(&self.model, &row) else {
            return;
        };
        let wanted = self.apps.shown_actions(index);
        let mut present = Vec::new();
        let mut changed = false;
        if let Some(child) = self.model.iter_children(Some(&row)) {
            loop {
                match action_value(&self.model, &child).filter(|action| wanted.contains(action)) {
                    Some(action) => {
                        present.push(action);
                        if !self.model.iter_next(&child) {
                            break;
                        }
                    }
                    None => {
                        changed = true;
                        // Moves on to the next row, if there is one.
                        if !self.model.remove(&child) {
                            break;
                        }
                    }
                }
            }
        }
        for &action in wanted.iter().filter(|action| !present.contains(action)) {
            if let Some(found) = self.apps.action(index, action) {
                insert_action_row(
                    &self.model,
                    Some(&row),
                    &desktop,
                    index,
                    action,
                    &found.name,
                );
                changed = true;
            }
        }
        if !changed {
            return;
        }
        // Without rows beneath, the app's actions are no longer shown.
        if wanted.is_empty() && self.apps.open.get() == Some(index) {
            self.apps.set_open(None);
        }
        // The cue depends on the rows beneath.
        if let Some(path) = self.model.path(&row) {
            self.model.row_changed(&path, &row);
        }
    }

    fn sync_all_actions_beneath(&self) {
        for index in 0..self.apps.len() {
            self.sync_actions_beneath(index);
        }
    }

    /// The app of the row whose INDEX is `index`.
    fn app_at(&self, index: usize) -> Option<gio::DesktopAppInfo> {
        let row = self.app_rows.borrow().get(index).cloned()?;
        app_value(&self.model, &row)
    }

    /// Whether Enter has to wait for the list: it has not arrived, and no
    /// row is highlighted. Before the list only an app added with the form
    /// can have a row.
    fn waits_for_list(&self) -> bool {
        !self.apps_loaded.get() && self.view.selection().selected().is_none()
    }

    /// Enter or Ctrl+Enter before the list arrived: keeps the search text
    /// of the first such key, and where it launches, for when the list
    /// arrives.
    fn defer_activation(&self, workspace: Workspace) {
        let mut pending = self.pending_enter.borrow_mut();
        if pending.is_none() {
            *pending = Some((self.entry.text().to_string(), workspace));
        }
    }

    fn connect_activation(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.view.connect_row_activated(move |_, path, _| {
            if let Some(page) = weak.upgrade() {
                page.execute_path(path);
            }
        });

        // A primary press on an action cue opens or closes the app's
        // actions, and the press and its release start nothing.
        let weak = Rc::downgrade(self);
        self.view.connect_button_press_event(move |view, event| {
            let Some(page) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if event.button() != gdk::BUTTON_PRIMARY || page.form.is_open() {
                return glib::Propagation::Proceed;
            }
            let Some((path, column)) = cell_at_pointer(view, event) else {
                return glib::Propagation::Proceed;
            };
            if column.as_ref() != Some(&page.cue_column) || !page.has_cue(&path) {
                return glib::Propagation::Proceed;
            }
            // The second and third presses of a double or triple click
            // come after a press of their own.
            if event.event_type() == gdk::EventType::ButtonPress {
                page.cue_pressed.set(true);
                page.toggle_actions(&path);
            }
            glib::Propagation::Stop
        });

        // Under the X11 grab the tree view does not activate a row on a
        // single click itself: launch the row a primary click is released
        // on. Where it does (Wayland), `launched` keeps it to one launch.
        let weak = Rc::downgrade(self);
        self.view.connect_button_release_event(move |view, event| {
            let Some(page) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if event.button() == gdk::BUTTON_PRIMARY && page.cue_pressed.replace(false) {
                return glib::Propagation::Stop;
            }
            if event.button() != gdk::BUTTON_PRIMARY
                || event
                    .state()
                    .intersects(gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK)
                || page.form.is_open()
            {
                return glib::Propagation::Proceed;
            }
            if let Some((path, _)) = cell_at_pointer(view, event) {
                page.execute_path(&path);
            }
            glib::Propagation::Proceed
        });

        // A move of the highlight that ends outside the app whose actions
        // are shown closes them.
        let weak = Rc::downgrade(self);
        self.view.connect_cursor_changed(move |view| {
            let Some(page) = weak.upgrade() else {
                return;
            };
            if page.moving.get() {
                return;
            }
            let Some(open) = page.apps.open.get() else {
                return;
            };
            let Some(path) = view.cursor().0 else {
                return;
            };
            // Only the open app has rows beneath it on show.
            let inside = path.depth() > 1
                || page
                    .filter
                    .iter(&path)
                    .is_some_and(|iter| row_value(&page.filter, &iter) == (open, None));
            if !inside {
                page.close_actions();
            }
        });
    }

    fn connect_form(self: &Rc<Self>) {
        // A primary click: the press and the release both on the button.
        let pressed = Rc::new(Cell::new(false));
        let pressed_for_press = pressed.clone();
        self.add_button.connect_button_press_event(move |_, event| {
            if event.button() == gdk::BUTTON_PRIMARY
                && event.event_type() == gdk::EventType::ButtonPress
            {
                pressed_for_press.set(true);
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(self);
        self.add_button
            .connect_button_release_event(move |button, event| {
                let (x, y) = event.position();
                let inside = x >= 0.0
                    && y >= 0.0
                    && x < f64::from(button.allocated_width())
                    && y < f64::from(button.allocated_height());
                if pressed.replace(false) && event.button() == gdk::BUTTON_PRIMARY && inside {
                    if let Some(page) = weak.upgrade() {
                        page.open_form();
                    }
                }
                glib::Propagation::Stop
            });
        let weak = Rc::downgrade(self);
        self.form.connect_cancelled(move || {
            if let Some(page) = weak.upgrade() {
                page.close_form();
            }
        });
        let weak = Rc::downgrade(self);
        self.form.connect_added(move |path| {
            if let Some(page) = weak.upgrade() {
                page.on_app_added(path);
            }
        });
    }

    /// Keeps the path strip naming the program of the highlighted row: when
    /// the highlight moves, when the page is shown again, and periodically
    /// while it is shown, for changes to the files the program is found by.
    fn connect_program_path(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.view.selection().connect_changed(move |_| {
            if let Some(page) = weak.upgrade() {
                page.update_program_path();
            }
        });
        let weak = Rc::downgrade(self);
        self.root.connect_map(move |_| {
            if let Some(page) = weak.upgrade() {
                page.update_program_path();
            }
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(PROGRAM_PATH_RECHECK, move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if page.root.is_mapped() {
                page.update_program_path();
            }
            glib::ControlFlow::Continue
        });
    }

    /// Shows the program of the highlighted row, or nothing when no row is
    /// highlighted.
    fn update_program_path(&self) {
        let text = self
            .view
            .selection()
            .selected()
            .and_then(|(model, iter)| {
                let app = app_value(&model, &iter)?;
                let action = row_value(&model, &iter)
                    .1
                    .and_then(|action| self.apps.action(index_value(&model, &iter), action));
                Some(match action {
                    Some(action) => program_path::describe_action(
                        &app,
                        action.exec.as_deref().unwrap_or_default(),
                    ),
                    None => program_path::describe_app(&app),
                })
            })
            .unwrap_or_default();
        // Only a different text is news, to assistive technology as well.
        if *self.program_text.borrow() == text {
            return;
        }
        self.program_path
            .set_text(&program_path::single_line_display(&text));
        if let Some(accessible) = self.program_path.accessible() {
            accessible.set_name(&text);
        }
        self.program_text.replace(text);
    }

    /// Ctrl+Shift+C copies the program path shown, without the target of a
    /// link. Nothing is copied when no program was found.
    fn copy_program_path(&self) {
        let text = self.program_text.borrow();
        if let Some(path) = program_path::copied_path(&text) {
            clipboard_keeper::copy(&self.program_path.display(), path);
        }
    }

    /// Shows the add-application form over the list.
    fn open_form(&self) {
        if self.form.is_open() {
            return;
        }
        let focus = self
            .root
            .toplevel()
            .and_downcast::<gtk::Window>()
            .and_then(|window| window.focused_widget());
        *self.focus_before_form.borrow_mut() = focus;
        // The filter belongs to the list: it keeps its text, and the form's
        // keys stay out of it.
        self.entry.set_sensitive(false);
        self.add_button.set_sensitive(false);
        self.form.open();
    }

    /// Shows the list again, as it was before the form was opened.
    fn close_form(&self) {
        self.form.close();
        self.add_button.set_sensitive(true);
        self.entry.set_sensitive(true);
        match self.focus_before_form.take() {
            Some(focus) => match focus.downcast::<gtk::Entry>() {
                Ok(entry) => entry.grab_focus_without_selecting(),
                Err(focus) => focus.grab_focus(),
            },
            None => self.view.grab_focus(),
        }
    }

    /// A desktop entry was written at `path`: clear the filter, then list
    /// the entry and highlight it.
    fn on_app_added(&self, path: PathBuf) {
        self.entry.set_text("");
        self.close_form();
        let Some((desktop, id)) = gio::DesktopAppInfo::from_filename(&path)
            .filter(|desktop| desktop.should_show())
            .and_then(|desktop| {
                let id = desktop.id()?.to_string();
                Some((desktop, id))
            })
        else {
            eprintln!("The new desktop entry {} cannot be listed", path.display());
            return;
        };
        let actions = entries::desktop_actions_of_file(&path);
        let row = self.add_row(id, &desktop, &actions, blank_icon(self.icon_size).as_ref());
        if let Some(icon) = gtk::IconTheme::default()
            .filter(|_| self.icon_size > 0)
            .and_then(|theme| load_icon(&theme, &desktop, self.icon_size))
        {
            self.model.set(&row, &[(ICON, &icon)]);
        }
        self.highlight(&row);
    }

    /// Highlights a model row, if the filter lists it.
    fn highlight(&self, row: &gtk::TreeIter) {
        let Some(path) = self
            .filter
            .convert_child_iter_to_iter(row)
            .and_then(|iter| self.filter.path(&iter))
        else {
            return;
        };
        // Also selects the row and scrolls to it.
        self.view
            .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
    }

    fn execute_path(&self, path: &gtk::TreePath) {
        let Some(iter) = self.filter.iter(path) else {
            return;
        };
        self.execute_row(row_value(&self.filter, &iter), Workspace::Current);
    }

    /// Starts the app or the action of the row `row` on `workspace`. For the
    /// next free workspace the window manager switches to it first, and the
    /// app or action is started as Enter starts it once the switch is
    /// confirmed. When the switch fails nothing is started and the window
    /// stays open.
    fn execute_row(&self, (index, action): Row, workspace: Workspace) {
        let Some(app) = self.app_at(index) else {
            return;
        };
        if workspace == Workspace::NextFree {
            if self.launched.get() {
                return;
            }
            if let Err(error) = new_workspace::switch_to_next_free(&self.wm_name) {
                eprintln!("Failed to open a new workspace: {error}");
                return;
            }
        }
        let action = action.and_then(|action| self.apps.action(index, action));
        execute_entry(
            &app,
            action.as_ref(),
            &self.settings,
            &self.launch_history,
            &self.launched,
            &self.quit,
        );
    }

    /// Highlights the first row: the best match for the text typed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        select_first_row(&self.view);
    }

    /// Launches the highlighted app on `workspace`; with no row highlighted
    /// nothing happens.
    fn execute_selected(&self, workspace: Workspace) {
        let Some((model, iter)) = self.view.selection().selected() else {
            return;
        };
        self.execute_row(row_value(&model, &iter), workspace);
    }

    /// Whether the row at `path` is an app's row that shows the action cue.
    fn has_cue(&self, path: &gtk::TreePath) -> bool {
        path.depth() == 1
            && self
                .filter
                .iter(path)
                .is_some_and(|iter| self.has_actions(&iter))
    }

    /// Whether the listed row at `iter` is the row of an app with actions
    /// beneath it in the model, whether or not the narrowing text lists
    /// any of them.
    fn has_actions(&self, iter: &gtk::TreeIter) -> bool {
        has_actions_beneath(self.filter.upcast_ref(), iter)
    }

    /// Highlights `path` without closing the app whose actions are shown.
    fn move_cursor_quietly(&self, path: &gtk::TreePath) {
        self.moving.set(true);
        self.view
            .set_cursor(path, None::<&gtk::TreeViewColumn>, false);
        self.moving.set(false);
    }

    /// Highlights the app row at `path` and shows its actions beneath it,
    /// or closes them when they are shown.
    fn toggle_actions(&self, path: &gtk::TreePath) {
        let Some(iter) = self.filter.iter(path) else {
            return;
        };
        let index = index_value(&self.filter, &iter);
        self.move_cursor_quietly(path);
        if self.apps.open.get() == Some(index) {
            self.close_actions();
        } else {
            self.open_actions(index);
        }
    }

    /// Shows the actions of the app with the index `index` beneath its row,
    /// which stays highlighted, after closing those of any other app. The
    /// rows of its actions on their own go meanwhile.
    fn open_actions(&self, index: usize) {
        self.close_actions();
        self.apps.set_open(Some(index));
        self.moving.set(true);
        self.filter.refilter();
        if let Some(path) = self.listed_path(index) {
            self.view.expand_row(&path, false);
            self.view
                .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
        }
        self.moving.set(false);
    }

    /// Closes the actions shown beneath an app's row, if any. The row
    /// highlighted stays highlighted, the app's row when it was one of its
    /// actions; the rows of the app's actions on their own come back.
    fn close_actions(&self) {
        let Some(index) = self.apps.open.get() else {
            return;
        };
        self.apps.set_open(None);
        let app_path = self.listed_path(index);
        let beneath = self
            .view
            .selection()
            .selected()
            .is_some_and(|(model, iter)| model.iter_parent(&iter).is_some());
        if let Some(path) = app_path.as_ref().filter(|_| beneath) {
            self.move_cursor_quietly(path);
        }
        self.moving.set(true);
        rebuild_keeping_selection(&self.view, &self.filter, || {
            if let Some(path) = app_path.as_ref() {
                self.view.collapse_row(path);
            }
            self.filter.refilter();
        });
        self.moving.set(false);
        self.announce_empty_group();
    }

    /// Right and Left on the highlighted row, while the page is shown:
    /// whether the page took the key. Right shows an app's actions, and
    /// once they are shown highlights the first; Left closes them, from
    /// the app's row or from one of its actions. On any other row the keys
    /// are left to the page tabs.
    fn arrow_key(&self, event: &gdk::EventKey) -> bool {
        let right = match event.keyval() {
            gdk::keys::constants::Right => true,
            gdk::keys::constants::Left => false,
            _ => return false,
        };
        if event.state().intersects(
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::MOD1_MASK,
        ) || !self.root.is_mapped()
            || self.form.is_open()
        {
            return false;
        }
        if right {
            return self.show_actions();
        }
        let Some((model, iter)) = self.view.selection().selected() else {
            return false;
        };
        let Some(path) = model.path(&iter) else {
            return false;
        };
        let (index, action) = row_value(&model, &iter);
        let open = self.apps.open.get() == Some(index);
        if (action.is_none() && open) || (action.is_some() && path.depth() > 1) {
            self.close_actions();
            return true;
        }
        false
    }

    /// Right on the highlighted row: shows the actions of an app whose row
    /// has the cue, and once they are shown highlights the first one left.
    /// Whether the row is one Right acts on: an app's with the cue, or an
    /// action's beneath its app, which has nothing more to show.
    fn show_actions(&self) -> bool {
        let Some((model, iter)) = self.view.selection().selected() else {
            return false;
        };
        let Some(path) = model.path(&iter) else {
            return false;
        };
        let (index, action) = row_value(&model, &iter);
        match action {
            Some(_) => path.depth() > 1,
            None if self.apps.open.get() == Some(index) => {
                let mut first = path.clone();
                first.down();
                if self.filter.iter(&first).is_some() {
                    self.view
                        .set_cursor(&first, None::<&gtk::TreeViewColumn>, false);
                }
                true
            }
            None if self.has_actions(&iter) => {
                self.open_actions(index);
                true
            }
            None => false,
        }
    }

    /// Alt+Enter: on the highlighted row of an app with the cue, what Right
    /// does; on any other row, and with no row, nothing at all.
    fn alt_enter(&self) {
        if self.form.is_open() {
            return;
        }
        let on_app = self
            .view
            .selection()
            .selected()
            .is_some_and(|(model, iter)| action_value(&model, &iter).is_none());
        if on_app {
            self.show_actions();
        }
    }

    /// A key typed while an app's actions are shown: a printable character
    /// extends the narrowing text, and Backspace takes back its last
    /// character while it has one. Whether the key was taken; otherwise it
    /// edits the filter text as usual.
    fn narrowing_key(&self, event: &gdk::EventKey) -> bool {
        if self.apps.open.get().is_none()
            || !self.root.is_mapped()
            || self.form.is_open()
            || event
                .state()
                .intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::MOD1_MASK)
        {
            return false;
        }
        let mut text = self.apps.narrowing.borrow().clone();
        if event.keyval() == gdk::keys::constants::BackSpace {
            if text.pop().is_none() {
                return false;
            }
        } else {
            match event.keyval().to_unicode().filter(|c| !c.is_control()) {
                Some(character) => text.push(character),
                None => return false,
            }
        }
        self.narrow(text);
        true
    }

    /// Makes `text` the narrowing text of the app whose actions are shown:
    /// the rows beneath its row become those of the actions whose names
    /// hold every word of it, in their order. The highlighted row stays
    /// highlighted while it is listed; otherwise the first action left is
    /// highlighted, or else the app's row.
    fn narrow(&self, text: String) {
        let Some(index) = self.apps.open.get() else {
            return;
        };
        let selected = self
            .view
            .selection()
            .selected()
            .map(|(model, iter)| row_value(&model, &iter));
        self.moving.set(true);
        *self.apps.narrowing.borrow_mut() = text;
        self.filter.refilter();
        if let Some(path) = self.listed_path(index) {
            let listed = self
                .filter
                .iter(&path)
                .is_some_and(|iter| self.filter.iter_has_child(&iter));
            // GtkTreeView collapses a row whose last row beneath goes.
            if listed && !self.view.row_expanded(&path) {
                self.view.expand_row(&path, false);
            }
            let mut first = path.clone();
            first.down();
            let target = selected
                .and_then(|row| self.path_in_group(&path, row))
                .or_else(|| listed.then_some(first))
                .unwrap_or(path);
            self.view
                .set_cursor(&target, None::<&gtk::TreeViewColumn>, false);
        }
        self.moving.set(false);
        self.announce_empty_group();
    }

    /// The path of the row listing `row` in the group whose app's row is at
    /// `app_path`: that row itself, or one of the rows beneath it.
    fn path_in_group(&self, app_path: &gtk::TreePath, row: Row) -> Option<gtk::TreePath> {
        let parent = self.filter.iter(app_path)?;
        if row.1.is_none() {
            return (row_value(&self.filter, &parent) == row).then(|| app_path.clone());
        }
        (0..self.filter.iter_n_children(Some(&parent))).find_map(|position| {
            let child = self.filter.iter_nth_child(Some(&parent), position)?;
            (row_value(&self.filter, &child) == row).then(|| {
                let mut path = app_path.clone();
                path.append_index(position);
                path
            })
        })
    }

    /// The index among the list's accessible children of the name cell of
    /// the app's row whose actions are shown with none left listed by the
    /// narrowing text, if there is one.
    fn empty_group_cell_index(&self) -> Option<i32> {
        let index = self.apps.open.get()?;
        let path = self.listed_path(index)?;
        let iter = self.filter.iter(&path)?;
        if self.filter.iter_has_child(&iter) || !self.has_actions(&iter) {
            return None;
        }
        // Only the open app's row can have rows beneath it, and it has
        // none: the accessible row is its position. The list's first
        // accessible children are its column headers, one per column.
        let row = *path.indices().first()?;
        let columns = self.view.columns();
        let name = self.view.expander_column()?;
        let column = columns.iter().position(|column| *column == name)?;
        Some((row + 1) * columns.len() as i32 + column as i32)
    }

    /// Tells assistive technologies about the app's row whose actions are
    /// shown but all narrowed away: GtkTreeView reports it as neither
    /// expandable nor expanded once it has no rows beneath, while it is
    /// both (see [`open_group_state`]). A row that stops being one is
    /// reported as it is.
    fn announce_empty_group(&self) {
        let now = self
            .empty_group_cell_index()
            .and_then(|index| open_group_state::cell(&self.view, index));
        let before = self.empty_group_cell.replace(now.clone());
        if before == now {
            return;
        }
        if let Some(before) = before {
            open_group_state::announce_actual(&before);
        }
        if let Some(now) = now {
            open_group_state::announce_open(&now);
        }
    }

    /// The row below `path` as the list shows it: an open app's first
    /// action, the next row, or the row after an open app's last action.
    fn row_below(&self, path: &gtk::TreePath) -> Option<gtk::TreePath> {
        if path.depth() == 1 && self.view.row_expanded(path) {
            let mut first = path.clone();
            first.down();
            if self.filter.iter(&first).is_some() {
                return Some(first);
            }
        }
        let mut next = path.clone();
        next.next();
        if self.filter.iter(&next).is_some() {
            return Some(next);
        }
        let mut after = path.clone();
        (after.up() && after.depth() > 0)
            .then(|| {
                after.next();
                after
            })
            .filter(|after| self.filter.iter(after).is_some())
    }

    /// The row above `path` as the list shows it.
    fn row_above(&self, path: &gtk::TreePath) -> Option<gtk::TreePath> {
        let mut previous = path.clone();
        if previous.prev() {
            if previous.depth() == 1 && self.view.row_expanded(&previous) {
                let count = self
                    .filter
                    .iter(&previous)
                    .map_or(0, |iter| self.filter.iter_n_children(Some(&iter)));
                if count > 0 {
                    previous.append_index(count - 1);
                }
            }
            return Some(previous);
        }
        let mut parent = path.clone();
        (parent.up() && parent.depth() > 0).then_some(parent)
    }

    fn navigate(&self, event: &gdk::EventKey) -> bool {
        if !event.state().contains(gdk::ModifierType::CONTROL_MASK) {
            return false;
        }
        let key = event.keyval();
        let moving_up = matches!(key, gdk::keys::constants::p | gdk::keys::constants::k);
        let moving_down = matches!(key, gdk::keys::constants::n | gdk::keys::constants::j);
        if !moving_up && !moving_down {
            return false;
        }

        let selection = self.view.selection();
        let (paths, _) = selection.selected_rows();
        // Through the rows as the list shows them, an app's actions
        // included when they are shown.
        let target = match paths.into_iter().next() {
            Some(path) if moving_up => self.row_above(&path),
            Some(path) => self.row_below(&path),
            None => Some(gtk::TreePath::new_first()),
        };
        if let Some(path) = target.filter(|path| self.filter.iter(path).is_some()) {
            selection.select_path(&path);
            self.view
                .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
        }
        true
    }

    /// Applies changes another program makes to `fuzzy-matching`,
    /// `match-fields`, `hidden-apps` and `favorite-apps` while the launcher
    /// runs: the rows are matched, grouped, starred and sorted again for the
    /// same filter text, keeping the highlighted app (see
    /// [`rebuild_keeping_selection`]). The actions shown beneath an app are
    /// closed first, unless only `favorite-apps` changed.
    fn follow_settings(self: &Rc<Self>) {
        for key in [FUZZY_MATCHING, MATCH_FIELDS, HIDDEN_APPS, FAVORITE_APPS] {
            let weak = Rc::downgrade(self);
            self.settings
                .connect_changed(Some(key), move |settings, _| {
                    let Some(page) = weak.upgrade() else {
                        return;
                    };
                    // The page's own Ctrl+Delete, Ctrl+z, Ctrl+f, Alt+Up and
                    // Alt+Down writes are applied already.
                    let settings = AppSettings::read(settings);
                    let applied = page.apps.settings();
                    if applied == settings {
                        return;
                    }
                    let favorites_only = AppSettings {
                        favorites: applied.favorites.clone(),
                        ..settings.clone()
                    } == applied;
                    if favorites_only {
                        page.apply_favorites(settings.favorites);
                    } else {
                        page.close_actions();
                        rebuild_keeping_selection(&page.view, &page.filter, || {
                            page.apps.apply_settings(settings);
                            page.sync_all_actions_beneath();
                            page.update_stars();
                            install_sort(&page.model, &page.apps);
                            page.filter.refilter();
                        });
                    }
                });
        }
    }

    /// The `hidden-apps` setting as stored now.
    fn stored_hidden(&self) -> Vec<String> {
        self.settings
            .strv(HIDDEN_APPS)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Applies the `hidden-apps` entries `hidden` to the rows, then stores
    /// them; whether they were stored. Applied before it is written, the
    /// change notification for the write finds nothing new to apply.
    fn store_hidden(&self, hidden: Vec<String>, id: &str) -> bool {
        self.apps.apply_settings(AppSettings {
            hidden: hidden.clone(),
            ..self.apps.settings()
        });
        self.sync_all_actions_beneath();
        let stored = match self.settings.set_strv(HIDDEN_APPS, hidden) {
            Ok(()) => {
                gio::Settings::sync();
                true
            }
            Err(error) => {
                eprintln!("Failed to change whether {id} is hidden: {error}");
                false
            }
        };
        self.filter.refilter();
        stored
    }

    /// Hides the highlighted app or action: appends its plain desktop id,
    /// or `<desktop id>#<action id>`, to the `hidden-apps` setting as stored
    /// now, after every entry already there, and removes its row. The row
    /// taking its place is highlighted, or the new last row when it was the
    /// last. Beneath an app, its next action is highlighted, or else the
    /// previous one, or else the app, whose actions are then closed.
    fn hide_selected(&self) {
        let Some((model, iter)) = self.view.selection().selected() else {
            return;
        };
        let Some(path) = model.path(&iter) else {
            return;
        };
        let row = row_value(&model, &iter);
        let key = self.apps.key(row);
        let next = (path.depth() > 1)
            .then(|| self.highlight_after_hiding(&path))
            .flatten();
        if row.1.is_none() && self.apps.open.get() == Some(row.0) {
            // Its actions go with its row.
            self.apps.set_open(None);
        }

        let mut hidden = self.stored_hidden();
        let appended = append_last(&mut hidden, &key);
        self.moving.set(true);
        if self.store_hidden(hidden, &key) && appended {
            self.hides.borrow_mut().push(key);
        }
        self.moving.set(false);
        let next = next.and_then(|next| {
            self.filter
                .convert_child_iter_to_iter(&next)
                .and_then(|iter| self.filter.path(&iter))
        });
        match next {
            Some(next) => self.move_cursor_quietly(&next),
            None => {
                select_row_near(&self.view, path.indices().first().copied().unwrap_or(0));
            }
        }
        self.announce_empty_group();
    }

    /// The model row to highlight once the action at `path`, beneath its
    /// app, is hidden: the next action, the previous one, or the app.
    fn highlight_after_hiding(&self, path: &gtk::TreePath) -> Option<gtk::TreeIter> {
        let mut next = path.clone();
        next.next();
        let mut previous = path.clone();
        let mut parent = path.clone();
        parent.up();
        let target = if self.filter.iter(&next).is_some() {
            next
        } else if previous.prev() {
            previous
        } else {
            parent
        };
        let iter = self.filter.iter(&target)?;
        Some(self.filter.convert_iter_to_child_iter(&iter))
    }

    /// Reverses the latest hide of this run that Ctrl+z has not reversed
    /// yet: deletes the last entry equal to the id it appended from the
    /// `hidden-apps` setting as stored now, every other entry staying where
    /// it is. When the app is then listed, its row is highlighted; otherwise
    /// the highlight stays where it is (see [`rebuild_keeping_selection`]).
    /// When another program has taken the id out, that hide is reversed
    /// without changing anything.
    fn undo_hide(&self) {
        let Some(id) = self.hides.borrow_mut().pop() else {
            return;
        };
        let mut hidden = self.stored_hidden();
        if !remove_last(&mut hidden, &id) {
            return;
        }
        rebuild_keeping_selection(&self.view, &self.filter, || {
            self.store_hidden(hidden, &id);
        });
        let restored = self
            .apps
            .row_of(&id)
            .and_then(|row| self.listed_row_path(row));
        if let Some(path) = restored {
            self.view
                .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
        }
    }

    /// The path of the row listing the app with the index `index`, or
    /// `None` when it is not listed.
    fn listed_path(&self, index: usize) -> Option<gtk::TreePath> {
        self.listed_row_path((index, None))
    }

    /// The path of the top-level row listing `row`, or `None` when it is
    /// not listed.
    fn listed_row_path(&self, row: Row) -> Option<gtk::TreePath> {
        (0..self.filter.iter_n_children(None)).find_map(|position| {
            let iter = self.filter.iter_nth_child(None, position)?;
            (row_value(&self.filter, &iter) == row)
                .then(|| gtk::TreePath::from_indicesv(&[position]))
        })
    }

    /// The desktop id of the highlighted app, or `None` when no row is
    /// highlighted or the highlighted row is an action's.
    fn selected_id(&self) -> Option<String> {
        let (model, iter) = self.view.selection().selected()?;
        let (index, action) = row_value(&model, &iter);
        action.is_none().then(|| self.apps.id(index))
    }

    /// Stars the rows of the favorites, and only them.
    fn update_stars(&self) {
        for (index, row) in self.app_rows.borrow().iter().enumerate() {
            set_star(&self.model, row, self.apps.is_favorite(index));
        }
    }

    /// The `favorite-apps` setting as stored now.
    fn stored_favorites(&self) -> Vec<String> {
        self.settings
            .strv(FAVORITE_APPS)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Applies the `favorite-apps` entries `favorites` to the rows: they are
    /// starred and sorted again, keeping the highlighted row and the actions
    /// shown beneath an app.
    fn apply_favorites(&self, favorites: Vec<String>) {
        self.moving.set(true);
        rebuild_keeping_selection(&self.view, &self.filter, || {
            self.apps.apply_settings(AppSettings {
                favorites,
                ..self.apps.settings()
            });
            self.update_stars();
            install_sort(&self.model, &self.apps);
            self.filter.refilter();
        });
        if let Some(path) = self
            .apps
            .open
            .get()
            .and_then(|index| self.listed_path(index))
        {
            self.view.expand_row(&path, false);
        }
        self.moving.set(false);
    }

    /// Applies the `favorite-apps` entries `favorites` to the rows (see
    /// [`Self::apply_favorites`]), then stores them. Applied before it is
    /// written, the change notification for the write finds nothing new to
    /// apply.
    fn store_favorites(&self, favorites: Vec<String>) {
        self.apply_favorites(favorites.clone());
        match self.settings.set_strv(FAVORITE_APPS, favorites) {
            Ok(()) => gio::Settings::sync(),
            Err(error) => eprintln!("Failed to change the favorite applications: {error}"),
        }
    }

    /// Ctrl+f: makes the highlighted app a favorite, after the others, or
    /// takes every entry of it out of the favorites, in the setting as
    /// stored now. Nothing is written when no row, or an action's row, is
    /// highlighted.
    fn toggle_favorite(&self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.store_favorites(toggled_favorite(self.stored_favorites(), &id));
    }

    /// Alt+Up and Alt+Down: moves the highlighted favorite ahead of the
    /// favorite shown above it, or behind the one shown below it, while the
    /// filter text has no words. Nothing is written for the first or last
    /// favorite shown, for an app that is not a favorite, nor for an
    /// action's row.
    fn move_favorite(&self, direction: Direction) {
        if self.apps.has_words() {
            return;
        }
        let Some(id) = self.selected_id() else {
            return;
        };
        let moved = moved_favorite(self.stored_favorites(), &id, direction, |id| {
            self.apps.is_shown(id)
        });
        if let Some(favorites) = moved {
            self.store_favorites(favorites);
        }
    }
}

impl DialogPage for ApplicationsPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn on_entry_changed(&self) {
        // A new filter text closes the app whose actions are shown.
        self.apps.set_open(None);
        self.moving.set(true);
        self.view.collapse_all();
        self.moving.set(false);
        self.apps.rank(self.entry.text().as_str());
        install_sort(&self.model, &self.apps);
        self.filter.refilter();
        self.set_selection();
        self.announce_empty_group();
        // The highlighted row may be the one highlighted before, with the
        // files behind it changed while another page was shown.
        self.update_program_path();
    }

    fn on_entry_activated(&self) {
        if self.waits_for_list() {
            self.defer_activation(Workspace::Current);
            return;
        }
        self.execute_selected(Workspace::Current);
    }

    fn get_id(&self) -> &'static str {
        "apps"
    }

    fn get_name(&self) -> &'static str {
        "<u>A</u>pplications"
    }

    fn get_icon_name(&self) -> &'static str {
        "view-app-grid-symbolic"
    }

    fn get_help(&self) -> &'static str {
        "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it. Applications that offer additional actions, such as opening a new window, are marked with › at the end of their row: the right arrow key, or a click on the mark, shows the actions under the application, and the left arrow key hides them. While they are shown, typing narrows them. Actions are also listed on their own, as \"Application: Action\", once the filter text names them, and a filter written as \"application:action\" lists the actions of the matching applications."
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [
            ("enter", "Launch Application"),
            ("ctrl+=", "Add Application"),
            ("ctrl+shift+c", "Copy Program Path"),
            ("ctrl+delete", "Hide Application"),
            ("ctrl+z", "Undo Hide"),
            ("ctrl+f", "Add or Remove Favorite"),
            ("alt+up", "Move Favorite Up"),
            ("alt+down", "Move Favorite Down"),
            ("ctrl+enter", "Launch on a New Workspace"),
            ("right", "Show Actions"),
            ("left", "Hide Actions"),
        ]
        .into_iter()
        .collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_new_workspace_key(event) {
            // Taken with no row too, and from the Help page, where the filter
            // is off as for Enter: nothing is launched then.
            if !self.root.is_mapped() {
                return true;
            }
            if self.waits_for_list() {
                self.defer_activation(Workspace::NextFree);
            } else {
                self.execute_selected(Workspace::NextFree);
            }
            return true;
        }
        // Alt+Enter never starts anything on this page.
        if is_enter_key(&event.keyval())
            && event.state().contains(gdk::ModifierType::MOD1_MASK)
            && self.root.is_mapped()
        {
            self.alt_enter();
            return true;
        }
        if is_enter_key(&event.keyval()) && self.waits_for_list() {
            self.defer_activation(Workspace::Current);
            return true;
        }
        // Ctrl+f is always taken; it pins or unpins only while the page is
        // shown, and does nothing from the Help page.
        if is_favorite_key(event) {
            if self.root.is_mapped() {
                // Never edits the filter text, with no row highlighted either.
                self.toggle_favorite();
            }
            return true;
        }
        if let Some(direction) = favorite_move_key(event).filter(|_| self.root.is_mapped()) {
            // Never moves the highlight, even where it moves no favorite.
            self.move_favorite(direction);
            return true;
        }
        if is_add_app_shortcut(event) {
            self.open_form();
            return true;
        }
        // Only while the page is shown: not from the Help page.
        if is_copy_program_path_key(event) && self.root.is_mapped() {
            self.copy_program_path();
            return true;
        }
        if event.keyval() == gdk::keys::constants::Delete
            && event.state().contains(gdk::ModifierType::CONTROL_MASK)
        {
            // Taken with no row highlighted too: on this page Ctrl+Delete
            // never edits the filter text.
            self.hide_selected();
            return true;
        }
        if event.keyval() == gdk::keys::constants::z
            && event.state().contains(gdk::ModifierType::CONTROL_MASK)
        {
            self.undo_hide();
            return true;
        }
        if self.narrowing_key(event) {
            return true;
        }
        if self.arrow_key(event) {
            return true;
        }
        self.navigate(event)
    }

    fn get_keybinding(&self) -> Option<char> {
        Some('a')
    }

    fn capture_key_event(&self, event: &gdk::EventKey) -> bool {
        self.form.is_open() && self.form.handle_key(event)
    }

    fn uses_key(&self, event: &gdk::EventKey) -> bool {
        if self.form.is_open() {
            return self.form.uses_key(event);
        }
        // Ctrl+z and the add-app shortcut with neither Alt nor Super, as for
        // the dialog's own Ctrl keys.
        let ctrl_only = event.state().contains(gdk::ModifierType::CONTROL_MASK)
            && !event
                .state()
                .intersects(gdk::ModifierType::MOD1_MASK | gdk::ModifierType::SUPER_MASK);
        // Ctrl+f also from the Help page, where it does nothing, so that it
        // never reaches the window manager.
        is_favorite_key(event)
            || (is_copy_program_path_key(event) && self.root.is_mapped())
            || (ctrl_only
                && (is_add_app_shortcut(event) || event.keyval() == gdk::keys::constants::z))
    }

    fn show(&self) {
        if self.form.is_open() {
            self.form.take_focus();
        } else {
            self.view.grab_focus();
        }
    }

    fn is_launching(&self) -> bool {
        self.launched.get()
    }
}

/// The strip's program path: one line from the page's left edge that never
/// takes the focus. It keeps the end of the path, the program's file name,
/// in view, and never asks for more width than the window has.
fn program_path_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.style_context().add_class("app_path");
    label.set_can_focus(false);
    label.set_single_line_mode(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    label.set_max_width_chars(1);
    label.set_xalign(0.0);
    label
}

/// Ctrl+Shift+C, whatever the state of Caps Lock.
pub fn is_copy_program_path_key(event: &gdk::EventKey) -> bool {
    let modifiers = event.state() & gtk::accelerator_get_default_mod_mask();
    modifiers == gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
        && event.keyval().to_lower() == gdk::keys::constants::c
}

/// The strip's + button. A GtkButton's accessible exposes neither its label
/// nor any child, so the button is an event box that shows assistive
/// technologies a push button holding the + label, as GTK 4 buttons do. It
/// takes no focus: the keys stay with the filter and the list.
fn add_app_button() -> gtk::EventBox {
    let label = gtk::Label::new(Some("+"));
    label.set_margin_start(10);
    label.set_margin_end(10);
    label.set_margin_top(4);
    label.set_margin_bottom(4);
    let button = gtk::EventBox::new();
    button.set_visible_window(false);
    button.style_context().add_class("add_app_button");
    button.add(&label);
    if let Some(accessible) = button.accessible() {
        accessible.set_role(gtk::atk::Role::PushButton);
        accessible.set_name("Add application");
    }
    button
}

/// The modifiers of a key press that select a shortcut: Shift, Control,
/// Alt and Super, whatever the state of Caps Lock and Num Lock.
fn shortcut_modifiers(event: &gdk::EventKey) -> gdk::ModifierType {
    event.state() & gtk::accelerator_get_default_mod_mask()
}

/// Ctrl+Enter, and Ctrl with the keypad Enter: launch on the next free
/// workspace.
fn is_new_workspace_key(event: &gdk::EventKey) -> bool {
    shortcut_modifiers(event) == gdk::ModifierType::CONTROL_MASK && is_enter_key(&event.keyval())
}

/// Ctrl+f, whatever the state of Caps Lock: pin or unpin.
fn is_favorite_key(event: &gdk::EventKey) -> bool {
    shortcut_modifiers(event) == gdk::ModifierType::CONTROL_MASK
        && event.keyval().to_lower() == gdk::keys::constants::f
}

/// Alt+Up and Alt+Down, the keypad's arrows too: move a favorite.
fn favorite_move_key(event: &gdk::EventKey) -> Option<Direction> {
    if shortcut_modifiers(event) != gdk::ModifierType::MOD1_MASK {
        return None;
    }
    match event.keyval() {
        gdk::keys::constants::Up | gdk::keys::constants::KP_Up => Some(Direction::Up),
        gdk::keys::constants::Down | gdk::keys::constants::KP_Down => Some(Direction::Down),
        _ => None,
    }
}

/// Ctrl and the key bearing + on a US layout, shifted or not, or the
/// keypad's +.
fn is_add_app_shortcut(event: &gdk::EventKey) -> bool {
    event.state().contains(gdk::ModifierType::CONTROL_MASK)
        && matches!(
            event.keyval(),
            gdk::keys::constants::plus | gdk::keys::constants::equal | gdk::keys::constants::KP_Add
        )
}

/// The path of the row under a button event on `view`, if any, and the
/// column under it.
fn cell_at_pointer(
    view: &gtk::TreeView,
    event: &gdk::EventButton,
) -> Option<(gtk::TreePath, Option<gtk::TreeViewColumn>)> {
    let bin_window = view.bin_window()?;
    let (mut x, mut y) = event.position();
    if event.window().as_ref() != Some(&bin_window) {
        let (_, origin_x, origin_y) = bin_window.origin();
        let (root_x, root_y) = event.root();
        x = root_x - f64::from(origin_x);
        y = root_y - f64::from(origin_y);
    }
    // Only a release over the rows themselves.
    if x < 0.0
        || y < 0.0
        || x >= f64::from(bin_window.width())
        || y >= f64::from(bin_window.height())
    {
        return None;
    }
    let (path, column, _, _) = view.path_at_pos(x as i32, y as i32)?;
    Some((path?, column))
}

/// A page file's applications: those it names that are installed and ask to
/// be shown, in the order it names them, whatever the filter text, the
/// launch history and the hidden apps.
pub struct AppListPage {
    id: String,
    name: String,
    icon: String,
    key: Option<char>,
    entry: gtk::Entry,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    rows: Rc<AppRows>,
    settings: gio::Settings,
    launch_history: Rc<RefCell<Vec<String>>>,
    quit: Rc<dyn Fn()>,
    // An app was started: ilia is closing.
    launched: Rc<Cell<bool>>,
}

/// The apps of an `AppListPage`, by the INDEX of their rows, and which of
/// them the filter text lists. They match as on the applications page.
struct AppRows {
    // Every field, folded.
    all: Vec<AppText>,
    // The fields `match-fields` lets the filter search.
    text: RefCell<Vec<AppText>>,
    settings: RefCell<(bool, MatchFields)>,
    words: RefCell<Vec<String>>,
}

impl AppRows {
    fn new(all: Vec<AppText>, settings: &gio::Settings) -> Self {
        let rows = Self {
            text: RefCell::new(all.clone()),
            all,
            settings: RefCell::new((false, MatchFields::ALL)),
            words: RefCell::default(),
        };
        rows.apply_settings(settings);
        rows
    }

    /// Takes the `fuzzy-matching` and `match-fields` settings; whether they
    /// changed.
    fn apply_settings(&self, settings: &gio::Settings) -> bool {
        let new = AppSettings::read(settings);
        let (subsequence, fields) = (new.subsequence, new.fields);
        let (old_subsequence, old_fields) = self.settings.replace((subsequence, fields));
        if old_fields != fields {
            *self.text.borrow_mut() = self.all.iter().map(|text| text.only(fields)).collect();
        }
        (old_subsequence, old_fields) != (subsequence, fields)
    }

    fn is_listed(&self, index: usize) -> bool {
        let words = self.words.borrow();
        words.is_empty()
            || self
                .text
                .borrow()
                .get(index)
                .and_then(|text| text.tier(&words, self.settings.borrow().0))
                .is_some()
    }
}

impl AppListPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        file: &PageFile,
        key: Option<char>,
        quit: Rc<dyn Fn()>,
    ) -> Self {
        let launch_history: Vec<String> = settings
            .strv(LAUNCH_HISTORY)
            .iter()
            .map(ToString::to_string)
            .collect();
        let icon_size = settings.int("icon-size");
        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
            u32::static_type(),
        ]);
        let blank = blank_icon(icon_size);
        let ids: &[String] = match &file.rows {
            Rows::Apps(ids) => ids,
            _ => &[],
        };
        let mut texts = Vec::new();
        for id in ids {
            // GIO finds no app for an id that is not installed, or whose
            // TryExec program is missing.
            let Some(desktop) = gio::DesktopAppInfo::new(id) else {
                continue;
            };
            if !desktop.should_show() {
                continue;
            }
            insert_list_row(&model, &desktop, texts.len(), blank.as_ref());
            texts.push(AppText::new(&desktop, MatchFields::ALL));
        }
        let rows = Rc::new(AppRows::new(texts, &settings));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let rows_for_filter = rows.clone();
        filter.set_visible_func(move |tree_model, iter| {
            rows_for_filter.is_listed(index_value(tree_model, iter))
        });
        let (view, root) = app_view(&filter, icon_size);

        let page = Self {
            id: file.id.clone(),
            // The title is plain text.
            name: glib::markup_escape_text(&file.name).into(),
            icon: file.icon.clone(),
            key,
            entry,
            filter,
            view,
            root,
            path: RefCell::new(gtk::TreePath::new_first()),
            rows,
            settings,
            launch_history: Rc::new(RefCell::new(launch_history)),
            quit,
            launched: Rc::default(),
        };
        connect_launch(
            &page.view,
            &page.settings,
            &page.launch_history,
            &page.launched,
            &page.quit,
        );
        page.follow_settings();
        page.set_selection();
        if icon_size > 0 {
            load_icons_in_background(&model, icon_size);
        }
        page
    }

    /// Applies changes another program makes to `fuzzy-matching` and
    /// `match-fields` while the launcher runs, keeping the highlighted app.
    fn follow_settings(&self) {
        for key in [FUZZY_MATCHING, MATCH_FIELDS] {
            let (rows, filter, view) = (self.rows.clone(), self.filter.clone(), self.view.clone());
            self.settings
                .connect_changed(Some(key), move |settings, _| {
                    if rows.apply_settings(settings) {
                        rebuild_keeping_selection(&view, &filter, || filter.refilter());
                    }
                });
        }
    }

    /// Highlights the first row listed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        *self.path.borrow_mut() = select_first_row(&self.view);
    }
}

impl DialogPage for AppListPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn on_entry_changed(&self) {
        *self.rows.words.borrow_mut() = query_words(self.entry.text().as_str());
        self.filter.refilter();
        self.set_selection();
    }

    fn on_entry_activated(&self) {
        let Some((model, iter)) = self.view.selection().selected() else {
            return;
        };
        if let Some(app) = app_value(&model, &iter) {
            execute_app(
                &app,
                &self.settings,
                &self.launch_history,
                &self.launched,
                &self.quit,
            );
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
        "This dialog lists the applications a page file names, in its order. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it."
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Launch Application")].into_iter().collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        navigate(&self.view, &self.path, event)
    }

    fn get_keybinding(&self) -> Option<char> {
        self.key
    }

    fn show(&self) {
        self.view.grab_focus();
    }
}

/// A page file's list of apps showing the rows of `filter`, and the
/// scrolled window holding it.
fn app_view(filter: &gtk::TreeModelFilter, icon_size: i32) -> (gtk::TreeView, gtk::Widget) {
    let view = gtk::TreeView::with_model(filter);
    view.style_context().add_class("item_view");
    view.set_headers_visible(false);
    view.set_fixed_height_mode(true);
    view.set_enable_search(false);
    view.selection().set_mode(gtk::SelectionMode::Single);
    if icon_size > 0 {
        let renderer = gtk::CellRendererPixbuf::new();
        let column =
            gtk::TreeViewColumn::with_attributes("Icon", &renderer, &[("pixbuf", ICON as i32)]);
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&column);
    }
    let renderer = gtk::CellRendererText::new();
    let column = gtk::TreeViewColumn::with_attributes("Name", &renderer, &[("text", NAME as i32)]);
    column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
    view.append_column(&column);
    view.set_activate_on_single_click(true);

    let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scrolled.style_context().add_class("scrolled_window");
    scrolled.add(&view);
    scrolled.set_hexpand(true);
    scrolled.set_vexpand(true);
    (view, scrolled.upcast())
}

/// The Applications page's list and the scrolled window holding it: the
/// icon, the name, the star of a favorite, and at the end of the row of an
/// app with actions the action cue, whose column is returned too. An app's actions are shown in
/// rows beneath its own, indented; the toolkit draws no expander.
fn applications_view(
    filter: &gtk::TreeModelFilter,
    icon_size: i32,
) -> (gtk::TreeView, gtk::Widget, gtk::TreeViewColumn) {
    let view = gtk::TreeView::with_model(filter);
    view.style_context().add_class("item_view");
    view.set_headers_visible(false);
    view.set_fixed_height_mode(true);
    view.set_enable_search(false);
    view.selection().set_mode(gtk::SelectionMode::Single);
    // Every column asks for a width that never changes, the name taking
    // the rest: GtkTreeView keeps the space it once gave an expanding
    // column, so a column growing afterwards would push the cue out of
    // sight.
    let separator = horizontal_separator(&view);
    if icon_size > 0 {
        let renderer = gtk::CellRendererPixbuf::new();
        let column =
            gtk::TreeViewColumn::with_attributes("Icon", &renderer, &[("pixbuf", ICON as i32)]);
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        column.set_fixed_width(icon_size + 2 * renderer.padding().0 + separator);
        view.append_column(&column);
    }
    let renderer = gtk::CellRendererText::new();
    let padding = renderer.padding();
    let name = gtk::TreeViewColumn::new();
    name.set_title("Name");
    TreeViewColumnExt::pack_start(&name, &renderer, true);
    name.set_sizing(gtk::TreeViewColumnSizing::Fixed);
    name.set_fixed_width(1);
    name.set_expand(true);
    TreeViewColumnExt::set_cell_data_func(
        &name,
        &renderer,
        Some(Box::new(move |column, cell, model, iter| {
            render_name(column, cell, model, iter, padding);
        })),
    );
    view.append_column(&name);

    // The star of a favorite.
    let renderer = gtk::CellRendererText::new();
    renderer.set_xalign(0.5);
    let star =
        gtk::TreeViewColumn::with_attributes("Favorite", &renderer, &[("text", FAVORITE as i32)]);
    star.set_sizing(gtk::TreeViewColumnSizing::Fixed);
    let (star_width, _) = view.create_pango_layout(Some(STAR)).pixel_size();
    star.set_fixed_width(star_width + 4 * renderer.xpad() as i32);
    view.append_column(&star);

    let renderer = gtk::CellRendererText::new();
    renderer.set_xalign(0.5);
    let cue = gtk::TreeViewColumn::new();
    cue.set_title("Actions");
    TreeViewColumnExt::pack_start(&cue, &renderer, false);
    cue.set_sizing(gtk::TreeViewColumnSizing::Fixed);
    cue.set_fixed_width(CUE_WIDTH);
    TreeViewColumnExt::set_cell_data_func(&cue, &renderer, Some(Box::new(render_cue)));
    view.append_column(&cue);

    // The rows of an app's actions are its rows' children: the name
    // reports the app's row as expandable, and as expanded while they are
    // shown, and the rows beneath are indented.
    view.set_expander_column(Some(&name));
    view.set_show_expanders(false);
    view.set_level_indentation(ACTION_INDENT);
    view.set_activate_on_single_click(true);

    let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scrolled.style_context().add_class("scrolled_window");
    scrolled.add(&view);
    scrolled.set_hexpand(true);
    scrolled.set_vexpand(true);
    (view, scrolled.upcast(), cue)
}

/// The space the theme puts between the cells of a row of `view`.
fn horizontal_separator(view: &gtk::TreeView) -> i32 {
    use glib::translate::{ToGlibPtr, ToGlibPtrMut};
    // gtk-rs passes GTK an uninitialized value, which it refuses.
    let mut value = glib::Value::from_type(glib::Type::I32);
    // SAFETY: the widget and the property name are valid for the call, and
    // the value is initialized with the property's type.
    unsafe {
        gtk::ffi::gtk_widget_style_get_property(
            view.upcast_ref::<gtk::Widget>().to_glib_none().0,
            "horizontal-separator".to_glib_none().0,
            value.to_glib_none_mut().0,
        );
    }
    value.get::<i32>().unwrap_or_default()
}

/// Fills in the name cell of a row. An action's row is the app name, a
/// colon and a space, then the action name, the first part in a colour
/// between the text's and the row's background. On a row of its own the
/// text begins `ACTION_INDENT` further right; beneath its app the row
/// itself is indented.
///
/// The colours depend on nothing but the row itself, never on whether it
/// is highlighted. The column's one renderer is filled in for whichever
/// row GTK asks about, and the accessible of a cell keeps the attributes
/// it was last given: it refreshes them when its row changes, not when
/// the highlight moves on. Colours chosen for the highlight would stay
/// with a row after it lost it. The plain colours hold on a highlighted
/// row as well whenever the highlight lies nearer the plain background
/// than the plain text colour, as it does in the stock theme: the app
/// name, halfway between the two, is then nearer the highlight than the
/// action name.
fn render_name(
    column: &gtk::TreeViewColumn,
    cell: &gtk::CellRenderer,
    model: &gtk::TreeModel,
    iter: &gtk::TreeIter,
    (xpad, ypad): (i32, i32),
) {
    let text: String = model.value(iter, NAME as i32).get().unwrap_or_default();
    let prefix = model
        .value(iter, PREFIX as i32)
        .get::<u32>()
        .unwrap_or_default();
    let own_row = prefix > 0 && model.iter_parent(iter).is_none();
    cell.set_padding(if own_row { xpad + ACTION_INDENT } else { xpad }, ypad);
    let attributes = (prefix > 0 && prefix as usize <= text.len()).then(|| {
        let view = column.tree_view().and_downcast::<gtk::TreeView>();
        let (foreground, background) = row_colors(view.as_ref());
        action_attributes(prefix, &foreground, &background)
    });
    cell.set_property("attributes", attributes);
    cell.set_property("text", &text);
}

/// The text colour and the background colour of a plain, unhighlighted
/// row of `view`.
fn row_colors(view: Option<&gtk::TreeView>) -> (gdk::RGBA, gdk::RGBA) {
    let state = gtk::StateFlags::NORMAL;
    let (foreground, background) = view.map_or((gdk::RGBA::BLACK, None), |view| {
        let context = view.style_context();
        context.save();
        context.set_state(state);
        let foreground = context.color(state);
        let background = context
            .style_property_for_state("background-color", state)
            .get::<gdk::RGBA>()
            .ok()
            .filter(|color| color.alpha() > 0.0)
            .or_else(|| context.lookup_color("theme_base_color"));
        context.restore();
        (foreground, background)
    });
    let background = background.unwrap_or_else(|| {
        let light = foreground.red() + foreground.green() + foreground.blue() > 1.5;
        if light {
            gdk::RGBA::BLACK
        } else {
            gdk::RGBA::WHITE
        }
    });
    (foreground, background)
}

/// The text attributes of an action row whose first `prefix` bytes are the
/// app name and ": ": that part in a colour halfway between `foreground`
/// and `background`, the action name in `foreground`. The background is
/// stated too, invisibly, so that assistive technologies can compare the
/// colours.
fn action_attributes(
    prefix: u32,
    foreground: &gdk::RGBA,
    background: &gdk::RGBA,
) -> pango::AttrList {
    let channel = |value: f64| (value.clamp(0.0, 1.0) * 65535.0).round() as u16;
    let mix = |a: f64, b: f64| channel((a + b) / 2.0);
    let dimmed = (
        mix(foreground.red(), background.red()),
        mix(foreground.green(), background.green()),
        mix(foreground.blue(), background.blue()),
    );
    let list = pango::AttrList::new();
    let add = |mut attribute: pango::Attribute, start: u32, end: u32| {
        attribute.set_start_index(start);
        attribute.set_end_index(end);
        list.insert(attribute);
    };
    add(
        pango::AttrColor::new_foreground(dimmed.0, dimmed.1, dimmed.2).into(),
        0,
        prefix,
    );
    add(
        pango::AttrColor::new_foreground(
            channel(foreground.red()),
            channel(foreground.green()),
            channel(foreground.blue()),
        )
        .into(),
        prefix,
        pango::ATTR_INDEX_TO_TEXT_END,
    );
    add(
        pango::AttrColor::new_background(
            channel(background.red()),
            channel(background.green()),
            channel(background.blue()),
        )
        .into(),
        0,
        pango::ATTR_INDEX_TO_TEXT_END,
    );
    // Pango takes an alpha of 0 for none set.
    add(
        pango::AttrInt::new_background_alpha(1).into(),
        0,
        pango::ATTR_INDEX_TO_TEXT_END,
    );
    list
}

/// Fills in the cue cell of a row: the cue for the row of an app with
/// actions to show, nothing otherwise.
fn render_cue(
    _column: &gtk::TreeViewColumn,
    cell: &gtk::CellRenderer,
    model: &gtk::TreeModel,
    iter: &gtk::TreeIter,
) {
    let cue = has_actions_beneath(model, iter);
    cell.set_property("text", if cue { ACTION_CUE } else { "" });
}

/// Whether the row at `iter` of the Applications page's list `model` is an
/// app's row with rows of its actions beneath it in the page's model,
/// whether or not the filter lists them: an app whose actions are all
/// narrowed away keeps its cue.
fn has_actions_beneath(model: &gtk::TreeModel, iter: &gtk::TreeIter) -> bool {
    let (model, iter) = match model.downcast_ref::<gtk::TreeModelFilter>() {
        Some(filter) => match filter.model() {
            Some(child) => (child, filter.convert_iter_to_child_iter(iter)),
            None => return false,
        },
        None => (model.clone(), *iter),
    };
    model.iter_parent(&iter).is_none()
        && action_value(&model, &iter).is_none()
        && model.iter_has_child(&iter)
}

/// The accessible states of the name cell of an app's row whose actions
/// are shown with every one of them narrowed away. GtkTreeView reports a
/// row as expandable only while it has rows beneath it, and as expanded
/// only while those are on show, so that row would read as neither. The
/// state sets of the toolkit's text cells pass through here, and that one
/// cell gains both states.
mod open_group_state {
    use std::cell::RefCell;
    use std::rc::{Rc, Weak};
    use std::sync::OnceLock;

    use glib::translate::{FromGlibPtrFull, ToGlibPtr};
    use gtk::atk::ffi as atk_ffi;
    use gtk::prelude::*;

    use super::ApplicationsPage;

    type RefStateSet = unsafe extern "C" fn(*mut atk_ffi::AtkObject) -> *mut atk_ffi::AtkStateSet;

    /// The text cells' own state sets.
    static TOOLKIT_STATES: OnceLock<Option<RefStateSet>> = OnceLock::new();

    thread_local! {
        // The Applications page: the cell its list reports so.
        static PAGE: RefCell<Weak<ApplicationsPage>> = const { RefCell::new(Weak::new()) };
    }

    /// Reports the cell of `page`'s list.
    pub fn follow(page: &Rc<ApplicationsPage>) {
        PAGE.with(|slot| *slot.borrow_mut() = Rc::downgrade(page));
        TOOLKIT_STATES.get_or_init(|| {
            // SAFETY: the class is referenced and never released, so it
            // stays valid; GTK runs on this thread alone, and the pointer
            // is replaced before any cell exists to call it.
            unsafe {
                let class = glib::gobject_ffi::g_type_class_ref(
                    gtk::ffi::gtk_text_cell_accessible_get_type(),
                ) as *mut atk_ffi::AtkObjectClass;
                if class.is_null() {
                    return None;
                }
                let toolkit = (*class).ref_state_set;
                (*class).ref_state_set = Some(ref_state_set);
                toolkit
            }
        });
    }

    unsafe extern "C" fn ref_state_set(
        object: *mut atk_ffi::AtkObject,
    ) -> *mut atk_ffi::AtkStateSet {
        let states = match TOOLKIT_STATES.get().copied().flatten() {
            Some(toolkit) => toolkit(object),
            None => atk_ffi::atk_state_set_new(),
        };
        if !states.is_null() && is_empty_group_cell(object) {
            atk_ffi::atk_state_set_add_state(states, atk_ffi::ATK_STATE_EXPANDABLE);
            atk_ffi::atk_state_set_add_state(states, atk_ffi::ATK_STATE_EXPANDED);
        }
        states
    }

    /// Whether `object` is the name cell of the page's app row whose
    /// actions are shown and all narrowed away.
    unsafe fn is_empty_group_cell(object: *mut atk_ffi::AtkObject) -> bool {
        let Some(page) = PAGE
            .try_with(|slot| slot.try_borrow().ok().and_then(|page| page.upgrade()))
            .ok()
            .flatten()
        else {
            return false;
        };
        let Some(list) = page.view.accessible() else {
            return false;
        };
        let list: *mut atk_ffi::AtkObject = list.to_glib_none().0;
        atk_ffi::atk_object_get_parent(object) == list
            && page.empty_group_cell_index()
                == Some(atk_ffi::atk_object_get_index_in_parent(object))
    }

    /// The accessible child of `view` at `index`.
    pub fn cell(view: &gtk::TreeView, index: i32) -> Option<gtk::atk::Object> {
        let list = view.accessible()?;
        // SAFETY: the list's accessible is valid, and the child comes with
        // a reference of its own, or is null.
        unsafe {
            let child = atk_ffi::atk_object_ref_accessible_child(list.to_glib_none().0, index);
            (!child.is_null()).then(|| gtk::atk::Object::from_glib_full(child))
        }
    }

    /// Reports `cell` as expandable and expanded.
    pub fn announce_open(cell: &gtk::atk::Object) {
        // SAFETY: the cell is a valid accessible.
        unsafe {
            for state in [atk_ffi::ATK_STATE_EXPANDABLE, atk_ffi::ATK_STATE_EXPANDED] {
                atk_ffi::atk_object_notify_state_change(
                    cell.to_glib_none().0,
                    state as atk_ffi::AtkState,
                    glib::ffi::GTRUE,
                );
            }
        }
    }

    /// Reports whether `cell` is expanded as its state set now has it.
    pub fn announce_actual(cell: &gtk::atk::Object) {
        // SAFETY: the cell is a valid accessible; its state set comes with
        // a reference of its own, released here.
        unsafe {
            let object = cell.to_glib_none().0;
            let states = atk_ffi::atk_object_ref_state_set(object);
            if states.is_null() {
                return;
            }
            let expanded =
                atk_ffi::atk_state_set_contains_state(states, atk_ffi::ATK_STATE_EXPANDED);
            glib::gobject_ffi::g_object_unref(states.cast());
            atk_ffi::atk_object_notify_state_change(
                object,
                atk_ffi::ATK_STATE_EXPANDED as atk_ffi::AtkState,
                expanded,
            );
        }
    }
}

/// Whether the filter of the Applications page lists the row at `iter`.
fn is_row_listed(apps: &AppList, model: &gtk::TreeModel, iter: &gtk::TreeIter) -> bool {
    let index = index_value(model, iter);
    match action_value(model, iter) {
        None => apps.is_listed(index),
        Some(action) if model.iter_parent(iter).is_some() => {
            apps.is_shown_beneath(index, action) && apps.passes_narrowing(index, action)
        }
        Some(action) => apps.is_action_listed(index, action),
    }
}

/// Launches the app of a row activated in `view`.
fn connect_launch(
    view: &gtk::TreeView,
    settings: &gio::Settings,
    history: &Rc<RefCell<Vec<String>>>,
    launched: &Rc<Cell<bool>>,
    quit: &Rc<dyn Fn()>,
) {
    let (settings, history, quit) = (settings.clone(), history.clone(), quit.clone());
    let launched = launched.clone();
    view.connect_row_activated(move |view, path, _| {
        let Some(filter) = view.model().and_downcast::<gtk::TreeModelFilter>() else {
            return;
        };
        let Some(iter) = filter.iter(path) else {
            return;
        };
        let Some(app) = app_value(&filter, &iter) else {
            return;
        };
        execute_app(&app, &settings, &history, &launched, &quit);
    });
}

/// The transparent icon a row shows until its app's icon is loaded.
fn blank_icon(icon_size: i32) -> Option<Pixbuf> {
    if icon_size <= 0 {
        return None;
    }
    Pixbuf::new(Colorspace::Rgb, true, 8, icon_size, icon_size).map(|pixbuf| {
        pixbuf.fill(0);
        pixbuf
    })
}

fn index_value(model: &impl glib::IsA<gtk::TreeModel>, iter: &gtk::TreeIter) -> usize {
    model
        .value(iter, INDEX as i32)
        .get::<u32>()
        .unwrap_or_default() as usize
}

/// The index of the action of a row of the Applications page, or `None` for
/// an app's row.
fn action_value(model: &impl glib::IsA<gtk::TreeModel>, iter: &gtk::TreeIter) -> Option<usize> {
    let action = model.value(iter, ACTION as i32).get::<i32>().ok()?;
    usize::try_from(action).ok()
}

/// The app and the action of a row of the Applications page.
fn row_value(model: &impl glib::IsA<gtk::TreeModel>, iter: &gtk::TreeIter) -> Row {
    (index_value(model, iter), action_value(model, iter))
}

fn app_value(
    model: &impl glib::IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
) -> Option<gio::DesktopAppInfo> {
    model
        .value(iter, APP_INFO as i32)
        .get::<gio::DesktopAppInfo>()
        .ok()
}

/// The desktop id of an entry the Applications page lists, or `None` for
/// one it leaves out. There is one entry per desktop id: like GIO, the
/// readers take each id from the first directory of the Desktop Entry
/// search order that has it, `XDG_DATA_HOME` and then `XDG_DATA_DIRS`.
fn shown_id(desktop: &gio::DesktopAppInfo) -> Option<String> {
    if !desktop.should_show() {
        return None;
    }
    desktop.id().map(String::from)
}

/// Adds a row for `desktop` whose INDEX is `index` to a page file's list,
/// and returns it. GtkListStore iterators stay valid across re-sorting.
fn insert_list_row(
    model: &gtk::ListStore,
    desktop: &gio::DesktopAppInfo,
    index: usize,
    blank: Option<&Pixbuf>,
) -> gtk::TreeIter {
    // The name the filter matches (see `AppText::new`).
    let name = entries::app_name(desktop);
    let index = index as u32;
    let mut values: Vec<(u32, &dyn ToValue)> =
        vec![(NAME, &name), (APP_INFO, desktop), (INDEX, &index)];
    if let Some(blank) = blank {
        values.push((ICON, blank));
    }
    model.insert_with_values(None, &values)
}

/// Adds a top-level row for the app `desktop` whose INDEX is `index` to
/// the Applications page, and returns it. GtkTreeStore iterators stay valid
/// across re-sorting.
fn insert_row(
    model: &gtk::TreeStore,
    desktop: &gio::DesktopAppInfo,
    index: usize,
    blank: Option<&Pixbuf>,
) -> gtk::TreeIter {
    let name = entries::app_name(desktop);
    let index = index as u32;
    let mut values: Vec<(u32, &dyn ToValue)> = vec![
        (NAME, &name),
        (APP_INFO, desktop),
        (INDEX, &index),
        (ACTION, &-1i32),
        (PREFIX, &0u32),
    ];
    if let Some(blank) = blank {
        values.push((ICON, blank));
    }
    model.insert_with_values(None, None, &values)
}

/// Adds a row for the action `action`, named `action_name`, of the app
/// `desktop` whose INDEX is `index`: beneath the app's row `parent`, or at
/// the top level.
fn insert_action_row(
    model: &gtk::TreeStore,
    parent: Option<&gtk::TreeIter>,
    desktop: &gio::DesktopAppInfo,
    index: usize,
    action: usize,
    action_name: &str,
) -> gtk::TreeIter {
    let app_name = entries::app_name(desktop);
    let text = action_row_text(&app_name, action_name);
    let prefix = (app_name.len() + ": ".len()) as u32;
    let (index, action) = (index as u32, action as i32);
    model.insert_with_values(
        parent,
        None,
        &[
            (NAME, &text),
            (APP_INFO, desktop),
            (INDEX, &index),
            (ACTION, &action),
            (PREFIX, &prefix),
        ],
    )
}

/// Shows the star in the FAVORITE column of the app's row `row` when
/// `favorite`, and nothing otherwise. Only a change is written: every write
/// announces the row as changed.
fn set_star(model: &gtk::TreeStore, row: &gtk::TreeIter, favorite: bool) {
    let star = if favorite { STAR } else { "" };
    let shown = model
        .value(row, FAVORITE as i32)
        .get::<String>()
        .unwrap_or_default();
    if shown != star {
        model.set(row, &[(FAVORITE, &star)]);
    }
}

fn install_sort(model: &gtk::TreeStore, apps: &Rc<AppList>) {
    let apps = apps.clone();
    model.set_sort_func(gtk::SortColumn::Index(NAME), move |model, a, b| {
        // The rows beneath an app keep the order of its Actions key.
        if model.iter_parent(a).is_some() {
            return action_value(model, a).cmp(&action_value(model, b));
        }
        apps.compare_rows(row_value(model, a), row_value(model, b))
    });
}

/// Whether the `hidden-apps` entries `hidden` hide the desktop id `id`. An
/// entry is an id or a pattern; one starting with `!` makes what the rest of
/// it fits visible again. The last entry that fits decides.
fn is_hidden(id: &str, hidden: &[String]) -> bool {
    hidden
        .iter()
        .rev()
        .find_map(|entry| match entry.strip_prefix('!') {
            Some(pattern) => fits_pattern(pattern, id).then_some(false),
            None => fits_pattern(entry, id).then_some(true),
        })
        .unwrap_or(false)
}

/// Appends `id` to `hidden`, after every entry, unless it is the last entry
/// already; whether it was appended. Placed last, the plain id decides over
/// any earlier `!` entry.
fn append_last(hidden: &mut Vec<String>, id: &str) -> bool {
    if hidden.last().map(String::as_str) == Some(id) {
        return false;
    }
    hidden.push(id.to_owned());
    true
}

/// Deletes the last entry of `hidden` equal to `id`, leaving the others in
/// their order; whether there was one.
fn remove_last(hidden: &mut Vec<String>, id: &str) -> bool {
    match hidden.iter().rposition(|entry| entry == id) {
        Some(position) => {
            hidden.remove(position);
            true
        }
        None => false,
    }
}

/// Whether `text` fits `pattern`, in which `*` stands for any run of
/// characters, `?` for exactly one character, and every other character for
/// itself.
fn fits_pattern(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // The position of the last `*` passed, and where the text its run
    // covers ends.
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&character) if character == '?' || character == text[t] => {
                p += 1;
                t += 1;
            }
            // Let the last `*` cover one more character, or fail.
            _ => {
                let Some((star_p, star_t)) = star else {
                    return false;
                };
                star = Some((star_p, star_t + 1));
                p = star_p + 1;
                t = star_t + 1;
            }
        }
    }
    pattern[p..].iter().all(|&character| character == '*')
}

/// The app fields the filter searches besides the name, which it always
/// searches: the `match-fields` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MatchFields {
    generic_name: bool,
    keywords: bool,
    comment: bool,
}

impl MatchFields {
    const ALL: Self = Self {
        generic_name: true,
        keywords: true,
        comment: true,
    };

    /// The fields `names` names; other names are ignored.
    fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut fields = Self::default();
        for name in names {
            match name {
                "generic-name" => fields.generic_name = true,
                "keywords" => fields.keywords = true,
                "comment" => fields.comment = true,
                _ => {}
            }
        }
        fields
    }
}

/// An app's text as the filter compares it: folded, and empty for the
/// fields the filter does not search.
#[derive(Clone, Debug, Default, PartialEq)]
struct AppText {
    name: String,
    generic_name: String,
    // Separated by spaces, which no filter word holds: a word matches within
    // one keyword.
    keywords: String,
    comment: String,
}

impl AppText {
    fn new(desktop: &gio::DesktopAppInfo, fields: MatchFields) -> Self {
        let fold =
            |text: Option<glib::GString>| text.map_or_else(String::new, |text| fold_text(&text));
        Self {
            name: fold_text(&entries::app_name(desktop)),
            generic_name: fold(desktop.generic_name().filter(|_| fields.generic_name)),
            keywords: if fields.keywords {
                fold_text(&keywords(desktop).join(" "))
            } else {
                String::new()
            },
            comment: fold(desktop.description().filter(|_| fields.comment)),
        }
    }

    /// The same text with only the fields `fields` searched.
    fn only(&self, fields: MatchFields) -> Self {
        let field = |text: &String, searched: bool| {
            if searched {
                text.clone()
            } else {
                String::new()
            }
        };
        Self {
            name: self.name.clone(),
            generic_name: field(&self.generic_name, fields.generic_name),
            keywords: field(&self.keywords, fields.keywords),
            comment: field(&self.comment, fields.comment),
        }
    }

    /// The group the app is listed in for the filter `words`, or `None` when
    /// it does not match. `words` is not empty. Subsequences of the name
    /// count only when `subsequence` is set.
    fn tier(&self, words: &[String], subsequence: bool) -> Option<AppTier> {
        let name = self.name.as_str();
        let query = words.join(" ");
        let tier = if name == query {
            AppTier::Exact
        } else if name.starts_with(&query) {
            AppTier::Prefix
        } else if words.iter().all(|word| begins_word(name, word)) {
            AppTier::WordStarts
        } else if words_in_fields(words, &[name]) {
            AppTier::InName
        } else if words_in_fields(words, &[name, &self.generic_name]) {
            AppTier::InGenericName
        } else if words_in_fields(
            words,
            &[name, &self.generic_name, &self.keywords, &self.comment],
        ) {
            AppTier::InOtherFields
        } else if subsequence {
            return summed_span(words, &[name]).map(AppTier::Subsequence);
        } else {
            return None;
        };
        Some(tier)
    }
}

/// The entry's keywords for the current locale. GLib's localized list reader
/// cannot read a keyword holding an escaped `;` (`\;`), and the entry then
/// has no keywords at all: read the untranslated list from the file instead.
fn keywords(desktop: &gio::DesktopAppInfo) -> Vec<String> {
    let keywords = desktop.keywords();
    if !keywords.is_empty() || !desktop.has_key("Keywords") {
        return keywords.iter().map(ToString::to_string).collect();
    }
    let Some(path) = desktop.filename() else {
        return Vec::new();
    };
    let keyfile = glib::KeyFile::new();
    if keyfile
        .load_from_file(path, glib::KeyFileFlags::NONE)
        .is_err()
    {
        return Vec::new();
    }
    keyfile
        .string_list("Desktop Entry", "Keywords")
        .map(|keywords| keywords.iter().map(ToString::to_string).collect())
        .unwrap_or_default()
}

/// Whether `word` begins a word of `text`, a longest run of letters and
/// digits.
fn begins_word(text: &str, word: &str) -> bool {
    text.split(|character: char| !character.is_alphanumeric())
        .any(|text_word| text_word.starts_with(word))
}

/// The groups the apps matching a filter are listed in, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum AppTier {
    /// The name is the filter.
    Exact,
    /// The name begins with the filter.
    Prefix,
    /// Every word begins a word of the name.
    WordStarts,
    /// Every word occurs in the name.
    InName,
    /// Every word occurs in the name or the generic name.
    InGenericName,
    /// Some word needs the keywords or the comment.
    InOtherFields,
    /// Every word is a subsequence of the name; the summed span.
    Subsequence(usize),
}

impl AppTier {
    /// The group of matches the tier belongs to: the tier itself, or one
    /// group for every subsequence match, whatever its span.
    fn group(self) -> Self {
        match self {
            Self::Subsequence(_) => Self::Subsequence(0),
            tier => tier,
        }
    }
}

/// An app on the page.
#[derive(Debug)]
struct App {
    id: String,
    launches: usize,
    // The order of an unfiltered list.
    lowercase_name: String,
    // Every field, folded.
    all: AppText,
    // The fields `match-fields` lets the filter search.
    text: AppText,
    // The actions it can be started with, in the order of its `Actions`
    // key.
    actions: Vec<AppAction>,
}

impl App {
    /// The app of `desktop`, whose desktop id is `id`, launched `launches`
    /// times. Every field is searched until the list applies `match-fields`.
    fn new(id: String, desktop: &gio::DesktopAppInfo, launches: usize) -> Self {
        let all = AppText::new(desktop, MatchFields::ALL);
        Self {
            id,
            launches,
            lowercase_name: entries::app_name(desktop).to_lowercase(),
            text: all.clone(),
            all,
            actions: Vec::new(),
        }
    }

    /// The app with the actions `actions` of its desktop entry, each
    /// launched as often as `launches` says for its history key. An action
    /// the app could not start is left out: one without an Exec line,
    /// unless the app is started over the session bus.
    fn with_actions(
        mut self,
        name: &str,
        actions: &[DesktopAction],
        activated: bool,
        launches: impl Fn(&str) -> usize,
    ) -> Self {
        self.actions = actions
            .iter()
            .filter(|action| activated || action.exec.is_some())
            .map(|action| {
                let key = action_key(&self.id, &action.id);
                let launches = launches(&key);
                AppAction::new(key, name, action.clone(), &self.all, launches)
            })
            .collect();
        self
    }
}

/// The history and `hidden-apps` key of the action `action` of the app
/// with the desktop id `id`.
fn action_key(id: &str, action: &str) -> String {
    format!("{id}#{action}")
}

/// The text of an action's row: the app name, a colon and a space, then the
/// action name.
fn action_row_text(app_name: &str, action_name: &str) -> String {
    format!("{app_name}: {action_name}")
}

/// An action of an app on the page.
#[derive(Debug)]
struct AppAction {
    // `<desktop id>#<action id>`.
    key: String,
    launches: usize,
    // The action name, folded.
    name: String,
    // The row text without its colon, folded: the app name, a space and
    // the action name.
    colonless: String,
    // The row as the filter compares it: the row text in place of the
    // name, the app's other fields.
    all: AppText,
    text: AppText,
    action: DesktopAction,
}

impl AppAction {
    fn new(
        key: String,
        app_name: &str,
        action: DesktopAction,
        app_text: &AppText,
        launches: usize,
    ) -> Self {
        let all = AppText {
            name: fold_text(&action_row_text(app_name, &action.name)),
            ..app_text.clone()
        };
        Self {
            key,
            launches,
            name: fold_text(&action.name),
            colonless: fold_text(&format!("{app_name} {}", action.name)),
            text: all.clone(),
            all,
            action,
        }
    }

    /// The group the action's own row is listed in for the filter `words`
    /// (not empty), or `None` when it is not listed: when its row would not
    /// be, the row text taking the place of the app name, or when no word
    /// occurs in the action name that does not occur in the app name
    /// `app_name`. A filter that is the action name or the row text without
    /// its colon counts as the name itself, one that begins the latter as
    /// its beginning.
    fn tier(&self, app_name: &str, words: &[String], subsequence: bool) -> Option<AppTier> {
        let own_word = words
            .iter()
            .any(|word| self.name.contains(word.as_str()) && !app_name.contains(word.as_str()));
        if !own_word {
            return None;
        }
        let tier = self.text.tier(words, subsequence)?;
        let query = words.join(" ");
        let named = if query == self.name || query == self.colonless {
            Some(AppTier::Exact)
        } else if self.colonless.starts_with(&query) {
            Some(AppTier::Prefix)
        } else {
            None
        };
        Some(named.map_or(tier, |named| named.min(tier)))
    }
}

/// The settings that decide which apps are listed, how they match, and
/// which come first.
#[derive(Clone, Debug, PartialEq)]
struct AppSettings {
    subsequence: bool,
    fields: MatchFields,
    hidden: Vec<String>,
    // Desktop ids, in favorite order. Repeats and ids of apps not listed
    // may be among them.
    favorites: Vec<String>,
}

impl AppSettings {
    fn read(settings: &gio::Settings) -> Self {
        Self {
            subsequence: settings.boolean(FUZZY_MATCHING),
            fields: MatchFields::from_names(
                settings.strv(MATCH_FIELDS).iter().map(|name| name.as_str()),
            ),
            hidden: settings
                .strv(HIDDEN_APPS)
                .iter()
                .map(ToString::to_string)
                .collect(),
            favorites: settings
                .strv(FAVORITE_APPS)
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

/// The two ways Alt+Up and Alt+Down move a favorite.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Direction {
    Up,
    Down,
}

/// `ids` with every repeated id after its first entry left out, the others
/// in their order: how `favorite-apps` is always written.
fn without_repeats(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// The favorites `stored` after Ctrl+f on the app `id`: every entry of it
/// taken out when there is one, else `id` appended.
fn toggled_favorite(mut stored: Vec<String>, id: &str) -> Vec<String> {
    if stored.iter().any(|entry| entry == id) {
        stored.retain(|entry| entry != id);
    } else {
        stored.push(id.to_owned());
    }
    without_repeats(stored)
}

/// The favorites `stored` after moving the favorite `id` in `direction`:
/// `id` placed immediately in front of the first entry of the favorite shown
/// above it, or immediately behind that of the favorite shown below it.
/// `shown` tells whether an id's app is listed. `None` when there is nothing
/// to move: `id` is not a favorite shown, or the first or last one.
fn moved_favorite(
    stored: Vec<String>,
    id: &str,
    direction: Direction,
    shown: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let mut favorites = without_repeats(stored);
    let order: Vec<&String> = favorites.iter().filter(|entry| shown(entry)).collect();
    let position = order.iter().position(|entry| *entry == id)?;
    let neighbour = match direction {
        Direction::Up => order.get(position.checked_sub(1)?)?,
        Direction::Down => order.get(position + 1)?,
    };
    let neighbour = (*neighbour).clone();
    favorites.retain(|entry| entry != id);
    let at = favorites.iter().position(|entry| *entry == neighbour)?;
    let at = match direction {
        Direction::Up => at,
        Direction::Down => at + 1,
    };
    favorites.insert(at, id.to_owned());
    Some(favorites)
}

/// A row of the Applications page: the index of its app, and for an action
/// row the index of the action among the app's.
type Row = (usize, Option<usize>);

/// What a filter text lists: the groups of the apps, and of the actions
/// listed on their own.
#[derive(Debug, Default)]
struct Ranking {
    // The words the apps are matched with: those of the filter text, or of
    // its part before the first colon. `None` without words.
    words: Option<Vec<String>>,
    // The words of the part after the first colon; `None` when the filter
    // text has no colon. A filter with a colon lists action rows only.
    action_words: Option<Vec<String>>,
    // The group of each app for `words`; `None` without words.
    tiers: Option<Vec<Option<AppTier>>>,
    // The group of each action's own row, by app; empty unless the filter
    // has words and no colon.
    action_tiers: Vec<Vec<Option<AppTier>>>,
}

impl Ranking {
    fn new(apps: &[App], text: &str, subsequence: bool) -> Self {
        let (words, action_words) = match text.split_once(':') {
            Some((app, action)) => (query_words(app), Some(query_words(action))),
            None => (query_words(text), None),
        };
        let mut ranking = Self {
            words: (!words.is_empty()).then_some(words),
            action_words,
            ..Self::default()
        };
        ranking.regroup(apps, subsequence);
        ranking
    }

    fn regroup(&mut self, apps: &[App], subsequence: bool) {
        self.tiers = None;
        self.action_tiers = Vec::new();
        if self.words.is_some() {
            self.tiers = Some(Vec::with_capacity(apps.len()));
            for app in apps {
                self.push(app, subsequence);
            }
        }
    }

    /// Groups an app added after the others.
    fn push(&mut self, app: &App, subsequence: bool) {
        let Some(words) = self.words.as_ref() else {
            return;
        };
        if let Some(tiers) = self.tiers.as_mut() {
            tiers.push(app.text.tier(words, subsequence));
        }
        if self.action_words.is_none() {
            self.action_tiers.push(
                app.actions
                    .iter()
                    .map(|action| action.tier(&app.all.name, words, subsequence))
                    .collect(),
            );
        }
    }

    fn app_tier(&self, index: usize) -> Option<Option<AppTier>> {
        self.tiers.as_ref().map(|tiers| tiers[index])
    }

    fn action_tier(&self, index: usize, action: usize) -> Option<AppTier> {
        self.action_tiers
            .get(index)
            .and_then(|tiers| tiers.get(action).copied())
            .flatten()
    }
}

/// The apps on the page, by the INDEX of their rows, which of them and of
/// their actions are hidden, which are favorites, and the group of each row
/// for the filter text.
struct AppList {
    apps: RefCell<Vec<App>>,
    settings: RefCell<AppSettings>,
    hidden: RefCell<Vec<bool>>,
    // The place of each favorite in the favorite order, `None` for the
    // other apps.
    favorites: RefCell<Vec<Option<usize>>>,
    // By app, whether each of its actions is hidden.
    hidden_actions: RefCell<Vec<Vec<bool>>>,
    ranking: RefCell<Ranking>,
    // The app whose actions are shown beneath its row, if any: the rows of
    // its actions are not listed on their own meanwhile.
    open: Cell<Option<usize>>,
    // Typed while an app's actions are shown: those beneath it are the
    // ones whose names hold every word of it. Empty whenever they open.
    narrowing: RefCell<String>,
}

impl AppList {
    fn new(apps: Vec<App>, settings: AppSettings) -> Self {
        let list = Self {
            hidden: RefCell::new(vec![false; apps.len()]),
            favorites: RefCell::new(vec![None; apps.len()]),
            hidden_actions: RefCell::default(),
            apps: RefCell::new(apps),
            settings: RefCell::new(settings.clone()),
            ranking: RefCell::default(),
            open: Cell::new(None),
            narrowing: RefCell::default(),
        };
        list.update(&settings, true);
        list
    }

    fn id(&self, index: usize) -> String {
        self.apps.borrow()[index].id.clone()
    }

    fn len(&self) -> usize {
        self.apps.borrow().len()
    }

    /// The history key of a row: the app's desktop id, or the action's key.
    fn key(&self, (index, action): Row) -> String {
        let apps = self.apps.borrow();
        match action {
            Some(action) => apps[index].actions[action].key.clone(),
            None => apps[index].id.clone(),
        }
    }

    /// The action `action` of the app with the index `index`.
    fn action(&self, index: usize, action: usize) -> Option<DesktopAction> {
        let apps = self.apps.borrow();
        Some(apps.get(index)?.actions.get(action)?.action.clone())
    }

    /// The indexes of the actions of the app with the index `index` that
    /// are not hidden, in their order.
    fn shown_actions(&self, index: usize) -> Vec<usize> {
        let hidden = self.hidden_actions.borrow();
        hidden.get(index).map_or_else(Vec::new, |hidden| {
            (0..hidden.len())
                .filter(|&action| !hidden[action])
                .collect()
        })
    }

    /// Adds an app after the others, matched, hidden and grouped as they
    /// are, and returns its index.
    fn push(&self, mut app: App) -> usize {
        let settings = self.settings.borrow();
        app.text = app.all.only(settings.fields);
        for action in &mut app.actions {
            action.text = action.all.only(settings.fields);
        }
        self.hidden
            .borrow_mut()
            .push(is_hidden(&app.id, &settings.hidden));
        self.favorites
            .borrow_mut()
            .push(favorite_place(&app.id, &settings.favorites));
        self.hidden_actions.borrow_mut().push(
            app.actions
                .iter()
                .map(|action| is_hidden(&action.key, &settings.hidden))
                .collect(),
        );
        self.ranking.borrow_mut().push(&app, settings.subsequence);
        let mut apps = self.apps.borrow_mut();
        apps.push(app);
        apps.len() - 1
    }

    /// The index of the app with the desktop id `id`.
    fn index_of(&self, id: &str) -> Option<usize> {
        self.apps.borrow().iter().position(|app| app.id == id)
    }

    /// Whether the app with the index `index` is a favorite.
    fn is_favorite(&self, index: usize) -> bool {
        self.favorites
            .borrow()
            .get(index)
            .is_some_and(Option::is_some)
    }

    /// Whether the app with the desktop id `id` is listed while the filter
    /// text has no words: it is on the page and not hidden.
    fn is_shown(&self, id: &str) -> bool {
        self.index_of(id)
            .is_some_and(|index| !self.hidden.borrow()[index])
    }

    /// Whether the filter text has words, or a colon: the rows are not
    /// those of an empty filter.
    fn has_words(&self) -> bool {
        let ranking = self.ranking.borrow();
        ranking.words.is_some() || ranking.action_words.is_some()
    }

    /// The row whose history key is `key`.
    fn row_of(&self, key: &str) -> Option<Row> {
        let apps = self.apps.borrow();
        apps.iter().enumerate().find_map(|(index, app)| {
            if app.id == key {
                return Some((index, None));
            }
            let action = app.actions.iter().position(|action| action.key == key)?;
            Some((index, Some(action)))
        })
    }

    fn settings(&self) -> AppSettings {
        self.settings.borrow().clone()
    }

    /// Takes new settings; whether they differ from those applied. The apps
    /// are then grouped again for the same filter text.
    fn apply_settings(&self, settings: AppSettings) -> bool {
        let old = self.settings.replace(settings.clone());
        if old == settings {
            return false;
        }
        self.update(&settings, old.fields != settings.fields);
        self.regroup();
        true
    }

    fn update(&self, settings: &AppSettings, fields_changed: bool) {
        let apps = &mut *self.apps.borrow_mut();
        if fields_changed {
            for app in apps.iter_mut() {
                app.text = app.all.only(settings.fields);
                for action in &mut app.actions {
                    action.text = action.all.only(settings.fields);
                }
            }
        }
        *self.hidden.borrow_mut() = apps
            .iter()
            .map(|app| is_hidden(&app.id, &settings.hidden))
            .collect();
        *self.favorites.borrow_mut() = apps
            .iter()
            .map(|app| favorite_place(&app.id, &settings.favorites))
            .collect();
        *self.hidden_actions.borrow_mut() = apps
            .iter()
            .map(|app| {
                app.actions
                    .iter()
                    .map(|action| is_hidden(&action.key, &settings.hidden))
                    .collect()
            })
            .collect();
    }

    /// Groups the apps and actions for the filter text `text`. Text of white
    /// space alone has no words and lists the apps as an empty filter does.
    fn rank(&self, text: &str) {
        let ranking = Ranking::new(
            &self.apps.borrow(),
            text,
            self.settings.borrow().subsequence,
        );
        self.ranking.replace(ranking);
    }

    fn regroup(&self) {
        let subsequence = self.settings.borrow().subsequence;
        self.ranking
            .borrow_mut()
            .regroup(&self.apps.borrow(), subsequence);
    }

    /// Whether the row of the app with the index `index` is listed.
    fn is_listed(&self, index: usize) -> bool {
        self.is_row_listed(&self.ranking.borrow(), (index, None), self.open.get())
    }

    /// Whether the row of the action `action` of the app with the index
    /// `index`, on its own, is listed.
    fn is_action_listed(&self, index: usize, action: usize) -> bool {
        self.is_row_listed(
            &self.ranking.borrow(),
            (index, Some(action)),
            self.open.get(),
        )
    }

    /// Whether the action `action` of the app with the index `index` is
    /// shown beneath the app's row when its actions are.
    fn is_shown_beneath(&self, index: usize, action: usize) -> bool {
        self.hidden_actions
            .borrow()
            .get(index)
            .and_then(|hidden| hidden.get(action))
            .is_some_and(|hidden| !hidden)
    }

    /// Shows the actions of the app with the index `index` beneath its
    /// row, or none with `None`; either way with no narrowing text.
    fn set_open(&self, index: Option<usize>) {
        self.open.set(index);
        self.narrowing.borrow_mut().clear();
    }

    /// Whether the action `action` of the app with the index `index`,
    /// shown beneath the app's row, holds every word of the narrowing text
    /// in its name. Only the app whose actions are shown has one.
    fn passes_narrowing(&self, index: usize, action: usize) -> bool {
        if self.open.get() != Some(index) {
            return true;
        }
        let words = query_words(&self.narrowing.borrow());
        self.apps.borrow()[index]
            .actions
            .get(action)
            .is_some_and(|action| contains_all_words(&action.name, &words))
    }

    fn is_row_listed(&self, ranking: &Ranking, (index, action): Row, open: Option<usize>) -> bool {
        if self.hidden.borrow().get(index).copied().unwrap_or(true) {
            return false;
        }
        let app_listed = ranking.app_tier(index).is_none_or(|tier| tier.is_some());
        let Some(action) = action else {
            return ranking.action_words.is_none() && app_listed;
        };
        if !self.is_shown_beneath(index, action) || open == Some(index) {
            return false;
        }
        match ranking.action_words.as_deref() {
            Some(words) => {
                app_listed
                    && self.apps.borrow()[index]
                        .actions
                        .get(action)
                        .is_some_and(|action| contains_all_words(&action.name, words))
            }
            None => ranking.action_tier(index, action).is_some(),
        }
    }

    /// The order of two apps: without a filter, the favorites first in
    /// their order, then launches, then the name; with one, the group, then
    /// the favorites first in their order, launches, the folded name, and
    /// the desktop id.
    #[cfg(test)]
    fn compare(&self, a: usize, b: usize) -> Ordering {
        self.compare_rows((a, None), (b, None))
    }

    /// The order of two rows listed at the top level (see [`compare_rows`]).
    fn compare_rows(&self, a: Row, b: Row) -> Ordering {
        compare_rows(
            &self.apps.borrow(),
            &self.favorites.borrow(),
            &self.ranking.borrow(),
            a,
            b,
        )
    }

    /// The index of the app listed first for the filter text `text`, with
    /// the settings applied now, whatever the filter text is now; `None`
    /// when no app is listed for it.
    #[cfg(test)]
    fn best_match(&self, text: &str) -> Option<usize> {
        self.best_row(text)
            .and_then(|(index, action)| action.is_none().then_some(index))
    }

    /// The row listed first for the filter text `text`, with the settings
    /// applied now, whatever the filter text is now; `None` when no row is
    /// listed for it.
    fn best_row(&self, text: &str) -> Option<Row> {
        let apps = self.apps.borrow();
        let ranking = Ranking::new(&apps, text, self.settings.borrow().subsequence);
        let rows = apps.iter().enumerate().flat_map(|(index, app)| {
            std::iter::once((index, None))
                .chain((0..app.actions.len()).map(move |action| (index, Some(action))))
        });
        let favorites = self.favorites.borrow();
        rows.filter(|&row| self.is_row_listed(&ranking, row, None))
            .min_by(|&a, &b| compare_rows(&apps, &favorites, &ranking, a, b))
    }
}

/// The order of two top-level rows for the groups `ranking` holds, with
/// the places of the apps in the favorite order in `favorites`. Without a
/// filter, apps go as [`compare_apps`] puts them: favorites first, then by
/// launches and name. With one, apps and the actions listed on their own go
/// by group, then the favorites' apps first in their order, then by exact
/// group, launches, folded row text and history key; an action's own row is
/// never a favorite's. With a colon, the actions go by the order of their
/// apps for the part before it, then by the order of their app's `Actions`
/// key. Rows a filter does not list are put somewhere too, consistently.
fn compare_rows(
    apps: &[App],
    favorites: &[Option<usize>],
    ranking: &Ranking,
    a: Row,
    b: Row,
) -> Ordering {
    let order = if ranking.action_words.is_some() || ranking.tiers.is_none() {
        a.1.is_some()
            .cmp(&b.1.is_some())
            .then_with(|| compare_apps(apps, favorites, ranking.tiers.as_deref(), a.0, b.0))
            // Apps that tie (same name, no filter words) keep their actions
            // together.
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    } else {
        let key = |(index, action): Row| {
            let app = &apps[index];
            match action {
                None => (
                    ranking.app_tier(index).flatten(),
                    favorites[index],
                    app.launches,
                    app.text.name.as_str(),
                    app.id.as_str(),
                ),
                Some(action) => {
                    let action_row = &app.actions[action];
                    (
                        ranking.action_tier(index, action),
                        None,
                        action_row.launches,
                        action_row.text.name.as_str(),
                        action_row.key.as_str(),
                    )
                }
            }
        };
        let (a_key, b_key) = (key(a), key(b));
        // All subsequence matches are one group: the favorites lead it
        // whatever their span.
        a_key
            .0
            .is_none()
            .cmp(&b_key.0.is_none())
            .then(
                a_key
                    .0
                    .map(AppTier::group)
                    .cmp(&b_key.0.map(AppTier::group)),
            )
            .then(favorite_order(a_key.1, b_key.1))
            .then(a_key.0.cmp(&b_key.0))
            .then(b_key.2.cmp(&a_key.2))
            .then_with(|| a_key.3.cmp(b_key.3))
            .then_with(|| a_key.4.cmp(b_key.4))
    };
    order.then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
}

/// The order of two places in the favorite order: favorites first, in
/// their order.
fn favorite_order(a: Option<usize>, b: Option<usize>) -> Ordering {
    match (a, b) {
        (Some(place_a), Some(place_b)) => place_a.cmp(&place_b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// The place of the desktop id `id` in the favorite order `favorites`: that
/// of its first entry, or `None` when it has none.
fn favorite_place(id: &str, favorites: &[String]) -> Option<usize> {
    favorites.iter().position(|entry| entry == id)
}

/// The order of the apps with the indexes `a` and `b`, whose places in the
/// favorite order are in `favorites`, for the groups `tiers` of the filter
/// text (`None` while it is empty). See [`AppList::compare`].
fn compare_apps(
    apps: &[App],
    favorites: &[Option<usize>],
    tiers: Option<&[Option<AppTier>]>,
    a: usize,
    b: usize,
) -> Ordering {
    let (app_a, app_b) = (&apps[a], &apps[b]);
    // Favorites first, in their order; launches never reorder them.
    let favorite_order = || favorite_order(favorites[a], favorites[b]);
    match tiers {
        None => favorite_order()
            .then(app_b.launches.cmp(&app_a.launches))
            .then_with(|| app_a.lowercase_name.cmp(&app_b.lowercase_name)),
        // All subsequence matches are one group: the favorites lead it
        // whatever their span.
        Some(tiers) => tiers[a]
            .map(AppTier::group)
            .cmp(&tiers[b].map(AppTier::group))
            .then_with(favorite_order)
            .then(tiers[a].cmp(&tiers[b]))
            .then(app_b.launches.cmp(&app_a.launches))
            .then_with(|| app_a.text.name.cmp(&app_b.text.name))
            .then_with(|| app_a.id.cmp(&app_b.id)),
    }
}

/// Starts the entry as its desktop file describes it, once per run (a click
/// can activate a row twice), and closes as soon as it has started or
/// failed. A start that succeeded is recorded in the launch history.
fn execute_app(
    desktop: &gio::DesktopAppInfo,
    settings: &gio::Settings,
    history: &Rc<RefCell<Vec<String>>>,
    launched: &Cell<bool>,
    quit: &Rc<dyn Fn()>,
) {
    execute_entry(desktop, None, settings, history, launched, quit);
}

/// Starts the entry, or its action `action`, as [`execute_app`] starts the
/// entry. An action's start is recorded under its own key,
/// `<desktop id>#<action id>`.
fn execute_entry(
    desktop: &gio::DesktopAppInfo,
    action: Option<&DesktopAction>,
    settings: &gio::Settings,
    history: &Rc<RefCell<Vec<String>>>,
    launched: &Cell<bool>,
    quit: &Rc<dyn Fn()>,
) {
    if launched.replace(true) {
        return;
    }
    let id = desktop.id().map(|id| id.to_string()).unwrap_or_default();
    let key = match action {
        Some(action) => action_key(&id, &action.id),
        None => id,
    };
    let (settings, history, quit) = (settings.clone(), history.clone(), quit.clone());
    let done = move |result: Result<(), String>| {
        match result {
            Ok(()) => record_launch(&settings, &mut history.borrow_mut(), key),
            Err(message) => eprintln!("{message}"),
        }
        quit();
    };
    match action {
        Some(action) => launch_desktop_action(desktop, action, done),
        None => launch_desktop_app(desktop, done),
    }
}

/// Appends `entry` to the launch `history` and stores its newest
/// `HISTORY_MAX_LEN` entries, the new one last.
pub fn record_launch(settings: &gio::Settings, history: &mut Vec<String>, entry: String) {
    push_capped(history, entry);
    let values: Vec<&str> = history.iter().map(String::as_str).collect();
    if settings.set_strv(LAUNCH_HISTORY, values).is_ok() {
        // The launcher exits right after a launch.
        gio::Settings::sync();
    }
}

/// Appends `entry` to `history`, dropping its oldest entries beyond
/// `HISTORY_MAX_LEN`.
fn push_capped(history: &mut Vec<String>, entry: String) {
    history.push(entry);
    let excess = history.len().saturating_sub(HISTORY_MAX_LEN);
    history.drain(..excess);
}

fn load_icons_in_background(model: &gtk::ListStore, icon_size: i32) {
    let mut apps = VecDeque::new();
    if let Some(iter) = model.iter_first() {
        loop {
            if let Some(app) = app_value(model, &iter) {
                apps.push_back(app);
            }
            if !model.iter_next(&iter) {
                break;
            }
        }
    }
    let apps = Rc::new(RefCell::new(apps));
    let model = model.clone();
    let Some(theme) = gtk::IconTheme::default() else {
        return;
    };
    glib::idle_add_local(move || {
        let Some(desktop) = apps.borrow_mut().pop_front() else {
            return glib::ControlFlow::Break;
        };
        if let Some(icon) = load_icon(&theme, &desktop, icon_size) {
            let id = desktop.id();
            if let Some(iter) = model.iter_first() {
                loop {
                    // A page file may list an app more than once.
                    if app_value(&model, &iter).and_then(|app| app.id()) == id {
                        model.set(&iter, &[(ICON, &icon)]);
                    }
                    if !model.iter_next(&iter) {
                        break;
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

fn load_icon(theme: &gtk::IconTheme, app: &gio::DesktopAppInfo, icon_size: i32) -> Option<Pixbuf> {
    let icon_name = gio::prelude::IconExt::to_string(&app.icon()?)?;
    if let Some(info) = theme.lookup_icon(&icon_name, icon_size, gtk::IconLookupFlags::FORCE_SIZE) {
        match info.load_icon() {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error: load_icon failed: {}", error.message()),
        }
    }
    if Path::new(&icon_name).exists() {
        match Pixbuf::from_file_at_size(&icon_name, icon_size, icon_size) {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error loading icon: {}", error.message()),
        }
    }
    None
}

/// Decodes the Applications page's icons on GTK's worker threads, a few at
/// a time, so that neither the PNG inflate nor the SVG render runs on the
/// main thread between keystrokes.
struct IconLoader {
    model: gtk::TreeStore,
    theme: gtk::IconTheme,
    icon_size: i32,
    queue: RefCell<VecDeque<gtk::TreeIter>>,
    // Icons being decoded.
    loading: Cell<usize>,
}

impl IconLoader {
    /// Queues every row, the ones visible through the filter first and in
    /// the order shown, so the visible rows get their icons first and rows
    /// hidden by text typed before the list arrived still get theirs.
    fn start(
        model: &gtk::TreeStore,
        filter: &gtk::TreeModelFilter,
        icon_size: i32,
    ) -> Option<Rc<Self>> {
        let theme = gtk::IconTheme::default()?;
        let mut visible = Vec::new();
        if let Some(iter) = filter.iter_first() {
            loop {
                let child = filter.convert_iter_to_child_iter(&iter);
                if let Some(path) = model.path(&child) {
                    visible.extend(path.indices().first().map(|&index| index as usize));
                }
                if !filter.iter_next(&iter) {
                    break;
                }
            }
        }
        let mut rows = Vec::new();
        if let Some(iter) = model.iter_first() {
            loop {
                rows.push(iter);
                if !model.iter_next(&iter) {
                    break;
                }
            }
        }
        let queue = icon_queue_order(&visible, rows.len())
            .into_iter()
            .map(|index| rows[index])
            .collect();
        let loader = Rc::new(Self {
            model: model.clone(),
            theme,
            icon_size,
            queue: RefCell::new(queue),
            loading: Cell::new(0),
        });
        for _ in 0..ICON_LOAD_PARALLELISM {
            loader.load_next();
        }
        Some(loader)
    }

    /// Queues the icon of a row added later.
    fn enqueue(self: &Rc<Self>, iter: gtk::TreeIter) {
        self.queue.borrow_mut().push_back(iter);
        if self.loading.get() < ICON_LOAD_PARALLELISM {
            self.load_next();
        }
    }

    /// Starts decoding the next queued icon that the theme can find.
    fn load_next(self: &Rc<Self>) {
        loop {
            let Some(iter) = self.queue.borrow_mut().pop_front() else {
                return;
            };
            // An action's row shows no icon.
            if action_value(&self.model, &iter).is_some() {
                continue;
            }
            let Some(info) = app_value(&self.model, &iter).and_then(|app| self.lookup(&app)) else {
                continue;
            };
            self.loading.set(self.loading.get() + 1);
            let loader = self.clone();
            info.load_icon_async(None::<&gio::Cancellable>, move |result| {
                match result {
                    // GtkTreeStore iterators persist across re-sorting and
                    // apps' rows are never removed.
                    Ok(pixbuf) => loader.model.set(&iter, &[(ICON, &pixbuf)]),
                    Err(error) => glib::g_debug!("ilia", "Icon not loaded: {}", error.message()),
                }
                loader.loading.set(loader.loading.get() - 1);
                loader.load_next();
            });
            return;
        }
    }

    fn lookup(&self, app: &gio::DesktopAppInfo) -> Option<gtk::IconInfo> {
        let icon = app.icon()?;
        let flags = gtk::IconLookupFlags::FORCE_SIZE;
        // Themed icons go by their plain name: looking up the GIcon also
        // tries the "-symbolic" variant GLib adds to the icon's names, which
        // the theme chain can find (grey, in Adwaita) before the colour icon.
        if icon.is::<gio::ThemedIcon>() {
            let name = gio::prelude::IconExt::to_string(&icon)?;
            self.theme.lookup_icon(name.as_str(), self.icon_size, flags)
        } else {
            self.theme.lookup_by_gicon(&icon, self.icon_size, flags)
        }
    }
}

/// The order icons are loaded in: the rows shown, in the order shown, then
/// every other row in model order. `visible` holds model row indices.
fn icon_queue_order(visible: &[usize], row_count: usize) -> Vec<usize> {
    let mut queued = vec![false; row_count];
    let mut order = Vec::with_capacity(row_count);
    for &index in visible {
        if index < row_count && !queued[index] {
            queued[index] = true;
            order.push(index);
        }
    }
    order.extend((0..row_count).filter(|&index| !queued[index]));
    order
}

#[cfg(test)]
mod tests {
    use super::{
        action_key, append_last, fits_pattern, icon_queue_order, is_hidden, moved_favorite,
        push_capped, remove_last, toggled_favorite, without_repeats, App, AppAction, AppList,
        AppSettings, AppText, AppTier, Direction, MatchFields,
    };
    use crate::entries::DesktopAction;
    use crate::filter::{fold_text, query_words};

    const ALL_FIELDS: MatchFields = MatchFields {
        generic_name: true,
        keywords: true,
        comment: true,
    };

    fn desktop_entry(lines: &str) -> gio::DesktopAppInfo {
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(
                &format!("[Desktop Entry]\nType=Application\nName=Fixture\nExec=true\n{lines}"),
                glib::KeyFileFlags::NONE,
            )
            .unwrap();
        gio::DesktopAppInfo::from_keyfile(&keyfile).unwrap()
    }

    // An app's text, folded as the page folds it.
    fn text(name: &str, generic_name: &str, keywords: &str, comment: &str) -> AppText {
        AppText {
            name: fold_text(name),
            generic_name: fold_text(generic_name),
            keywords: fold_text(keywords),
            comment: fold_text(comment),
        }
    }

    fn named(name: &str) -> AppText {
        text(name, "", "", "")
    }

    fn tier(text: &AppText, query: &str, subsequence: bool) -> Option<AppTier> {
        text.tier(&query_words(query), subsequence)
    }

    fn app(id: &str, launches: usize, text: AppText) -> App {
        App {
            id: id.to_owned(),
            launches,
            lowercase_name: text.name.clone(),
            all: text.clone(),
            text,
            actions: Vec::new(),
        }
    }

    fn settings(subsequence: bool, hidden: &[&str]) -> AppSettings {
        AppSettings {
            subsequence,
            fields: ALL_FIELDS,
            hidden: hidden.iter().map(|entry| entry.to_string()).collect(),
            favorites: Vec::new(),
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn with_favorites(settings: AppSettings, favorites: &[&str]) -> AppSettings {
        AppSettings {
            favorites: ids(favorites),
            ..settings
        }
    }

    // The ids of the apps `list` lists, in order.
    fn rows(list: &AppList) -> Vec<String> {
        let count = list.apps.borrow().len();
        let mut rows: Vec<usize> = (0..count).filter(|&index| list.is_listed(index)).collect();
        rows.sort_by(|&a, &b| list.compare(a, b));
        rows.into_iter().map(|index| list.id(index)).collect()
    }

    // The ids of the apps listed for `query`, in order.
    fn listed(apps: Vec<App>, query: &str, subsequence: bool) -> Vec<String> {
        let list = AppList::new(apps, settings(subsequence, &[]));
        list.rank(query);
        rows(&list)
    }

    // `app` with the actions `actions`, (id, name) pairs, each launched
    // as often as `launches` says for its key.
    fn with_actions(
        mut app: App,
        name: &str,
        actions: &[(&str, &str)],
        launches: &[(&str, usize)],
    ) -> App {
        app.actions = actions
            .iter()
            .map(|(id, action_name)| {
                let key = action_key(&app.id, id);
                let count = launches
                    .iter()
                    .find(|(launched, _)| *launched == key)
                    .map_or(0, |(_, count)| *count);
                let action = DesktopAction {
                    id: id.to_string(),
                    name: action_name.to_string(),
                    exec: Some("probe".to_owned()),
                };
                AppAction::new(key, name, action, &app.all, count)
            })
            .collect();
        app
    }

    // The history keys of the top-level rows `list` lists, in order.
    fn top_rows(list: &AppList) -> Vec<String> {
        let apps = list.apps.borrow();
        let mut rows: Vec<(usize, Option<usize>)> = apps
            .iter()
            .enumerate()
            .flat_map(|(index, app)| {
                std::iter::once((index, None))
                    .chain((0..app.actions.len()).map(move |action| (index, Some(action))))
            })
            .collect();
        drop(apps);
        rows.retain(|&(index, action)| match action {
            None => list.is_listed(index),
            Some(action) => list.is_action_listed(index, action),
        });
        rows.sort_by(|&a, &b| list.compare_rows(a, b));
        rows.into_iter().map(|row| list.key(row)).collect()
    }

    fn fixture_apps() -> Vec<App> {
        vec![
            with_actions(
                app(
                    "browser.desktop",
                    0,
                    text(
                        "Quokka Browser",
                        "Web Browser",
                        "internet surf",
                        "Private browsing for everyone",
                    ),
                ),
                "Quokka Browser",
                &[
                    ("new-window", "New Window"),
                    ("private-window", "Private Window"),
                    ("profiles", "Profile Manager"),
                ],
                &[("browser.desktop#new-window", 3)],
            ),
            with_actions(
                app("kf.desktop", 0, text("KF Terminal", "", "shell", "")),
                "KF Terminal",
                &[("new-tab", "New Tab"), ("prefs", "Preferences")],
                &[],
            ),
            with_actions(
                app("ux.desktop", 0, named("UX Term")),
                "UX Term",
                &[("open", "Open UX Term Session")],
                &[],
            ),
            app("wmt.desktop", 0, named("Window Maker Tools")),
        ]
    }

    fn fixture_list(subsequence: bool, hidden: &[&str]) -> AppList {
        let list = AppList::new(Vec::new(), settings(subsequence, hidden));
        for app in fixture_apps() {
            list.push(app);
        }
        list
    }

    fn listed_for(list: &AppList, query: &str) -> Vec<String> {
        list.rank(query);
        top_rows(list)
    }

    #[test]
    fn an_empty_filter_lists_only_the_apps() {
        let list = fixture_list(false, &[]);
        // By name: "KF Terminal" before "Quokka Browser".
        assert_eq!(
            listed_for(&list, ""),
            ["kf.desktop", "browser.desktop", "ux.desktop", "wmt.desktop"]
        );
        assert_eq!(listed_for(&list, " \t"), listed_for(&list, ""));
    }

    #[test]
    fn an_action_row_needs_a_word_of_its_own() {
        let list = fixture_list(false, &[]);
        assert_eq!(listed_for(&list, "kf"), ["kf.desktop"]);
        assert_eq!(listed_for(&list, "ux term"), ["ux.desktop"]);
        assert_eq!(listed_for(&list, "term new"), ["kf.desktop#new-tab"]);
        // The app's other fields still count for the row.
        assert_eq!(
            listed_for(&list, "surf manager"),
            ["browser.desktop#profiles"]
        );
        assert_eq!(
            listed_for(&list, "private"),
            ["browser.desktop#private-window", "browser.desktop"]
        );
    }

    #[test]
    fn action_rows_rank_among_the_apps() {
        let list = fixture_list(false, &[]);
        // The name prefix ranks first; then the action launched more.
        assert_eq!(
            listed_for(&list, "win"),
            [
                "wmt.desktop",
                "browser.desktop#new-window",
                "browser.desktop#private-window"
            ]
        );
        // The action name, or the row text without its colon, is the name.
        assert_eq!(
            listed_for(&list, "Private Window"),
            [
                "browser.desktop#private-window",
                "browser.desktop#new-window"
            ]
        );
        assert_eq!(
            listed_for(&list, "quokka browser private window")[0],
            "browser.desktop#private-window"
        );
        // Its beginning ranks as a prefix of the name.
        assert_eq!(
            listed_for(&list, "quokka browser private"),
            ["browser.desktop#private-window", "browser.desktop"]
        );
    }

    #[test]
    fn subsequences_never_stand_for_the_word_of_its_own() {
        let list = fixture_list(true, &[]);
        assert_eq!(
            listed_for(&list, "qkka manager"),
            ["browser.desktop#profiles"]
        );
        assert!(listed_for(&list, "qkka mangr").is_empty());
    }

    #[test]
    fn a_colon_lists_actions_grouped_by_their_apps() {
        let list = fixture_list(false, &[]);
        // In the apps' unfiltered order; an action's launches change nothing.
        assert_eq!(
            listed_for(&list, ":"),
            [
                "kf.desktop#new-tab",
                "kf.desktop#prefs",
                "browser.desktop#new-window",
                "browser.desktop#private-window",
                "browser.desktop#profiles",
                "ux.desktop#open",
            ]
        );
        assert_eq!(
            listed_for(&list, "term:"),
            ["kf.desktop#new-tab", "kf.desktop#prefs", "ux.desktop#open"]
        );
        assert_eq!(listed_for(&list, "  TERM  :  PREF  "), ["kf.desktop#prefs"]);
        assert!(listed_for(&list, "quokka::").is_empty());
        // No subsequences after the colon, whatever the setting.
        let fuzzy = fixture_list(true, &[]);
        assert_eq!(
            listed_for(&fuzzy, "qkka:priv"),
            ["browser.desktop#private-window"]
        );
        assert!(listed_for(&fuzzy, "qkka:prvt").is_empty());
    }

    #[test]
    fn hidden_actions_and_apps_list_no_action_rows() {
        let list = fixture_list(
            false,
            &["browser.desktop#new-?indow", "kf.desktop#*", "ux.desktop"],
        );
        assert_eq!(
            listed_for(&list, "quokka:"),
            ["browser.desktop#private-window", "browser.desktop#profiles"]
        );
        assert!(listed_for(&list, "kf:").is_empty());
        assert_eq!(listed_for(&list, "kf"), ["kf.desktop"]);
        assert!(listed_for(&list, "ux:").is_empty());
        assert_eq!(list.shown_actions(0), [1, 2]);
        assert!(list.shown_actions(1).is_empty());
    }

    #[test]
    fn open_actions_are_not_listed_on_their_own() {
        let list = fixture_list(false, &[]);
        list.open.set(Some(0));
        assert_eq!(listed_for(&list, "quokka private"), ["browser.desktop"]);
        list.open.set(None);
        assert_eq!(
            listed_for(&list, "quokka private"),
            ["browser.desktop#private-window", "browser.desktop"]
        );
    }

    #[test]
    fn narrowing_keeps_the_open_actions_holding_every_word() {
        let list = fixture_list(false, &[]);
        let beneath = |index: usize| -> Vec<usize> {
            (0..list.apps.borrow()[index].actions.len())
                .filter(|&action| list.passes_narrowing(index, action))
                .collect()
        };
        list.set_open(Some(0));
        assert_eq!(beneath(0), [0, 1, 2]);
        *list.narrowing.borrow_mut() = "WINDOW new".to_owned();
        assert_eq!(beneath(0), [0]);
        *list.narrowing.borrow_mut() = "zzz".to_owned();
        assert!(beneath(0).is_empty());
        // Only the open app is narrowed, and opening again starts afresh.
        assert_eq!(beneath(1), [0, 1]);
        list.set_open(Some(1));
        assert_eq!(beneath(1), [0, 1]);
        assert_eq!(beneath(0), [0, 1, 2]);
    }

    #[test]
    fn an_early_enter_may_start_an_action() {
        let list = fixture_list(false, &[]);
        assert_eq!(list.best_row("term:pref"), Some((1, Some(1))));
        assert_eq!(list.best_row("private"), Some((0, Some(1))));
        assert_eq!(list.best_row("kf"), Some((1, None)));
    }

    #[test]
    fn a_colon_keeps_the_actions_of_apps_that_tie_together() {
        let list = AppList::new(Vec::new(), settings(false, &[]));
        for id in ["a.desktop", "b.desktop"] {
            list.push(with_actions(
                app(id, 0, named("Terminal")),
                "Terminal",
                &[("new", "New Window"), ("prefs", "Preferences")],
                &[],
            ));
        }
        assert_eq!(
            listed_for(&list, ":"),
            [
                "a.desktop#new",
                "a.desktop#prefs",
                "b.desktop#new",
                "b.desktop#prefs"
            ]
        );
    }

    #[test]
    fn favorites_order_the_action_groups_but_never_lift_an_action_row() {
        let favorite = |id: &str| {
            let list = AppList::new(Vec::new(), with_favorites(settings(false, &[]), &[id]));
            for app in fixture_apps() {
                list.push(app);
            }
            list
        };
        let list = favorite("ux.desktop");
        assert_eq!(
            listed_for(&list, ""),
            ["ux.desktop", "kf.desktop", "browser.desktop", "wmt.desktop"]
        );
        // "term" begins a word of both names: the favorite's group first.
        assert_eq!(
            listed_for(&list, "term:"),
            ["ux.desktop#open", "kf.desktop#new-tab", "kf.desktop#prefs"]
        );
        assert_eq!(list.best_row("term:"), Some((2, Some(0))));
        // An action's own row is not its app's: pinning the app moves it
        // neither ahead of a better match nor within its own group.
        let list = favorite("browser.desktop");
        assert_eq!(
            listed_for(&list, "win"),
            [
                "wmt.desktop",
                "browser.desktop#new-window",
                "browser.desktop#private-window"
            ]
        );
        assert_eq!(
            listed_for(&list, "private"),
            ["browser.desktop#private-window", "browser.desktop"]
        );
        // A colon lists actions only: Alt+Up and Alt+Down move no favorite
        // there, as with words.
        list.rank(":");
        assert!(list.has_words());
        list.rank(" ");
        assert!(!list.has_words());
    }

    #[test]
    fn match_fields_ignore_unknown_names() {
        assert_eq!(
            MatchFields::from_names(["generic-name", "keywords", "comment"]),
            ALL_FIELDS
        );
        assert_eq!(
            MatchFields::from_names(["name", "Comment", "comment", "comment"]),
            MatchFields {
                comment: true,
                ..MatchFields::default()
            }
        );
        assert_eq!(MatchFields::from_names([]), MatchFields::default());
    }

    #[test]
    fn app_text_is_folded_and_leaves_out_fields_not_searched() {
        let desktop = desktop_entry(
            "GenericName=Web Browser\nKeywords=Internet;WWW;\nComment=Browse the Café\n",
        );
        assert_eq!(
            AppText::new(&desktop, ALL_FIELDS),
            AppText {
                name: "fixture".to_owned(),
                generic_name: "web browser".to_owned(),
                keywords: "internet www".to_owned(),
                comment: "browse the cafe".to_owned(),
            }
        );
        assert_eq!(
            AppText::new(
                &desktop,
                MatchFields {
                    keywords: true,
                    ..MatchFields::default()
                }
            ),
            AppText {
                name: "fixture".to_owned(),
                keywords: "internet www".to_owned(),
                ..AppText::default()
            }
        );
        assert_eq!(
            AppText::new(&desktop, MatchFields::default()),
            named("Fixture")
        );
        assert_eq!(
            AppText::new(&desktop_entry(""), ALL_FIELDS),
            named("Fixture")
        );
    }

    #[test]
    fn tiers_go_from_the_name_to_the_other_fields() {
        let code = |text: AppText| tier(&text, "code", false);
        assert_eq!(code(named("Code")), Some(AppTier::Exact));
        assert_eq!(code(named("Codec Tool")), Some(AppTier::Prefix));
        assert_eq!(code(named("Visual Studio Code")), Some(AppTier::WordStarts));
        assert_eq!(code(named("Barcode Reader")), Some(AppTier::InName));
        assert_eq!(
            code(text("Text Editor", "Source code editor", "", "")),
            Some(AppTier::InGenericName)
        );
        assert_eq!(
            code(text("Firefox", "", "code browser", "")),
            Some(AppTier::InOtherFields)
        );
        assert_eq!(
            code(text("Editor", "", "", "Edit code")),
            Some(AppTier::InOtherFields)
        );
        assert_eq!(code(named("Firefox")), None);
        // Fields the filter does not search are empty.
        assert_eq!(code(text("Editor", "", "", "")), None);
    }

    #[test]
    fn tiers_compare_the_folded_words_joined_by_single_spaces() {
        let zeta_dock = |text: AppText| tier(&text, "  Zeta\t dock ", false);
        assert_eq!(zeta_dock(named("Zeta Dock")), Some(AppTier::Exact));
        assert_eq!(zeta_dock(named("Zeta Dockyard")), Some(AppTier::Prefix));
        assert_eq!(zeta_dock(named("Dock-Zeta")), Some(AppTier::WordStarts));
        assert_eq!(zeta_dock(named("Azeta Bdock")), Some(AppTier::InName));
        assert_eq!(
            zeta_dock(text("Zeta Station", "Dock Utilities", "", "")),
            Some(AppTier::InGenericName)
        );
        assert_eq!(
            zeta_dock(text("A Utility", "Zeta", "", "Dock")),
            Some(AppTier::InOtherFields)
        );
        assert_eq!(zeta_dock(named("Zeta Only")), None);
        assert_eq!(tier(&named("Café"), "CAFE", false), Some(AppTier::Exact));
        assert_eq!(
            tier(&named("Straße Planer"), "strasse", false),
            Some(AppTier::Prefix)
        );
        assert_eq!(tier(&named("C"), "c", false), Some(AppTier::Exact));
    }

    #[test]
    fn name_words_are_runs_of_letters_and_digits() {
        assert_eq!(
            tier(&named("Tool-λ7"), "λ7", false),
            Some(AppTier::WordStarts)
        );
        assert_eq!(
            tier(&named("Tool_λ7"), "λ7", false),
            Some(AppTier::WordStarts)
        );
        assert_eq!(tier(&named("Tool9λ7"), "λ7", false), Some(AppTier::InName));
        assert_eq!(tier(&named("Toolaλ7"), "λ7", false), Some(AppTier::InName));
    }

    #[test]
    fn subsequences_match_the_name_alone() {
        let firefox = text("Firefox", "Browser", "fancy fox", "Fast fox");
        assert_eq!(tier(&firefox, "ffx", true), Some(AppTier::Subsequence(7)));
        assert_eq!(tier(&firefox, "ffx", false), None);
        assert_eq!(
            tier(&text("Plain App", "", "fancy fox", ""), "ffx", true),
            None
        );
        assert_eq!(
            tier(&named("Z Qzx Vyz"), "qx vz", true),
            Some(AppTier::Subsequence(3 + 3))
        );
        // Every word must be a subsequence of the name: other fields do not
        // complete it.
        assert_eq!(tier(&text("Axb", "", "", "cd"), "ab cd", true), None);
        // A match by the word rule, in any field, comes first.
        assert_eq!(
            tier(&text("Z Utility", "", "", "qx"), "qx", true),
            Some(AppTier::InOtherFields)
        );
    }

    #[test]
    fn white_space_alone_lists_apps_as_an_empty_filter_does() {
        // The unfiltered order compares the names lowercased, not folded.
        let unfolded = |id: &str, name: &str| App {
            lowercase_name: name.to_lowercase(),
            ..app(id, 0, named(name))
        };
        let apps = || {
            vec![
                unfolded("zeta.desktop", "Zeta"),
                unfolded("eclair.desktop", "Éclair"),
                unfolded("b.desktop", "Same"),
                unfolded("a.desktop", "Same"),
            ]
        };
        // Unfiltered: the lowercase name, in which "é" sorts after "z".
        assert_eq!(
            listed(apps(), "", false),
            ["b.desktop", "a.desktop", "zeta.desktop", "eclair.desktop"]
        );
        // Typed white space has no words: the same rows, in the same order.
        assert_eq!(
            listed(apps(), " \t", false),
            ["b.desktop", "a.desktop", "zeta.desktop", "eclair.desktop"]
        );
    }

    #[test]
    fn hidden_apps_come_back_and_settings_regroup_live() {
        let list = AppList::new(
            vec![
                app("alpha.desktop", 3, named("Probe Alpha")),
                app("beta.desktop", 0, text("Probe Beta", "", "needle", "")),
                app("fafx.desktop", 0, named("Fafx")),
            ],
            settings(false, &["alpha.desktop"]),
        );
        assert_eq!(rows(&list), ["fafx.desktop", "beta.desktop"]);
        // The same settings change nothing.
        assert!(!list.apply_settings(settings(false, &["alpha.desktop"])));
        // No longer hidden: back with its launches, first.
        assert!(list.apply_settings(settings(false, &["alpha.desktop", "!alpha.desktop"])));
        assert_eq!(
            rows(&list),
            ["alpha.desktop", "fafx.desktop", "beta.desktop"]
        );
        list.rank("ffx");
        assert!(rows(&list).is_empty());
        assert!(list.apply_settings(settings(true, &[])));
        assert_eq!(rows(&list), ["fafx.desktop"]);
        list.rank("needle");
        assert_eq!(rows(&list), ["beta.desktop"]);
        assert!(list.apply_settings(AppSettings {
            fields: MatchFields::default(),
            ..settings(true, &[])
        }));
        assert!(rows(&list).is_empty());
        assert!(list.apply_settings(AppSettings {
            fields: MatchFields::from_names(["keywords"]),
            ..settings(true, &[])
        }));
        assert_eq!(rows(&list), ["beta.desktop"]);
    }

    #[test]
    fn added_apps_are_matched_hidden_and_grouped_like_the_others() {
        let list = AppList::new(
            vec![app("alpha.desktop", 2, named("Probe Alpha"))],
            AppSettings {
                fields: MatchFields::from_names(["keywords"]),
                ..settings(false, &["probe-h*"])
            },
        );
        list.rank("probe");
        let beta = app(
            "beta.desktop",
            0,
            text("Probe Beta", "", "needle", "remark"),
        );
        assert_eq!(list.push(beta), 1);
        assert_eq!(
            list.push(app("probe-hidden.desktop", 0, named("Probe Hidden"))),
            2
        );
        assert_eq!(rows(&list), ["alpha.desktop", "beta.desktop"]);
        list.rank("needle");
        assert_eq!(rows(&list), ["beta.desktop"]);
        // The comment is not searched.
        list.rank("remark");
        assert!(rows(&list).is_empty());
        list.rank("");
        assert_eq!(rows(&list), ["alpha.desktop", "beta.desktop"]);
    }

    #[test]
    fn keywords_holding_semicolons_are_searched() {
        let directory = std::env::temp_dir().join(format!("ilia-keywords-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("keywords.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Fixture\nExec=/usr/bin/true\n\
             Keywords=Alpha\\;beta;gamma;\n",
        )
        .unwrap();
        let desktop = gio::DesktopAppInfo::from_filename(&path).unwrap();
        // The keywords GLib cannot read are read from the file.
        let keywords = AppText::new(&desktop, ALL_FIELDS).keywords;
        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(keywords, "alpha;beta gamma");
    }

    #[test]
    fn the_last_fitting_hidden_entry_decides() {
        let hidden = |entries: &[&str]| -> Vec<String> {
            entries.iter().map(|entry| entry.to_string()).collect()
        };
        assert!(is_hidden("vendor-a.desktop", &hidden(&["vendor-*"])));
        assert!(!is_hidden(
            "vendor-a.desktop",
            &hidden(&["vendor-*", "!vendor-a.desktop"])
        ));
        assert!(is_hidden(
            "vendor-a.desktop",
            &hidden(&["vendor-*", "!vendor-a.desktop", "vendor-a.desktop"])
        ));
        assert!(is_hidden(
            "vendor-a.desktop",
            &hidden(&["!vendor-a.desktop", "vendor-*"])
        ));
        assert!(!is_hidden(
            "vendor-a.desktop",
            &hidden(&["*", "!vendor-?.desktop"])
        ));
        assert!(is_hidden(
            "vendor-b.desktop",
            &hidden(&["*", "!vendor-a.desktop"])
        ));
        // Entries that fit nothing decide nothing; `!` alone fits no id.
        assert!(is_hidden(
            "vendor-a.desktop",
            &hidden(&["vendor-a.desktop", "!other.desktop", "!"])
        ));
        assert!(!is_hidden(
            "vendor-a.desktop",
            &hidden(&["!vendor-a.desktop"])
        ));
        // The `!` is only special in front.
        assert!(!is_hidden("vendor-a.desktop", &hidden(&["vendor-!*"])));
    }

    #[test]
    fn hiding_appends_the_plain_id_after_every_entry() {
        let mut hidden: Vec<String> = ["vendor-*", "!vendor-a.desktop", "other.desktop"]
            .iter()
            .map(|entry| entry.to_string())
            .collect();
        assert!(!is_hidden("vendor-a.desktop", &hidden));
        assert!(append_last(&mut hidden, "vendor-a.desktop"));
        assert_eq!(
            hidden,
            [
                "vendor-*",
                "!vendor-a.desktop",
                "other.desktop",
                "vendor-a.desktop"
            ]
        );
        // Last, the plain id overrides the earlier exception.
        assert!(is_hidden("vendor-a.desktop", &hidden));
        // Already the last entry (another program put it there): not again.
        assert!(!append_last(&mut hidden, "vendor-a.desktop"));
        assert_eq!(hidden.len(), 4);
        // Undoing the hide gives back the list as it was.
        assert!(remove_last(&mut hidden, "vendor-a.desktop"));
        assert_eq!(hidden, ["vendor-*", "!vendor-a.desktop", "other.desktop"]);
        let mut empty = Vec::new();
        assert!(append_last(&mut empty, "a.desktop"));
        assert_eq!(empty, ["a.desktop"]);
    }

    #[test]
    fn undoing_a_hide_deletes_only_the_last_equal_entry() {
        let mut hidden: Vec<String> = ["a.desktop", "x.desktop", "b*", "x.desktop", "!c"]
            .iter()
            .map(|entry| entry.to_string())
            .collect();
        assert!(remove_last(&mut hidden, "x.desktop"));
        assert_eq!(hidden, ["a.desktop", "x.desktop", "b*", "!c"]);
        assert!(remove_last(&mut hidden, "x.desktop"));
        assert_eq!(hidden, ["a.desktop", "b*", "!c"]);
        // Patterns are not ids: only an equal entry is deleted.
        assert!(!remove_last(&mut hidden, "b.desktop"));
        assert!(!remove_last(&mut hidden, "x.desktop"));
        assert_eq!(hidden, ["a.desktop", "b*", "!c"]);
    }

    #[test]
    fn unfiltered_apps_go_by_launches_then_name() {
        let apps = vec![
            app("zeta.desktop", 0, named("Zeta")),
            app("alpha.desktop", 0, named("Alpha")),
            app("popular.desktop", 2, named("Popular")),
        ];
        assert_eq!(
            listed(apps, " ", false),
            ["popular.desktop", "alpha.desktop", "zeta.desktop"]
        );
    }

    #[test]
    fn filtered_apps_go_by_tier_launches_folded_name_and_id() {
        let apps = || {
            vec![
                app(
                    "firefox.desktop",
                    10,
                    text("Firefox", "", "code;browser", ""),
                ),
                app("barcode.desktop", 0, named("Barcode Reader")),
                app("codec.desktop", 3, named("Codec Tool")),
                app("code.desktop", 0, named("Code")),
                app("z-codex.desktop", 1, named("Codex")),
                app("a-codex.desktop", 1, named("Codex")),
                app("codeé.desktop", 1, named("Codeé")),
            ]
        };
        assert_eq!(
            listed(apps(), "code", false),
            [
                "code.desktop",
                "codec.desktop",
                "codeé.desktop",
                "a-codex.desktop",
                "z-codex.desktop",
                "barcode.desktop",
                "firefox.desktop",
            ]
        );
        // A one-character filter is grouped too.
        assert_eq!(listed(apps(), "c", false)[0], "codec.desktop");
    }

    #[test]
    fn subsequence_matches_come_last_by_shorter_span() {
        let apps = vec![
            app("firefox.desktop", 10, named("Firefox")),
            app("fafax.desktop", 5, named("Fafax Browser")),
            app("ffax.desktop", 0, named("Ffax Tool")),
            app("plain.desktop", 0, text("Plain", "", "", "ffx")),
            app("other.desktop", 0, named("Other")),
        ];
        assert_eq!(
            listed(apps, "ffx", true),
            [
                "plain.desktop",
                "ffax.desktop",
                "fafax.desktop",
                "firefox.desktop"
            ]
        );
    }

    #[test]
    fn hidden_entries_are_ids_or_patterns() {
        assert!(fits_pattern(
            "org.example.App.desktop",
            "org.example.App.desktop"
        ));
        assert!(!fits_pattern(
            "org.example.app.desktop",
            "org.example.App.desktop"
        ));
        assert!(fits_pattern("snap-*", "snap-one.desktop"));
        assert!(!fits_pattern("snap-*", "my-snap-one.desktop"));
        assert!(!fits_pattern("*.desktop", "a.desktop.bak"));
        assert!(fits_pattern("a*.desktop", "a.desktop"));
        assert!(fits_pattern("*", ""));
        assert!(fits_pattern("a?.desktop", "a1.desktop"));
        assert!(!fits_pattern("a?.desktop", "a.desktop"));
        assert!(!fits_pattern("a?.desktop", "abc.desktop"));
        // `?` stands for one character, not one byte.
        assert!(fits_pattern("caf?.desktop", "café.desktop"));
        assert!(fits_pattern("*a*b?", "xxaybz"));
        assert!(!fits_pattern("*a*b?", "xxayb"));
        // Other characters stand for themselves.
        assert!(fits_pattern("[a]+.desktop", "[a]+.desktop"));
        assert!(!fits_pattern("[a]+.desktop", "a.desktop"));
        assert!(is_hidden(
            "vendor-tool.desktop",
            &["other.desktop".to_owned(), "vendor-*".to_owned()]
        ));
        assert!(!is_hidden("vendor-tool.desktop", &[]));
    }

    #[test]
    fn an_early_enter_launches_what_the_list_would_show_first_for_its_text() {
        let list = AppList::new(
            vec![
                app("calc.desktop", 0, named("Calculator")),
                app("office.desktop", 3, text("LibreOffice Calc", "", "", "")),
                app("hidden-calc.desktop", 9, named("Calc")),
                app("term.desktop", 1, text("Terminal", "", "shell", "")),
            ],
            settings(false, &["hidden-calc.desktop"]),
        );
        let best = |text: &str| list.best_match(text).map(|index| list.id(index));
        // The text at the press decides, not the filter text now.
        list.rank("term");
        assert_eq!(best("calc").as_deref(), Some("calc.desktop"));
        // An empty or blank text: the first row of the unfiltered list.
        assert_eq!(best("").as_deref(), Some("office.desktop"));
        assert_eq!(best("   ").as_deref(), Some("office.desktop"));
        // Hidden apps are never chosen; other fields count as in the list.
        assert_eq!(best("shell").as_deref(), Some("term.desktop"));
        assert_eq!(best("zzz"), None);
        // Subsequences only with fuzzy matching.
        assert_eq!(best("clcltr"), None);
        list.apply_settings(settings(true, &["hidden-calc.desktop"]));
        assert_eq!(best("clcltr").as_deref(), Some("calc.desktop"));
        // Each candidate agrees with the order the list sorts by.
        for query in ["calc", "", "shell", "c", "l"] {
            list.rank(query);
            assert_eq!(rows(&list).first().cloned(), best(query), "{query:?}");
        }
    }

    // Four apps whose names all hold "win", the most launched last.
    fn win_apps() -> Vec<App> {
        vec![
            app("alpha.desktop", 1, named("Alpha Win")),
            app("beta.desktop", 3, named("Beta Win")),
            app("delta.desktop", 0, named("Delta Win")),
            app("gamma.desktop", 0, named("Gamma Win")),
            app("win.desktop", 0, text("Win", "", "", "")),
        ]
    }

    #[test]
    fn favorites_lead_an_empty_filter_in_their_order() {
        let list = AppList::new(
            win_apps(),
            with_favorites(
                settings(false, &[]),
                &[
                    "gamma.desktop",
                    "ghost.desktop",
                    "alpha.desktop",
                    "gamma.desktop",
                ],
            ),
        );
        assert_eq!(
            rows(&list),
            [
                "gamma.desktop",
                "alpha.desktop",
                "beta.desktop",
                "delta.desktop",
                "win.desktop"
            ]
        );
        assert!(list.is_favorite(0) && list.is_favorite(3));
        assert!(!list.is_favorite(1) && !list.is_favorite(4));
        // Blank text has no words.
        list.rank("  ");
        assert_eq!(rows(&list)[..2], ["gamma.desktop", "alpha.desktop"]);
        assert!(!list.has_words());
        // Without favorites, the order of launches and names.
        assert!(list.apply_settings(settings(false, &[])));
        assert_eq!(
            rows(&list),
            [
                "beta.desktop",
                "alpha.desktop",
                "delta.desktop",
                "gamma.desktop",
                "win.desktop"
            ]
        );
    }

    #[test]
    fn favorites_lead_only_within_their_match_group() {
        let list = AppList::new(
            win_apps(),
            with_favorites(settings(false, &[]), &["delta.desktop", "beta.desktop"]),
        );
        list.rank("win");
        assert!(list.has_words());
        // The exact match stays first, unpinned; the pins lead the rest.
        assert_eq!(
            rows(&list),
            [
                "win.desktop",
                "delta.desktop",
                "beta.desktop",
                "alpha.desktop",
                "gamma.desktop"
            ]
        );
        // A pinned keyword match comes after an unpinned prefix match.
        let list = AppList::new(
            vec![
                app("gamma.desktop", 0, named("Gamma Win")),
                app("delta.desktop", 0, text("Delta Win", "", "gamma", "")),
            ],
            with_favorites(settings(false, &[]), &["delta.desktop"]),
        );
        list.rank("gamma");
        assert_eq!(rows(&list), ["gamma.desktop", "delta.desktop"]);
        assert_eq!(
            list.best_match("gamma").map(|index| list.id(index)),
            Some("gamma.desktop".to_owned())
        );
        assert_eq!(
            list.best_match("").map(|index| list.id(index)),
            Some("delta.desktop".to_owned())
        );
    }

    #[test]
    fn favorites_lead_every_subsequence_match_whatever_its_span() {
        let list = AppList::new(
            win_apps(),
            with_favorites(settings(true, &[]), &["delta.desktop"]),
        );
        // "ewn" spans 7 characters of "beta win" and 8 of "delta win".
        list.rank("ewn");
        assert_eq!(rows(&list), ["delta.desktop", "beta.desktop"]);
        assert!(list.apply_settings(settings(true, &[])));
        assert_eq!(rows(&list), ["beta.desktop", "delta.desktop"]);
    }

    #[test]
    fn hidden_favorites_are_not_shown_and_return_to_their_place() {
        let list = AppList::new(
            win_apps(),
            with_favorites(
                settings(false, &["gamma.desktop"]),
                &["gamma.desktop", "alpha.desktop"],
            ),
        );
        assert!(!list.is_shown("gamma.desktop"));
        assert!(list.is_shown("alpha.desktop"));
        assert!(!list.is_shown("ghost.desktop"));
        assert_eq!(rows(&list)[..2], ["alpha.desktop", "beta.desktop"]);
        assert!(list.apply_settings(with_favorites(
            settings(false, &["gamma.desktop", "!gamma*"]),
            &["gamma.desktop", "alpha.desktop"]
        )));
        assert_eq!(rows(&list)[..2], ["gamma.desktop", "alpha.desktop"]);
    }

    #[test]
    fn favorites_are_written_without_repeats() {
        assert_eq!(
            without_repeats(ids(&["a", "b", "a", "c", "b"])),
            ids(&["a", "b", "c"])
        );
        // Ctrl+f appends an id that is not there, and takes out every entry
        // of one that is; the others keep their order.
        assert_eq!(
            toggled_favorite(ids(&["a", "b", "a"]), "c"),
            ids(&["a", "b", "c"])
        );
        assert_eq!(
            toggled_favorite(ids(&["g", "a", "x", "a", "g", "n", "x"]), "a"),
            ids(&["g", "x", "n"])
        );
        assert_eq!(toggled_favorite(Vec::new(), "a"), ids(&["a"]));
        assert_eq!(toggled_favorite(ids(&["a"]), "a"), Vec::<String>::new());
    }

    #[test]
    fn favorites_move_past_the_neighbour_shown() {
        let shown = |id: &str| ["a", "b", "g"].contains(&id);
        let stored = ids(&["ghost", "a", "nodisplay", "g", "a", "tryexec", "b", "ghost"]);
        assert_eq!(
            moved_favorite(stored.clone(), "a", Direction::Down, shown),
            Some(ids(&["ghost", "nodisplay", "g", "a", "tryexec", "b"]))
        );
        assert_eq!(
            moved_favorite(stored.clone(), "b", Direction::Up, shown),
            Some(ids(&["ghost", "a", "nodisplay", "b", "g", "tryexec"]))
        );
        assert_eq!(
            moved_favorite(stored.clone(), "g", Direction::Up, shown),
            Some(ids(&["ghost", "g", "a", "nodisplay", "tryexec", "b"]))
        );
        // The first and last favorites shown, apps that are not favorites,
        // and favorites not shown do not move.
        assert_eq!(
            moved_favorite(stored.clone(), "a", Direction::Up, shown),
            None
        );
        assert_eq!(
            moved_favorite(stored.clone(), "b", Direction::Down, shown),
            None
        );
        assert_eq!(
            moved_favorite(stored.clone(), "z", Direction::Up, shown),
            None
        );
        assert_eq!(
            moved_favorite(stored, "ghost", Direction::Down, shown),
            None
        );
    }

    #[test]
    fn icons_load_for_visible_rows_first_then_the_rest_in_model_order() {
        assert_eq!(icon_queue_order(&[3, 1], 5), vec![3, 1, 0, 2, 4]);
        assert_eq!(icon_queue_order(&[], 3), vec![0, 1, 2]);
        assert_eq!(icon_queue_order(&[2, 0, 1], 3), vec![2, 0, 1]);
        // Out-of-range and repeated indices are ignored.
        assert_eq!(icon_queue_order(&[1, 1, 7], 2), vec![1, 0]);
        assert!(icon_queue_order(&[], 0).is_empty());
    }

    #[test]
    fn the_launch_history_keeps_its_newest_entries() {
        let mut history: Vec<String> = (0..40).map(|index| format!("old{index}")).collect();
        push_capped(&mut history, "new".to_owned());
        assert_eq!(history.len(), 32);
        assert_eq!(history[0], "old9");
        assert_eq!(history.last().map(String::as_str), Some("new"));
        let mut short = vec!["a".to_owned()];
        push_capped(&mut short, "b".to_owned());
        assert_eq!(short, ["a", "b"]);
    }
}
