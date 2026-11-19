use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;
use gtk::atk::prelude::AtkObjectExt;
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::prelude::*;

use crate::add_app_form::AddAppForm;
use crate::filter::{fold_text, query_words, summed_span, words_in_fields};
use crate::keybindings::navigate;
use crate::page_files::{PageFile, Rows};
use crate::pages::{
    rebuild_keeping_selection, select_first_row, select_row_near, DialogPage, FUZZY_MATCHING,
};
use crate::runner::runner_app_info;

const ICON: u32 = 0;
const NAME: u32 = 1;
const APP_INFO: u32 = 2;
// The index of the row's app in the page's `AppList`.
const INDEX: u32 = 3;
const HISTORY_MAX_LEN: usize = 32;
/// The launch history the pages share: desktop ids, and the choices on page
/// files ordered by frequency.
pub const LAUNCH_HISTORY: &str = "app-launch-counts";
const HIDDEN_APPS: &str = "hidden-apps";
const MATCH_FIELDS: &str = "match-fields";

pub struct ApplicationsPage {
    entry: gtk::Entry,
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    // The + button in the strip below the list, and the form it opens over
    // the list.
    add_button: gtk::EventBox,
    form: Rc<AddAppForm>,
    // What had the keyboard focus before the form was opened.
    focus_before_form: RefCell<Option<gtk::Widget>>,
    icon_size: i32,
    settings: gio::Settings,
    launch_history: Rc<RefCell<Vec<String>>>,
    apps: Rc<AppList>,
    // The desktop ids this run's Ctrl+Delete hides appended to
    // `hidden-apps`, latest last, less those Ctrl+z has reversed.
    hides: RefCell<Vec<String>>,
    post_launch_sleep: u64,
    quit: Rc<dyn Fn()>,
    // An app was started: ilia is closing. A click can activate a row twice.
    launched: Cell<bool>,
}

