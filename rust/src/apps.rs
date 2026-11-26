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
use gtk::prelude::*;

use crate::add_app_form::AddAppForm;
use crate::clipboard_keeper;
use crate::entries::{self, Delivery, DesktopEntry};
use crate::filter::{fold_text, query_words, summed_span, words_in_fields};
use crate::keybindings::navigate;
use crate::launch::launch_desktop_app;
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
// On the Applications page: the star of a favorite, empty for other apps.
const FAVORITE: u32 = 4;
/// The mark of a favorite's row, in a cell of its own after the name.
const STAR: &str = "\u{2605}";
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
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
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

        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
            u32::static_type(),
            String::static_type(),
        ]);
        // The rows come once the desktop entries have been read (see
        // `finish_loading`); the window is built and shown in the meantime.
        let apps = Rc::new(AppList::new(Vec::new(), AppSettings::read(&settings)));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let apps_for_filter = apps.clone();
        filter.set_visible_func(move |tree_model, iter| {
            apps_for_filter.is_listed(index_value(tree_model, iter))
        });

        install_sort(&model, &apps);
        model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);

        let (view, scrolled) = app_view(&filter, icon_size, true);

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
        });
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
        for desktop in entries.iter().filter_map(|entry| entry.app.as_ref()) {
            if let Some(id) = shown_id(desktop).filter(|id| listed.insert(id.clone())) {
                self.add_row(id, desktop, blank.as_ref());
            }
        }
        self.set_selection();
        self.apps_loaded.set(true);
        // That Enter acts once, on this list, for the text typed before it,
        // whatever the search box holds now. When nothing matches that text
        // nothing is started and the window stays open.
        if let Some((text, workspace)) = self.pending_enter.take() {
            if let Some(app) = self
                .apps
                .best_match(&text)
                .and_then(|index| self.app_at(index))
            {
                self.execute_on(&app, workspace);
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
        let row = self.add_row(id, desktop, blank_icon(self.icon_size).as_ref());
        // Before the loader starts, it finds the row in the model itself.
        if let Some(loader) = self.icon_loader.borrow().as_ref() {
            loader.enqueue(row);
        }
    }

    /// Adds the app `desktop`, whose desktop id is `id`, to the list and a
    /// row for it to the model, and returns the row.
    fn add_row(
        &self,
        id: String,
        desktop: &gio::DesktopAppInfo,
        blank: Option<&Pixbuf>,
    ) -> gtk::TreeIter {
        let launches = self.launch_counts.get(&id).copied().unwrap_or_default();
        let index = self.apps.push(App::new(id, desktop, launches));
        let row = insert_row(&self.model, desktop, index, blank);
        set_star(&self.model, &row, self.apps.is_favorite(index));
        row
    }

    /// The app of the row whose INDEX is `index`.
    fn app_at(&self, index: usize) -> Option<gio::DesktopAppInfo> {
        let iter = self.model.iter_first()?;
        loop {
            if index_value(&self.model, &iter) == index {
                return app_value(&self.model, &iter);
            }
            if !self.model.iter_next(&iter) {
                return None;
            }
        }
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

        // Under the X11 grab the tree view does not activate a row on a
        // single click itself: launch the row a primary click is released
        // on. Where it does (Wayland), `launched` keeps it to one launch.
        let weak = Rc::downgrade(self);
        self.view.connect_button_release_event(move |view, event| {
            let Some(page) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if event.button() != gdk::BUTTON_PRIMARY
                || event
                    .state()
                    .intersects(gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK)
                || page.form.is_open()
            {
                return glib::Propagation::Proceed;
            }
            if let Some(path) = row_at_pointer(view, event) {
                page.execute_path(&path);
            }
            glib::Propagation::Proceed
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
            .and_then(|(model, iter)| app_value(&model, &iter))
            .map(|app| program_path::describe_app(&app))
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
        let index = self.apps.push(App::new(id, &desktop, 0));
        let row = insert_row(
            &self.model,
            &desktop,
            index,
            blank_icon(self.icon_size).as_ref(),
        );
        set_star(&self.model, &row, self.apps.is_favorite(index));
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
        if let Some(app) = app_value(&self.filter, &iter) {
            self.execute(&app);
        }
    }

    fn execute(&self, app: &gio::DesktopAppInfo) {
        execute_app(
            app,
            &self.settings,
            &self.launch_history,
            &self.launched,
            &self.quit,
        );
    }

    /// Launches `app` on `workspace`. For the next free workspace the window
    /// manager switches to it first, and the app is started as Enter starts
    /// it once the switch is confirmed. When the switch fails nothing is
    /// started and the window stays open.
    fn execute_on(&self, app: &gio::DesktopAppInfo, workspace: Workspace) {
        if workspace == Workspace::NextFree {
            if self.launched.get() {
                return;
            }
            if let Err(error) = new_workspace::switch_to_next_free(&self.wm_name) {
                eprintln!("Failed to open a new workspace: {error}");
                return;
            }
        }
        self.execute(app);
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
        if let Some(app) = app_value(&model, &iter) {
            self.execute_on(&app, workspace);
        }
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
        let mut path = paths
            .into_iter()
            .next()
            .unwrap_or_else(gtk::TreePath::new_first);
        if moving_up {
            path.prev();
        } else {
            let mut next = path.clone();
            next.next();
            if self.filter.iter(&next).is_some() {
                path = next;
            }
        }
        if self.filter.iter(&path).is_some() {
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
    /// [`rebuild_keeping_selection`]).
    fn follow_settings(&self) {
        for key in [FUZZY_MATCHING, MATCH_FIELDS, HIDDEN_APPS, FAVORITE_APPS] {
            let (apps, model, filter, view) = (
                self.apps.clone(),
                self.model.clone(),
                self.filter.clone(),
                self.view.clone(),
            );
            self.settings
                .connect_changed(Some(key), move |settings, _| {
                    // The page's own Ctrl+Delete, Ctrl+z, Ctrl+f, Alt+Up and
                    // Alt+Down writes are applied already.
                    let settings = AppSettings::read(settings);
                    if apps.settings() != settings {
                        rebuild_keeping_selection(&view, &filter, || {
                            apps.apply_settings(settings);
                            update_stars(&model, &apps);
                            install_sort(&model, &apps);
                            filter.refilter();
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

    /// Hides the highlighted app: appends its plain desktop id to the
    /// `hidden-apps` setting as stored now, after every entry already there,
    /// and removes its row. The row taking its place is highlighted, or the
    /// new last row when it was the last.
    fn hide_selected(&self) {
        let Some((model, iter)) = self.view.selection().selected() else {
            return;
        };
        let Some(path) = model.path(&iter) else {
            return;
        };
        let id = self.apps.id(index_value(&model, &iter));

        let mut hidden = self.stored_hidden();
        let appended = append_last(&mut hidden, &id);
        if self.store_hidden(hidden, &id) && appended {
            self.hides.borrow_mut().push(id);
        }
        select_row_near(&self.view, path.indices().first().copied().unwrap_or(0));
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
            .index_of(&id)
            .and_then(|index| self.listed_path(index));
        if let Some(path) = restored {
            self.view
                .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
        }
    }

    /// The path of the row listing the app with the index `index`, or
    /// `None` when it is not listed.
    fn listed_path(&self, index: usize) -> Option<gtk::TreePath> {
        (0..self.filter.iter_n_children(None)).find_map(|row| {
            let iter = self.filter.iter_nth_child(None, row)?;
            (index_value(&self.filter, &iter) == index)
                .then(|| gtk::TreePath::from_indicesv(&[row]))
        })
    }

    /// The desktop id of the highlighted app, or `None` when no row is
    /// highlighted.
    fn selected_id(&self) -> Option<String> {
        let (model, iter) = self.view.selection().selected()?;
        Some(self.apps.id(index_value(&model, &iter)))
    }

    /// The `favorite-apps` setting as stored now.
    fn stored_favorites(&self) -> Vec<String> {
        self.settings
            .strv(FAVORITE_APPS)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Applies the `favorite-apps` entries `favorites` to the rows, keeping
    /// the highlighted app, then stores them. Applied before it is written,
    /// the change notification for the write finds nothing new to apply.
    fn store_favorites(&self, favorites: Vec<String>) {
        rebuild_keeping_selection(&self.view, &self.filter, || {
            self.apps.apply_settings(AppSettings {
                favorites: favorites.clone(),
                ..self.apps.settings()
            });
            update_stars(&self.model, &self.apps);
            install_sort(&self.model, &self.apps);
            self.filter.refilter();
        });
        match self.settings.set_strv(FAVORITE_APPS, favorites) {
            Ok(()) => gio::Settings::sync(),
            Err(error) => eprintln!("Failed to change the favorite applications: {error}"),
        }
    }

    /// Ctrl+f: makes the highlighted app a favorite, after the others, or
    /// takes every entry of it out of the favorites, in the setting as
    /// stored now. Nothing is written when no row is highlighted.
    fn toggle_favorite(&self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.store_favorites(toggled_favorite(self.stored_favorites(), &id));
    }

    /// Alt+Up and Alt+Down: moves the highlighted favorite ahead of the
    /// favorite shown above it, or behind the one shown below it, while the
    /// filter text has no words. Nothing is written for the first or last
    /// favorite shown, nor for an app that is not a favorite.
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
        self.apps.rank(self.entry.text().as_str());
        install_sort(&self.model, &self.apps);
        self.filter.refilter();
        self.set_selection();
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
        "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it."
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

/// The path of the row under a button event on `view`, if any.
fn row_at_pointer(view: &gtk::TreeView, event: &gdk::EventButton) -> Option<gtk::TreePath> {
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
    view.path_at_pos(x as i32, y as i32)?.0
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
            insert_row(&model, &desktop, texts.len(), blank.as_ref());
            texts.push(AppText::new(&desktop, MatchFields::ALL));
        }
        let rows = Rc::new(AppRows::new(texts, &settings));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let rows_for_filter = rows.clone();
        filter.set_visible_func(move |tree_model, iter| {
            rows_for_filter.is_listed(index_value(tree_model, iter))
        });
        let (view, root) = app_view(&filter, icon_size, false);

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

/// A list of apps showing the rows of `filter`, and the scrolled window
/// holding it. With `stars`, a cell after the name shows the FAVORITE
/// column.
fn app_view(
    filter: &gtk::TreeModelFilter,
    icon_size: i32,
    stars: bool,
) -> (gtk::TreeView, gtk::Widget) {
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
    if stars {
        // The name takes the width the star leaves.
        column.set_expand(true);
        let renderer = gtk::CellRendererText::new();
        renderer.set_xalign(0.5);
        let column = gtk::TreeViewColumn::with_attributes(
            "Favorite",
            &renderer,
            &[("text", FAVORITE as i32)],
        );
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        let (star_width, _) = view.create_pango_layout(Some(STAR)).pixel_size();
        column.set_fixed_width(star_width + 4 * renderer.xpad() as i32);
        view.append_column(&column);
    }
    view.set_activate_on_single_click(true);

    let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scrolled.style_context().add_class("scrolled_window");
    scrolled.add(&view);
    scrolled.set_hexpand(true);
    scrolled.set_vexpand(true);
    (view, scrolled.upcast())
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

/// Adds a row for `desktop` whose INDEX is `index`, and returns it.
/// GtkListStore iterators stay valid across re-sorting.
fn insert_row(
    model: &gtk::ListStore,
    desktop: &gio::DesktopAppInfo,
    index: usize,
    blank: Option<&Pixbuf>,
) -> gtk::TreeIter {
    let name = desktop.name().to_string();
    let index = index as u32;
    let mut values: Vec<(u32, &dyn ToValue)> =
        vec![(NAME, &name), (APP_INFO, desktop), (INDEX, &index)];
    if let Some(blank) = blank {
        values.push((ICON, blank));
    }
    model.insert_with_values(None, &values)
}

/// Shows the star in the FAVORITE column of `row` when `favorite`, and
/// nothing otherwise. Only a change is written: every write announces the
/// row as changed.
fn set_star(model: &gtk::ListStore, row: &gtk::TreeIter, favorite: bool) {
    let star = if favorite { STAR } else { "" };
    let shown = model
        .value(row, FAVORITE as i32)
        .get::<String>()
        .unwrap_or_default();
    if shown != star {
        model.set(row, &[(FAVORITE, &star)]);
    }
}

/// Stars the rows of the favorites in `apps`, and only them.
fn update_stars(model: &gtk::ListStore, apps: &AppList) {
    // Collected first: a row whose value changes may move in the sorted
    // model, and GtkListStore iterators last across moves.
    let mut rows = Vec::new();
    if let Some(iter) = model.iter_first() {
        loop {
            rows.push(iter);
            if !model.iter_next(&iter) {
                break;
            }
        }
    }
    for row in rows {
        set_star(model, &row, apps.is_favorite(index_value(model, &row)));
    }
}

fn install_sort(model: &gtk::ListStore, apps: &Rc<AppList>) {
    let apps = apps.clone();
    model.set_sort_func(gtk::SortColumn::Index(NAME), move |model, a, b| {
        apps.compare(index_value(model, a), index_value(model, b))
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
            name: fold_text(&desktop.name()),
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
}

impl App {
    /// The app of `desktop`, whose desktop id is `id`, launched `launches`
    /// times. Every field is searched until the list applies `match-fields`.
    fn new(id: String, desktop: &gio::DesktopAppInfo, launches: usize) -> Self {
        let all = AppText::new(desktop, MatchFields::ALL);
        Self {
            id,
            launches,
            lowercase_name: desktop.name().to_lowercase(),
            text: all.clone(),
            all,
        }
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

/// The apps on the page, by the INDEX of their rows, which of them are
/// hidden, which are favorites, and the group of each one for the filter
/// text.
struct AppList {
    apps: RefCell<Vec<App>>,
    settings: RefCell<AppSettings>,
    hidden: RefCell<Vec<bool>>,
    // The place of each favorite in the favorite order, `None` for the
    // other apps.
    favorites: RefCell<Vec<Option<usize>>>,
    // The words of the filter text, `None` while it is empty.
    words: RefCell<Option<Vec<String>>>,
    // `None` while the filter text is empty.
    tiers: RefCell<Option<Vec<Option<AppTier>>>>,
}

impl AppList {
    fn new(apps: Vec<App>, settings: AppSettings) -> Self {
        let list = Self {
            hidden: RefCell::new(vec![false; apps.len()]),
            favorites: RefCell::new(vec![None; apps.len()]),
            apps: RefCell::new(apps),
            settings: RefCell::new(settings.clone()),
            words: RefCell::default(),
            tiers: RefCell::default(),
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

    /// Adds an app after the others, matched, hidden and grouped as they
    /// are, and returns its index.
    fn push(&self, mut app: App) -> usize {
        let settings = self.settings.borrow();
        app.text = app.all.only(settings.fields);
        self.hidden
            .borrow_mut()
            .push(is_hidden(&app.id, &settings.hidden));
        self.favorites
            .borrow_mut()
            .push(favorite_place(&app.id, &settings.favorites));
        if let (Some(words), Some(tiers)) = (
            self.words.borrow().as_ref(),
            self.tiers.borrow_mut().as_mut(),
        ) {
            tiers.push(app.text.tier(words, settings.subsequence));
        }
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

    /// Whether the filter text has words.
    fn has_words(&self) -> bool {
        self.words.borrow().is_some()
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
    }

    /// Groups the apps for the filter text `text`. Text of white space alone
    /// has no words and lists the apps as an empty filter does.
    fn rank(&self, text: &str) {
        let words = query_words(text);
        self.words.replace((!words.is_empty()).then_some(words));
        self.regroup();
    }

    fn regroup(&self) {
        let subsequence = self.settings.borrow().subsequence;
        let tiers = self.words.borrow().as_ref().map(|words| {
            self.apps
                .borrow()
                .iter()
                .map(|app| app.text.tier(words, subsequence))
                .collect()
        });
        self.tiers.replace(tiers);
    }

    fn is_listed(&self, index: usize) -> bool {
        !self.hidden.borrow().get(index).copied().unwrap_or(true)
            && self
                .tiers
                .borrow()
                .as_ref()
                .is_none_or(|tiers| tiers[index].is_some())
    }

    /// The order of two apps: without a filter, the favorites first in
    /// their order, then launches, then the name; with one, the group, then
    /// the favorites first in their order, launches, the folded name, and
    /// the desktop id.
    fn compare(&self, a: usize, b: usize) -> Ordering {
        compare_apps(
            &self.apps.borrow(),
            &self.favorites.borrow(),
            self.tiers.borrow().as_deref(),
            a,
            b,
        )
    }

    /// The index of the app listed first for the filter text `text`, with
    /// the settings applied now, whatever the filter text is now; `None`
    /// when no app is listed for it.
    fn best_match(&self, text: &str) -> Option<usize> {
        let words = query_words(text);
        let apps = self.apps.borrow();
        let subsequence = self.settings.borrow().subsequence;
        let tiers: Option<Vec<Option<AppTier>>> = (!words.is_empty()).then(|| {
            apps.iter()
                .map(|app| app.text.tier(&words, subsequence))
                .collect()
        });
        let hidden = self.hidden.borrow();
        let favorites = self.favorites.borrow();
        (0..apps.len())
            .filter(|&index| {
                !hidden[index] && tiers.as_ref().is_none_or(|tiers| tiers[index].is_some())
            })
            .min_by(|&a, &b| compare_apps(&apps, &favorites, tiers.as_deref(), a, b))
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
    let favorite_order = || match (favorites[a], favorites[b]) {
        (Some(place_a), Some(place_b)) => place_a.cmp(&place_b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
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
    if launched.replace(true) {
        return;
    }
    let id = desktop.id().map(|id| id.to_string()).unwrap_or_default();
    let (settings, history, quit) = (settings.clone(), history.clone(), quit.clone());
    launch_desktop_app(desktop, move |result| {
        match result {
            Ok(()) => record_launch(&settings, &mut history.borrow_mut(), id),
            Err(message) => eprintln!("{message}"),
        }
        quit();
    });
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
    model: gtk::ListStore,
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
        model: &gtk::ListStore,
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
            let Some(info) = app_value(&self.model, &iter).and_then(|app| self.lookup(&app)) else {
                continue;
            };
            self.loading.set(self.loading.get() + 1);
            let loader = self.clone();
            info.load_icon_async(None::<&gio::Cancellable>, move |result| {
                match result {
                    // GtkListStore iterators persist across re-sorting and
                    // rows are never removed.
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
        append_last, fits_pattern, icon_queue_order, is_hidden, moved_favorite, push_capped,
        remove_last, toggled_favorite, without_repeats, App, AppList, AppSettings, AppText,
        AppTier, Direction, MatchFields,
    };
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
