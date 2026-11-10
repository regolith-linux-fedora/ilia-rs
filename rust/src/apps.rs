use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::prelude::*;

use crate::filter::matches_all_words;
use crate::pages::{is_enter_key, select_first_row, DialogPage};
use crate::runner::runner_app_info;

const ICON: u32 = 0;
const NAME: u32 = 1;
const KEYWORDS: u32 = 2;
const APP_INFO: u32 = 3;
const HISTORY_MAX_LEN: usize = 32;
// Icons decoded at the same time on GTK's worker threads.
const ICON_LOAD_PARALLELISM: usize = 4;

/// The desktop entries, read on a worker thread and handed over to the main
/// thread.
struct DesktopApps(Vec<gio::DesktopAppInfo>);

// SAFETY: the worker thread creates these objects and moves the only
// references to them to the main thread without using them again, so no
// object is ever used from two threads at once. GObject reference counting
// is atomic, and a GDesktopAppInfo is not tied to the thread or main context
// it was created in.
unsafe impl Send for DesktopApps {}

/// The worker thread's result, kept on the main thread until the
/// Applications page asks for it.
#[derive(Default)]
struct Prefetch {
    started: bool,
    apps: Option<Vec<gio::DesktopAppInfo>>,
    waiter: Option<Box<dyn FnOnce(Vec<gio::DesktopAppInfo>)>>,
}

thread_local! {
    static PREFETCH: RefCell<Prefetch> = RefCell::default();
}

/// Starts reading the desktop files on a worker thread, once, so that the
/// parse overlaps with GTK's own start-up (theme CSS, fontconfig). Call it
/// on the main thread. GLib translates the entries' names and keywords as it
/// reads them, so the locale must be set first.
pub fn prefetch_apps() {
    if PREFETCH.with(|prefetch| std::mem::replace(&mut prefetch.borrow_mut().started, true)) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ilia-apps".to_owned())
        .spawn(|| {
            let mut apps = Some(DesktopApps(read_desktop_apps()));
            glib::idle_add_full(glib::Priority::HIGH, move || {
                if let Some(DesktopApps(apps)) = apps.take() {
                    deliver_apps(apps);
                }
                glib::ControlFlow::Break
            });
        });
    if let Err(error) = spawned {
        eprintln!("Cannot read the desktop files in the background: {error}");
        deliver_apps(read_desktop_apps());
    }
}

/// Every desktop entry GLib finds, hidden ones included.
fn read_desktop_apps() -> Vec<gio::DesktopAppInfo> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|app| app.downcast::<gio::DesktopAppInfo>().ok())
        .collect()
}

fn deliver_apps(apps: Vec<gio::DesktopAppInfo>) {
    let waiter = PREFETCH.with(|prefetch| prefetch.borrow_mut().waiter.take());
    match waiter {
        Some(waiter) => waiter(apps),
        None => PREFETCH.with(|prefetch| prefetch.borrow_mut().apps = Some(apps)),
    }
}

/// Calls `callback` with the desktop entries: at once if they have been
/// read, otherwise from the main loop once the worker thread is done.
fn with_desktop_apps(callback: impl FnOnce(Vec<gio::DesktopAppInfo>) + 'static) {
    prefetch_apps();
    let apps = PREFETCH.with(|prefetch| prefetch.borrow_mut().apps.take());
    match apps {
        Some(apps) => callback(apps),
        None => PREFETCH.with(|prefetch| prefetch.borrow_mut().waiter = Some(Box::new(callback))),
    }
}

pub struct ApplicationsPage {
    entry: gtk::Entry,
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    settings: gio::Settings,
    launch_history: Rc<RefCell<Vec<String>>>,
    launch_counts: Rc<HashMap<String, usize>>,
    post_launch_sleep: u64,
    icon_size: i32,
    quit: Rc<dyn Fn()>,
    // Set once the desktop entries are in the model.
    apps_loaded: Cell<bool>,
    // Enter was pressed before the list arrived: launch the best match once
    // it has.
    activate_when_loaded: Cell<bool>,
}

