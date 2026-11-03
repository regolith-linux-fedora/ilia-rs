use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::prelude::*;

use crate::filter::matches_all_words;
use crate::pages::DialogPage;
use crate::runner::runner_app_info;

const ICON: u32 = 0;
const NAME: u32 = 1;
const KEYWORDS: u32 = 2;
const APP_INFO: u32 = 3;
const HISTORY_MAX_LEN: usize = 32;

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
    quit: Rc<dyn Fn()>,
}

impl ApplicationsPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Self {
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

        load_apps(&model, icon_size);
        install_sort(&model, &entry, &launch_counts);
        model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);

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

        let page = Self {
            entry,
            model,
            filter,
            view,
            root: scrolled.upcast(),
            settings,
            launch_history: Rc::new(RefCell::new(launch_history)),
            launch_counts,
            post_launch_sleep,
            quit,
        };
        page.connect_activation();
        page.set_selection();
        if icon_size > 0 {
            load_icons_in_background(&page.model, icon_size);
        }
        page
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

    fn set_selection(&self) {
        let selection = self.view.selection();
        let (paths, _) = selection.selected_rows();
        if let Some(path) = paths.first() {
            self.view
                .scroll_to_cell(Some(path), None::<&gtk::TreeViewColumn>, false, 0.0, 0.0);
            return;
        }
        let first = gtk::TreePath::new_first();
        if self.filter.iter(&first).is_some() {
            selection.select_path(&first);
        }
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
        self.execute_selected();
    }

    fn get_id(&self) -> &'static str {
        "apps"
    }

    fn get_name(&self) -> &'static str {
        "<u>A</u>pplications"
    }

    fn get_icon_name(&self) -> &'static str {
        "system-run"
    }

    fn get_help(&self) -> &'static str {
        "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it."
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Launch Application")].into_iter().collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
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

fn load_apps(model: &gtk::ListStore, icon_size: i32) {
    let blank = if icon_size > 0 {
        Pixbuf::new(Colorspace::Rgb, true, 8, icon_size, icon_size).map(|pixbuf| {
            pixbuf.fill(0);
            pixbuf
        })
    } else {
        None
    };

    for app in gio::AppInfo::all() {
        let Some(id) = app.id() else {
            continue;
        };
        let Some(desktop) = gio::DesktopAppInfo::new(id.as_str()) else {
            continue;
        };
        if !desktop.should_show() {
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
                    if app_value(&model, &iter).and_then(|app| app.id()) == id {
                        model.set(&iter, &[(ICON, &icon)]);
                        break;
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
    use super::{compare_apps, search_text};
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
