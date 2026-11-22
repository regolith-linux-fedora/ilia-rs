use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::apps::{self, EntriesUpdate};
use crate::entries::{self, DesktopEntry};
use crate::filter::fold_text;
use crate::icons::load_icon_from_name;
use crate::ipc::{self, TreeReply};
use crate::keybindings::{navigate, wm_cli};
use crate::pages::{follow_fuzzy_setting, select_first_row, DialogPage, RowFilter, FUZZY_MATCHING};

const ICON: u32 = 0;
const TITLE: u32 = 1;
const ID: u32 = 2;
const FOLDED_TITLE: u32 = 3;

pub struct WindowsPage {
    entry: gtk::Entry,
    model: gtk::ListStore,
    row_filter: Rc<RowFilter>,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: Rc<RefCell<gtk::TreePath>>,
    wm_name: String,
    quit: Rc<dyn Fn()>,
}

impl WindowsPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        wm_name: String,
        is_wayland: bool,
        quit: Rc<dyn Fn()>,
    ) -> Self {
        let icon_size = settings.int("icon-size");
        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        // Subsequence matching uses the title too.
        let row_filter = RowFilter::new(&[FOLDED_TITLE], settings.boolean(FUZZY_MATCHING));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let row_filter_for_filter = row_filter.clone();
        filter.set_visible_func(move |tree_model, iter| {
            row_filter_for_filter.row_match(tree_model, iter).is_some()
        });

        match ipc::get_tree(&wm_name) {
            Ok(tree) => add_windows(&model, &tree, is_wayland, icon_size),
            Err(error) => {
                eprintln!("Failed to read or parse window tree from {wm_name}: {error}")
            }
        }
        row_filter.sort(&model, TITLE);
        model.set_sort_column_id(gtk::SortColumn::Index(TITLE), gtk::SortType::Ascending);

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        // With a fixed height GTK sizes the column from one row: a fixed
        // size keeps every icon shown when that row has none.
        let icon_renderer = gtk::CellRendererPixbuf::new();
        if icon_size > 0 {
            icon_renderer.set_fixed_size(icon_size, icon_size);
        }
        let icon_column =
            gtk::TreeViewColumn::with_attributes("App", &icon_renderer, &[("pixbuf", ICON as i32)]);
        icon_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&icon_column);
        let text_renderer = gtk::CellRendererText::new();
        let text_column = gtk::TreeViewColumn::with_attributes(
            "Title",
            &text_renderer,
            &[("text", TITLE as i32)],
        );
        text_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&text_column);
        view.set_activate_on_single_click(true);
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let path = Rc::new(RefCell::new(gtk::TreePath::new_first()));
        follow_fuzzy_setting(&settings, &row_filter, &model, TITLE, &filter, &view, &path);
        let page = Self {
            entry,
            model,
            row_filter,
            filter,
            view,
            root: scrolled.upcast(),
            path: path.clone(),
            wm_name,
            quit,
        };
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let wm_name = self.wm_name.clone();
        let quit = self.quit.clone();
        self.view.connect_row_activated(move |view, path, _| {
            let Some(model) = view.model() else {
                return;
            };
            let Some(iter) = model.iter(path) else {
                return;
            };
            let id = string_value(&model, &iter, ID as i32);
            if !id.is_empty() {
                focus_window(&wm_name, &id, &quit);
            }
        });
    }

    /// Highlights the first row: the best match for the text typed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        *self.path.borrow_mut() = select_first_row(&self.view);
    }

    fn focus_selected(&self) {
        let Some(iter) = self.filter.iter(&self.path.borrow()) else {
            return;
        };
        let id = string_value(&self.filter, &iter, ID as i32);
        if !id.is_empty() {
            focus_window(&self.wm_name, &id, &self.quit);
        }
    }
}