impl ApplicationsPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
        let launch_history: Vec<String> = settings
            .strv("app-launch-counts")
            .iter()
            .map(ToString::to_string)
            .collect();
        let mut counts = HashMap::new();
        for id in &launch_history {
            *counts.entry(id.clone()).or_insert(0) += 1;
        }
        let launch_counts = Rc::new(counts);
        let icon_size = settings.int("icon-size");
        let post_launch_sleep = settings.int("post-launch-sleep").max(0) as u64;

        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        let entry_for_filter = entry.clone();
        filter.set_visible_func(move |tree_model, iter| {
            // Each word may match the name or the keywords.
            matches_all_words(
                entry_for_filter.text().as_str(),
                &[
                    &string_value(tree_model, iter, NAME as i32),
                    &string_value(tree_model, iter, KEYWORDS as i32),
                ],
            )
        });

        let view = gtk::TreeView::with_model(&filter);
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
        let column =
            gtk::TreeViewColumn::with_attributes("Name", &renderer, &[("text", NAME as i32)]);
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&column);
        view.set_activate_on_single_click(true);

        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.style_context().add_class("scrolled_window");
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Rc::new(Self {
            entry,
            model,
            filter,
            view,
            root: scrolled.upcast(),
            settings,
            launch_history: Rc::new(RefCell::new(launch_history)),
            launch_counts,
            post_launch_sleep,
            icon_size,
            quit,
            apps_loaded: Cell::new(false),
            activate_when_loaded: Cell::new(false),
        });
        page.connect_activation();
        // Keys can arrive before the list does: the navigation handler needs
        // a path.
        page.set_selection();

        // The window is built and shown while the desktop files are still
        // being read.
        let weak = Rc::downgrade(&page);
        with_desktop_apps(move |apps| {
            if let Some(page) = weak.upgrade() {
                page.finish_loading(apps);
            }
        });
        page
    }

    fn finish_loading(&self, apps: Vec<gio::DesktopAppInfo>) {
        load_apps(&self.model, apps, self.icon_size);
        install_sort(&self.model, &self.entry, &self.launch_counts);
        self.model
            .set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);
        self.set_selection();
        self.apps_loaded.set(true);
        if self.activate_when_loaded.get() {
            self.execute_selected();
        }

        // Icons once the list is on screen. The model must only be accessed
        // from the GTK main thread: walking it from another thread races with
        // the re-sort done on every keystroke.
        if self.icon_size > 0 {
            let (model, filter, icon_size) =
                (self.model.clone(), self.filter.clone(), self.icon_size);
            glib::idle_add_local_once(move || IconLoader::start(&model, &filter, icon_size));
        }
    }

    fn connect_activation(&self) {
        let settings = self.settings.clone();
        let history = self.launch_history.clone();
        let quit = self.quit.clone();
        let post_launch_sleep = self.post_launch_sleep;
        self.view.connect_row_activated(move |view, path, _| {
            let Some(filter) = view.model().and_downcast::<gtk::TreeModelFilter>() else {
                return;
            };
            let Some(iter) = filter.iter(path) else {
                return;
            };
            let Some(app) = app_value(&filter, &iter) else {
                return;
            };
            execute_app(&app, &settings, &history, post_launch_sleep, &quit);
        });
    }

    /// Highlights the first row: the best match for the text typed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        select_first_row(&self.view);
    }

    fn execute_selected(&self) {
        let Some((model, iter)) = self.view.selection().selected() else {
            return;
        };
        let Some(app) = app_value(&model, &iter) else {
            return;
        };
        execute_app(
            &app,
            &self.settings,
            &self.launch_history,
            self.post_launch_sleep,
            &self.quit,
        );
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
}

impl DialogPage for ApplicationsPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn on_entry_changed(&self) {
        install_sort(&self.model, &self.entry, &self.launch_counts);
        self.filter.refilter();
        self.set_selection();
    }

    fn on_entry_activated(&self) {
        if !self.apps_loaded.get() {
            self.activate_when_loaded.set(true);
            return;
        }
        self.execute_selected();
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
        [("enter", "Launch Application")].into_iter().collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if !self.apps_loaded.get() && is_enter_key(&event.keyval()) {
            self.activate_when_loaded.set(true);
            return true;
        }
        self.navigate(event)
    }

    fn get_keybinding(&self) -> char {
        'a'
    }

    fn show(&self) {
        self.view.grab_focus();
    }
}

