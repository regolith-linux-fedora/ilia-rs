use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::filter::matches_all_words;
use crate::ipc::{self, TreeReply};
use crate::keybindings::{navigate, wm_cli};
use crate::pages::{select_first_row, DialogPage};

const ICON: u32 = 0;
const TITLE: u32 = 1;
const ID: u32 = 2;

pub struct WindowsPage {
    entry: gtk::Entry,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
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
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        let entry_for_filter = entry.clone();
        filter.set_visible_func(move |tree_model, iter| {
            matches_all_words(
                entry_for_filter.text().as_str(),
                &[&string_value(tree_model, iter, TITLE as i32)],
            )
        });

        match ipc::get_tree() {
            Ok(tree) => add_tree(&model, &tree, is_wayland, icon_size),
            Err(error) => {
                eprintln!("Failed to read or parse window tree from {wm_name}: {error}")
            }
        }
        model.set_sort_column_id(gtk::SortColumn::Index(TITLE), gtk::SortType::Ascending);

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        let icon_renderer = gtk::CellRendererPixbuf::new();
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

        let page = Self {
            entry,
            filter,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
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
    fn get_keybinding(&self) -> char {
        'w'
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

fn add_tree(model: &gtk::ListStore, node: &TreeReply, is_wayland: bool, icon_size: i32) {
    if navigable_window(node, is_wayland) {
        let row = model.append();
        let icon_name = node
            .window_properties
            .as_ref()
            .and_then(|properties| properties.instance.as_deref())
            .or(node.app_id.as_deref());
        if let Some(icon) = load_icon_from_app_name(icon_name, icon_size) {
            model.set(&row, &[(ICON, &icon), (TITLE, &node.name), (ID, &node.id)]);
        } else {
            model.set(&row, &[(TITLE, &node.name), (ID, &node.id)]);
        }
    }
    for child in &node.nodes {
        add_tree(model, child, is_wayland, icon_size);
    }
    for child in &node.floating_nodes {
        add_tree(model, child, is_wayland, icon_size);
    }
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

fn load_icon_from_app_name(name: Option<&str>, icon_size: i32) -> Option<Pixbuf> {
    if icon_size == 0 {
        return None;
    }
    let theme = gtk::IconTheme::default()?;
    if let Some(name) = name {
        if let Some(group) = gio::DesktopAppInfo::search(name).first() {
            if let Some(id) = group.first() {
                return gio::DesktopAppInfo::new(id)
                    .and_then(|app| load_icon_from_info(&theme, &app, icon_size));
            }
        }
        if let Some(pixbuf) = load_icon_name(&theme, name, icon_size) {
            return Some(pixbuf);
        }
    }
    load_icon_name(&theme, "emblem-generic", icon_size)
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