impl DialogPage for WindowsPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        self.row_filter.set_text(self.entry.text().as_str());
        self.row_filter.sort(&self.model, TITLE);
        self.filter.refilter();
        self.set_selection();
    }
    fn on_entry_activated(&self) {
        self.focus_selected();
    }
    fn get_id(&self) -> &'static str {
        "windows"
    }
    fn get_name(&self) -> &'static str {
        "<u>W</u>indows"
    }
    fn get_icon_name(&self) -> &'static str {
        "window-new"
    }
    fn get_help(&self) -> &'static str {
        "This dialog allows for navigating between open windows in the desktop environment.  Selecting a window from the list will cause it to be viewed on screen and focused."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Navigate to Window")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        event.keyval() == gdk::keys::constants::Return
            && self.filter.iter_first().is_none()
            && !self.entry.text().is_empty()
    }
    fn get_keybinding(&self) -> Option<char> {
        Some('w')
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

/// Lists the navigable windows under `tree` with their icons.
///
/// When the desktop entries are being read for the Applications page (with
/// `-a`), GLib's own search would wait for that read, or forever for a file
/// that never becomes readable: GLib reads entry files under one lock. The
/// rows then start with the generic application icon and get their own
/// icons from the entries read, once they are listed.
fn add_windows(model: &gtk::ListStore, tree: &TreeReply, is_wayland: bool, icon_size: i32) {
    let mut windows = Vec::new();
    collect_windows(tree, is_wayland, &mut windows);
    let deferred = apps::entries_requested();
    let mut rows = Vec::new();
    for node in windows {
        let app_name = node
            .window_properties
            .as_ref()
            .and_then(|properties| properties.instance.as_deref())
            .or(node.app_id.as_deref());
        let icon = if deferred {
            generic_icon(icon_size)
        } else {
            window_icon(app_name, icon_size, search_glib)
        };
        let folded_title = fold_text(&node.name);
        let mut values: Vec<(u32, &dyn ToValue)> = vec![
            (TITLE, &node.name),
            (ID, &node.id),
            (FOLDED_TITLE, &folded_title),
        ];
        if let Some(icon) = icon.as_ref() {
            values.push((ICON, icon));
        }
        let row = model.append();
        model.set(&row, &values);
        rows.push((row, app_name.map(str::to_owned)));
    }
    if !deferred || rows.is_empty() {
        return;
    }
    let model = model.clone();
    apps::watch_entries(move |update| {
        let (EntriesUpdate::Listed(all) | EntriesUpdate::Added { all, .. }) = update;
        for (row, app_name) in &rows {
            let icon = window_icon(app_name.as_deref(), icon_size, |name| {
                search_entries(all, name)
            });
            if let Some(icon) = icon {
                model.set(row, &[(ICON, &icon)]);
            }
        }
    });
}

fn collect_windows<'a>(node: &'a TreeReply, is_wayland: bool, windows: &mut Vec<&'a TreeReply>) {
    if navigable_window(node, is_wayland) {
        windows.push(node);
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        collect_windows(child, is_wayland, windows);
    }
}

fn window_icon(
    app_name: Option<&str>,
    icon_size: i32,
    find_entry: impl Fn(&str) -> Option<Option<gio::DesktopAppInfo>>,
) -> Option<Pixbuf> {
    // The app has no icon, or it cannot be loaded.
    load_icon_from_app_name(app_name, icon_size, find_entry).or_else(|| generic_icon(icon_size))
}

fn generic_icon(icon_size: i32) -> Option<Pixbuf> {
    gtk::IconTheme::default()
        .and_then(|theme| load_icon_from_name(&theme, "application-x-executable", icon_size))
}

/// Whether the windows page lists a tree node. Every window the window
/// manager manages is listed, dialogs and utility windows included, as sway
/// does; i3 reports each X11 window's type, and dock windows (bars) are left
/// out.
pub fn navigable_window(node: &TreeReply, is_wayland: bool) -> bool {
    let class = node
        .window_properties
        .as_ref()
        .and_then(|properties| properties.class.as_deref());
    let is_listed_type = node
        .window_type
        .as_deref()
        .is_some_and(|window_type| window_type != "dock")
        || (is_wayland && node.layout.as_deref() == Some("none"));
    matches!(node.node_type.as_str(), "con" | "floating_con")
        && is_listed_type
        && class != Some("i3bar")
}

/// A window's icon: that of the desktop entry `find_entry` matches to its
/// app name, else the themed icon of that name, else the generic emblem.
/// `find_entry` returns `None` when no entry matches and `Some(None)` when
/// the best match cannot be loaded, which leaves the window without an icon.
fn load_icon_from_app_name(
    name: Option<&str>,
    icon_size: i32,
    find_entry: impl Fn(&str) -> Option<Option<gio::DesktopAppInfo>>,
) -> Option<Pixbuf> {
    if icon_size == 0 {
        return None;
    }
    let theme = gtk::IconTheme::default()?;
    if let Some(name) = name {
        if let Some(app) = find_entry(name) {
            return app.and_then(|app| load_icon_from_info(&theme, &app, icon_size));
        }
        if let Some(pixbuf) = load_icon_name(&theme, name, icon_size) {
            return Some(pixbuf);
        }
    }
    load_icon_name(&theme, "emblem-generic", icon_size)
}