fn string_value(
    model: &impl glib::IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
    column: i32,
) -> String {
    model
        .value(iter, column)
        .get::<String>()
        .unwrap_or_default()
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

fn load_apps(model: &gtk::ListStore, apps: Vec<gio::DesktopAppInfo>, icon_size: i32) {
    let blank = if icon_size > 0 {
        Pixbuf::new(Colorspace::Rgb, true, 8, icon_size, icon_size).map(|pixbuf| {
            pixbuf.fill(0);
            pixbuf
        })
    } else {
        None
    };

    // The objects GLib read: creating new ones from their ids would parse
    // each desktop file a second time.
    for desktop in apps {
        if desktop.id().is_none() || !desktop.should_show() {
            continue;
        }
        let name = desktop.name().to_string();
        let keywords = search_text(&desktop);
        let iter = model.append();
        if let Some(blank) = blank.as_ref() {
            model.set(
                &iter,
                &[
                    (ICON, blank),
                    (NAME, &name),
                    (KEYWORDS, &keywords),
                    (APP_INFO, &desktop),
                ],
            );
        } else {
            model.set(
                &iter,
                &[(NAME, &name), (KEYWORDS, &keywords), (APP_INFO, &desktop)],
            );
        }
    }
}

fn install_sort(
    model: &gtk::ListStore,
    entry: &gtk::Entry,
    launch_counts: &Rc<HashMap<String, usize>>,
) {
    let entry = entry.clone();
    let launch_counts = launch_counts.clone();
    model.set_sort_func(gtk::SortColumn::Index(NAME), move |model, a, b| {
        let query = entry.text().to_lowercase();
        let a_name = string_value(model, a, NAME as i32).to_lowercase();
        let b_name = string_value(model, b, NAME as i32).to_lowercase();
        let a_id = app_value(model, a)
            .and_then(|app| app.id())
            .map(|id| id.to_string())
            .unwrap_or_default();
        let b_id = app_value(model, b)
            .and_then(|app| app.id())
            .map(|id| id.to_string())
            .unwrap_or_default();
        compare_apps(&a_name, &b_name, &a_id, &b_id, &query, &launch_counts)
    });
}

fn compare_apps(
    a_name: &str,
    b_name: &str,
    a_id: &str,
    b_id: &str,
    query: &str,
    launch_counts: &HashMap<String, usize>,
) -> Ordering {
    if query.len() > 1 {
        let a_prefix = a_name.starts_with(query);
        let b_prefix = b_name.starts_with(query);
        match (a_prefix, b_prefix) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
    }
    let a_count = launch_counts.get(a_id).copied().unwrap_or_default();
    let b_count = launch_counts.get(b_id).copied().unwrap_or_default();
    b_count.cmp(&a_count).then_with(|| a_name.cmp(b_name))
}

fn execute_app(
    desktop: &gio::DesktopAppInfo,
    settings: &gio::Settings,
    history: &RefCell<Vec<String>>,
    post_launch_sleep: u64,
    quit: &Rc<dyn Fn()>,
) {
    let app_info: gio::AppInfo = desktop.clone().upcast();
    let id = app_info.id().map(|id| id.to_string()).unwrap_or_default();
    let name = app_info.name().to_string();
    let runner = match runner_app_info(&app_info) {
        Ok(runner) => runner,
        Err(error) => {
            eprintln!("{}", error.message());
            quit();
            return;
        }
    };
    let context = gio::AppLaunchContext::new();
    let quit_on_launched = quit.clone();
    context.connect_launched(move |_, _, _| quit_on_launched());
    let quit_on_failure = quit.clone();
    context.connect_launch_failed(move |_, startup_notify_id| {
        eprintln!("Failed to launch app: {startup_notify_id}");
        quit_on_failure();
    });
    let quit_on_timeout = quit.clone();
    context.connect_local("launch-started", false, move |_| {
        let quit = quit_on_timeout.clone();
        glib::timeout_add_local_once(Duration::from_millis(post_launch_sleep), move || quit());
        None
    });

    match runner.launch(&[], Some(&context)) {
        Ok(()) => {
            let mut history = history.borrow_mut();
            history.push(id);
            let stored = if history.len() <= HISTORY_MAX_LEN {
                history.as_slice()
            } else {
                &history[1..=HISTORY_MAX_LEN]
            };
            let values: Vec<&str> = stored.iter().map(String::as_str).collect();
            let _ = settings.set_strv("app-launch-counts", values);
        }
        Err(error) => {
            if error.message().is_empty() {
                eprintln!("Failed to launch {name}");
            } else {
                eprintln!("{}", error.message());
            }
            quit();
        }
    }
}

/// Decodes the rows' icons on GTK's worker threads, a few at a time. The
/// icons are looked up on the main thread, where the theme cache makes that
/// cheap.
struct IconLoader {
    model: gtk::ListStore,
    theme: gtk::IconTheme,
    icon_size: i32,
    queue: RefCell<VecDeque<gtk::TreeIter>>,
}

impl IconLoader {
    /// Queues every row, the ones visible through the filter first and in
    /// the order shown, so the visible rows get their icons first and rows
    /// hidden by text typed before the list arrived still get theirs.
    fn start(model: &gtk::ListStore, filter: &gtk::TreeModelFilter, icon_size: i32) {
        let Some(theme) = gtk::IconTheme::default() else {
            return;
        };
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
                rows.push(iter.clone());
                if !model.iter_next(&iter) {
                    break;
                }
            }
        }
        let queue = icon_queue_order(&visible, rows.len())
            .into_iter()
            .map(|index| rows[index].clone())
            .collect();
        let loader = Rc::new(Self {
            model: model.clone(),
            theme,
            icon_size,
            queue: RefCell::new(queue),
        });
        for _ in 0..ICON_LOAD_PARALLELISM {
            loader.load_next();
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
            let loader = self.clone();
            info.load_icon_async(None::<&gio::Cancellable>, move |result| {
                match result {
                    // GtkListStore iterators persist across re-sorting and
                    // rows are never removed.
                    Ok(pixbuf) => loader.model.set(&iter, &[(ICON, &pixbuf)]),
                    Err(error) => glib::g_debug!("ilia", "Icon not loaded: {}", error.message()),
                }
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

/// The text the Applications page searches besides the app name: the
/// generic name, the keywords and the comment, as the desktop entry gives
/// them for the current locale. Each part is optional; they are separated by
/// spaces so words from different parts do not run together.
fn search_text(desktop: &gio::DesktopAppInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(generic_name) = desktop.generic_name() {
        parts.push(generic_name.to_string());
    }
    parts.extend(desktop.keywords().iter().map(|keyword| keyword.to_string()));
    if let Some(comment) = desktop.description() {
        parts.push(comment.to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::{compare_apps, icon_queue_order, search_text};
    use std::cmp::Ordering;
    use std::collections::HashMap;

    #[test]
    fn app_sort_prefers_prefix_then_history_then_name() {
        let counts = HashMap::from([("popular.desktop".to_owned(), 2)]);
        assert_eq!(
            compare_apps("alpha", "my alpha", "a", "popular.desktop", "al", &counts),
            Ordering::Less
        );
        assert_eq!(
            compare_apps("zeta", "alpha", "popular.desktop", "a", "", &counts),
            Ordering::Less
        );
        assert_eq!(
            compare_apps("alpha", "zeta", "a", "z", "", &HashMap::new()),
            Ordering::Less
        );
    }

    // GLib rejects an entry whose Exec program is not on PATH, and another
    // test swaps PATH while it runs: name the program by its absolute path.
    fn desktop_entry(lines: &str) -> gio::DesktopAppInfo {
        let keyfile = glib::KeyFile::new();
        keyfile
            .load_from_data(
                &format!(
                    "[Desktop Entry]\nType=Application\nName=Fixture\nExec=/usr/bin/true\n{lines}"
                ),
                glib::KeyFileFlags::NONE,
            )
            .unwrap();
        gio::DesktopAppInfo::from_keyfile(&keyfile).unwrap()
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
    fn search_text_holds_generic_name_keywords_and_comment() {
        // Keywords count without a comment.
        assert_eq!(
            search_text(&desktop_entry("Keywords=Preferences;Settings;\n")),
            "Preferences Settings"
        );
        assert_eq!(
            search_text(&desktop_entry(
                "GenericName=Web Browser\nKeywords=Internet;WWW;\nComment=Browse the web\n"
            )),
            "Web Browser Internet WWW Browse the web"
        );
        assert_eq!(search_text(&desktop_entry("")), "");
    }
}