impl ApplicationsPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
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
        let post_launch_sleep = settings.int("post-launch-sleep").max(0) as u64;

        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
            u32::static_type(),
        ]);
        // Hidden apps get rows too, which the filter leaves out: one that
        // stops being hidden while the launcher runs comes back as it was.
        let apps = Rc::new(AppList::new(
            load_apps(&model, icon_size, &launch_counts),
            AppSettings::read(&settings),
        ));
        let filter = gtk::TreeModelFilter::new(&model, None);
        let apps_for_filter = apps.clone();
        filter.set_visible_func(move |tree_model, iter| {
            apps_for_filter.is_listed(index_value(tree_model, iter))
        });

        install_sort(&model, &apps);
        model.set_sort_column_id(gtk::SortColumn::Index(NAME), gtk::SortType::Ascending);

        let (view, scrolled) = app_view(&filter, icon_size);

        // The form covers the list while it is shown; the list stays as it
        // is underneath.
        let form = AddAppForm::new();
        let overlay = gtk::Overlay::new();
        overlay.add(&scrolled);
        overlay.add_overlay(form.widget());

        // One line below the list, the + button at its right end.
        let add_button = add_app_button();
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        strip.style_context().add_class("app_strip");
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
            add_button,
            form,
            focus_before_form: RefCell::default(),
            icon_size,
            settings,
            launch_history: Rc::new(RefCell::new(launch_history)),
            apps,
            hides: RefCell::default(),
            post_launch_sleep,
            quit,
            launched: Cell::new(false),
        });
        page.connect_activation();
        page.connect_form();
        page.follow_settings();
        page.set_selection();
        if icon_size > 0 {
            load_icons_in_background(&page.model, icon_size);
        }
        page
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
        // Only the first activation starts anything: ilia is closing.
        if self.launched.replace(true) {
            return;
        }
        execute_app(
            app,
            &self.settings,
            &self.launch_history,
            self.post_launch_sleep,
            &self.quit,
        );
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
        if let Some(app) = app_value(&model, &iter) {
            self.execute(&app);
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
    /// `match-fields` and `hidden-apps` while the launcher runs: the rows are
    /// matched, grouped and sorted again for the same filter text, keeping
    /// the highlighted app (see [`rebuild_keeping_selection`]).
    fn follow_settings(&self) {
        for key in [FUZZY_MATCHING, MATCH_FIELDS, HIDDEN_APPS] {
            let (apps, model, filter, view) = (
                self.apps.clone(),
                self.model.clone(),
                self.filter.clone(),
                self.view.clone(),
            );
            self.settings
                .connect_changed(Some(key), move |settings, _| {
                    // The page's own Ctrl+Delete and Ctrl+z writes are applied
                    // already.
                    let settings = AppSettings::read(settings);
                    if apps.settings() != settings {
                        rebuild_keeping_selection(&view, &filter, || {
                            apps.apply_settings(settings);
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
        "view-app-grid-symbolic"
    }

    fn get_help(&self) -> &'static str {
        "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it."
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [
            ("enter", "Launch Application"),
            ("ctrl+=", "Add Application"),
            ("ctrl+delete", "Hide Application"),
            ("ctrl+z", "Undo Hide"),
        ]
        .into_iter()
        .collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_add_app_shortcut(event) {
            self.open_form();
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

    fn show(&self) {
        if self.form.is_open() {
            self.form.take_focus();
        } else {
            self.view.grab_focus();
        }
    }
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
    post_launch_sleep: u64,
    quit: Rc<dyn Fn()>,
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
        let post_launch_sleep = settings.int("post-launch-sleep").max(0) as u64;
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
            post_launch_sleep,
            quit,
        };
        connect_launch(
            &page.view,
            &page.settings,
            &page.launch_history,
            page.post_launch_sleep,
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
                self.post_launch_sleep,
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
/// holding it.
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

/// Launches the app of a row activated in `view`.
fn connect_launch(
    view: &gtk::TreeView,
    settings: &gio::Settings,
    history: &Rc<RefCell<Vec<String>>>,
    post_launch_sleep: u64,
    quit: &Rc<dyn Fn()>,
) {
    let (settings, history, quit) = (settings.clone(), history.clone(), quit.clone());
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
        execute_app(&app, &settings, &history, post_launch_sleep, &quit);
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

/// Adds a row to `model` for each app to show, hidden ones included, and
/// returns the apps by the INDEX of their rows. There is one app per desktop
/// id: GIO takes each id from the first directory of the Desktop Entry
/// search order that has it, `XDG_DATA_HOME` and then `XDG_DATA_DIRS`.
fn load_apps(
    model: &gtk::ListStore,
    icon_size: i32,
    launch_counts: &HashMap<String, usize>,
) -> Vec<App> {
    let blank = blank_icon(icon_size);

    let mut apps = Vec::new();
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
        insert_row(model, &desktop, apps.len(), blank.as_ref());
        let launches = launch_counts.get(id.as_str()).copied().unwrap_or_default();
        apps.push(App::new(id.into(), &desktop, launches));
    }
    apps
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

/// The settings that decide which apps are listed and how they match.
#[derive(Clone, Debug, PartialEq)]
struct AppSettings {
    subsequence: bool,
    fields: MatchFields,
    hidden: Vec<String>,
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
        }
    }
}

/// The apps on the page, by the INDEX of their rows, which of them are
/// hidden, and the group of each one for the filter text.
struct AppList {
    apps: RefCell<Vec<App>>,
    settings: RefCell<AppSettings>,
    hidden: RefCell<Vec<bool>>,
    // The words of the filter text, `None` while it is empty.
    words: RefCell<Option<Vec<String>>>,
    // `None` while the filter text is empty.
    tiers: RefCell<Option<Vec<Option<AppTier>>>>,
}

impl AppList {
    fn new(apps: Vec<App>, settings: AppSettings) -> Self {
        let list = Self {
            hidden: RefCell::new(vec![false; apps.len()]),
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

    /// Adds an app after the others, matched, hidden and grouped as they
    /// are, and returns its index.
    fn push(&self, mut app: App) -> usize {
        let settings = self.settings.borrow();
        app.text = app.all.only(settings.fields);
        self.hidden
            .borrow_mut()
            .push(is_hidden(&app.id, &settings.hidden));
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

    /// The order of two apps: without a filter, launches first, then the
    /// name; with one, the group, launches, the folded name, and the desktop
    /// id.
    fn compare(&self, a: usize, b: usize) -> Ordering {
        let apps = self.apps.borrow();
        let (app_a, app_b) = (&apps[a], &apps[b]);
        match self.tiers.borrow().as_ref() {
            None => app_b
                .launches
                .cmp(&app_a.launches)
                .then_with(|| app_a.lowercase_name.cmp(&app_b.lowercase_name)),
            Some(tiers) => tiers[a]
                .cmp(&tiers[b])
                .then(app_b.launches.cmp(&app_a.launches))
                .then_with(|| app_a.text.name.cmp(&app_b.text.name))
                .then_with(|| app_a.id.cmp(&app_b.id)),
        }
    }
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
        Ok(()) => record_launch(settings, &mut history.borrow_mut(), id),
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

#[cfg(test)]
mod tests {
    use super::{
        append_last, fits_pattern, is_hidden, push_capped, remove_last, App, AppList, AppSettings,
        AppText, AppTier, MatchFields,
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