/// GLib's best search match for a window's app name.
fn search_glib(name: &str) -> Option<Option<gio::DesktopAppInfo>> {
    let id = gio::DesktopAppInfo::search(name)
        .into_iter()
        .next()?
        .into_iter()
        .next()?;
    Some(gio::DesktopAppInfo::new(&id))
}

/// The same match among the entries the Applications page has read.
fn search_entries(entries: &[Rc<DesktopEntry>], name: &str) -> Option<Option<gio::DesktopAppInfo>> {
    entries::best_match(entries.iter().map(Rc::as_ref), name).map(|entry| entry.app.clone())
}

fn load_icon_from_info(
    theme: &gtk::IconTheme,
    app: &gio::DesktopAppInfo,
    icon_size: i32,
) -> Option<Pixbuf> {
    let icon = app
        .icon()
        .and_then(|icon| gio::prelude::IconExt::to_string(&icon))?;
    if let Some(info) =
        theme.lookup_icon(icon.as_str(), icon_size, gtk::IconLookupFlags::FORCE_SIZE)
    {
        match info.load_icon() {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error: load_icon failed: {}", error.message()),
        }
    }
    if Path::new(icon.as_str()).exists() {
        match Pixbuf::from_file_at_size(icon.as_str(), icon_size, icon_size) {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error loading icon: {}", error.message()),
        }
    }
    None
}

fn load_icon_name(theme: &gtk::IconTheme, name: &str, icon_size: i32) -> Option<Pixbuf> {
    if Path::new(name).exists() {
        match Pixbuf::from_file_at_size(name, icon_size, icon_size) {
            Ok(pixbuf) => return Some(pixbuf),
            Err(error) => eprintln!("Error loading icon: {}", error.message()),
        }
    }
    let info = theme.lookup_icon(name, icon_size, gtk::IconLookupFlags::FORCE_SIZE)?;
    match info.load_icon() {
        Ok(pixbuf) => Some(pixbuf),
        Err(error) => {
            eprintln!("Error: load_icon failed: {}", error.message());
            None
        }
    }
}

fn focus_window(wm_name: &str, id: &str, quit: &Rc<dyn Fn()>) {
    let Some(commandline) = focus_commandline(wm_name, id) else {
        eprintln!("Error: Cannot focus window - action not supported for your WM.");
        return;
    };
    match gio::AppInfo::create_from_commandline(&commandline, None, gio::AppInfoCreateFlags::NONE)
        .and_then(|app| app.launch(&[], None::<&gio::AppLaunchContext>))
    {
        Ok(()) => quit(),
        Err(error) => eprintln!("Error: execute_keybinding failed: {}", error.message()),
    }
}

fn focus_commandline(wm_name: &str, id: &str) -> Option<String> {
    wm_cli(wm_name).map(|cli| format!("{cli}[con_id=\"{id}\"] focus"))
}

#[cfg(test)]
mod tests {
    use super::{focus_commandline, navigable_window};
    use crate::ipc::{TreeReply, WindowProperties};

    fn node(node_type: &str, window_type: Option<&str>, class: Option<&str>) -> TreeReply {
        TreeReply {
            node_type: node_type.to_owned(),
            window_type: window_type.map(str::to_owned),
            layout: Some("none".to_owned()),
            window_properties: Some(WindowProperties {
                class: class.map(str::to_owned),
                ..WindowProperties::default()
            }),
            ..TreeReply::default()
        }
    }

    #[test]
    fn navigability_matches_compositor_rules() {
        assert!(navigable_window(&node("con", Some("normal"), None), false));
        assert!(navigable_window(
            &node("floating_con", Some("unknown"), None),
            false
        ));
        assert!(!navigable_window(
            &node("con", Some("normal"), Some("i3bar")),
            false
        ));
        assert!(navigable_window(&node("con", None, None), true));
        assert!(!navigable_window(
            &node("workspace", Some("normal"), None),
            true
        ));
        // i3 lists dialogs and utility windows too, but not docks.
        for window_type in ["dialog", "utility", "splash", "toolbar", "menu"] {
            assert!(
                navigable_window(&node("con", Some(window_type), None), false),
                "{window_type}"
            );
        }
        assert!(navigable_window(
            &node("floating_con", Some("dialog"), None),
            false
        ));
        assert!(!navigable_window(&node("con", Some("dock"), None), false));
        assert!(!navigable_window(&node("con", None, None), false));
        assert_eq!(
            focus_commandline("sway", "42").as_deref(),
            Some("/usr/bin/swaymsg [con_id=\"42\"] focus")
        );
    }
}
