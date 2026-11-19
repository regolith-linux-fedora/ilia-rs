use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::apps::{AppListPage, ApplicationsPage};
use crate::args::Args;
use crate::features::FeaturesPage;
use crate::filter::{match_fields, query_words, Match};
use crate::keybindings::KeybindingsPage;
use crate::notifications::RoficationPage;
use crate::page_files::{PageFile, Rows};
use crate::terminal::CommandPage;
use crate::textlist::TextListPage;
use crate::tracker::TrackerPage;
use crate::windows::WindowsPage;

#[derive(Clone, Copy)]
pub struct Page {
    pub id: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub key: char,
    pub help: &'static str,
    pub bindings: &'static [(&'static str, &'static str)],
}

pub trait DialogPage {
    fn initialize(&self) {}
    fn get_root(&self) -> gtk::Widget;
    fn on_entry_changed(&self) {}
    fn on_entry_activated(&self) {}
    fn get_id(&self) -> &str;
    /// The tab title, as markup.
    fn get_name(&self) -> &str;
    fn get_icon_name(&self) -> &str;
    fn get_help(&self) -> &str;
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str>;
    fn key_event(&self, _event: &gdk::EventKey) -> bool {
        false
    }
    /// The letter that selects the page with Alt.
    fn get_keybinding(&self) -> Option<char>;
    fn show(&self) {}
    /// Whether the page's help lists the keys every page shares after its
    /// own.
    fn lists_common_keys(&self) -> bool {
        true
    }
    /// Escape: true when the page took it and the launcher stays open,
    /// false to exit.
    fn cancel(&self) -> bool {
        false
    }
    /// Whether the launcher must stay open when it loses the focus, as
    /// while a change the page runs may ask for authorisation in a window
    /// of its own.
    fn keeps_open(&self) -> bool {
        false
    }
}

impl DialogPage for Page {
    fn get_root(&self) -> gtk::Widget {
        gtk::ListBox::new().upcast()
    }

    fn get_id(&self) -> &str {
        self.id
    }

    fn get_name(&self) -> &str {
        self.name
    }

    fn get_icon_name(&self) -> &str {
        self.icon
    }

    fn get_help(&self) -> &str {
        self.help
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        self.bindings.iter().copied().collect()
    }

    fn get_keybinding(&self) -> Option<char> {
        Some(self.key)
    }
}

pub const PAGES: &[Page] = &[
    Page { id: "apps", name: "<u>A</u>pplications", icon: "view-app-grid-symbolic", key: 'a', help: "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it.", bindings: &[("enter", "Launch Application"), ("ctrl+delete", "Hide Application"), ("ctrl+z", "Undo Hide")] },
    Page { id: "terminal", name: "<u>C</u>ommands", icon: "utilities-terminal", key: 'c', help: "This dialog allows for the executing commands on the user path. Initially all commands are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it. If no item matches, enter runs the typed command line, including any arguments.", bindings: &[("enter", "Execute Command")] },
    Page { id: "notifications", name: "<u>N</u>otifications", icon: "mail-message-new", key: 'n', help: "This dialog manages desktop notifications. Initially all desktop apps are presented. The user may filter the list in the top text box.", bindings: &[("del", "Delete Notification"), ("shift del", "Delete All Notifications")] },
    Page { id: "keybindings", name: "<u>K</u>eybindings", icon: "input-keyboard", key: 'k', help: "This dialog allows for viewing desktop keybindings. Initially all keybindings are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will perform the keybinding action.", bindings: &[("enter", "Invoke Keybinding")] },
    Page { id: "textlist", name: "TextList", icon: "utilities-terminal", key: 't', help: "This dialog presents a filterable list of items to select from. If no item matches, enter returns the typed text.", bindings: &[("enter", "Select Item")] },
    Page { id: "windows", name: "<u>W</u>indows", icon: "window-new", key: 'w', help: "This dialog allows for navigating between open windows in the desktop environment.  Selecting a window from the list will cause it to be viewed on screen and focused.", bindings: &[("enter", "Navigate to Window")] },
    Page { id: "tracker", name: "<u>F</u>iles", icon: "emblem-documents", key: 'f', help: "This dialog allows for searching for files based on content. Type in a text query, hit enter and select from the list of files which contain the search string to open it.", bindings: &[("enter", "Open File")] },
    Page { id: "features", name: "F<u>e</u>atures", icon: "system-software-install", key: 'e', help: "This dialog lists the optional Regolith features for the running session: looks, bar blocks, compositors and window manager extensions. Installed features are marked. The user may filter the list in the top text box. Enter installs the selected feature, or removes it if it is installed, asking first when other packages would be removed. Escape cancels a question or a running change.", bindings: &[("enter", "Install or Remove Package"), ("escape", "Cancel")] },
];

/// The pages that need i3 or sway, which are not offered under any other
/// window manager.
const WM_PAGE_IDS: &[&str] = &["keybindings", "windows"];

impl Page {
    /// Whether the page works under the window manager `wm_name`: `i3`,
    /// `sway`, or `Unknown` for any other.
    pub fn available_under(&self, wm_name: &str) -> bool {
        !WM_PAGE_IDS.contains(&self.id) || matches!(wm_name, "i3" | "sway")
    }
}

/// The keys that act on the highlighted row: Return and the keypad Enter.
pub fn is_enter_key(key: &gdk::keys::Key) -> bool {
    matches!(
        *key,
        gdk::keys::constants::Return
            | gdk::keys::constants::KP_Enter
            | gdk::keys::constants::ISO_Enter
    )
}

/// Highlights the first row of `view`, the best match for a new filter
/// text, and returns its path. The arrow keys and Ctrl+n/p move the view's
/// cursor: a refilter keeps that row highlighted when it still matches, or
/// moves the highlight to a neighbouring row when it does not.
pub fn select_first_row(view: &gtk::TreeView) -> gtk::TreePath {
    let first = gtk::TreePath::new_first();
    if view.model().and_then(|model| model.iter_first()).is_some() {
        // Also selects the row and scrolls to it.
        view.set_cursor(&first, None::<&gtk::TreeViewColumn>, false);
    }
    first
}

/// Highlights the row at `position` of `view`, or its last row when it has
/// no row there, and returns its path; with no rows, nothing is highlighted.
pub fn select_row_near(view: &gtk::TreeView, position: i32) -> Option<gtk::TreePath> {
    let selection = view.selection();
    selection.set_mode(gtk::SelectionMode::Single);
    let rows = view.model().map_or(0, |model| model.iter_n_children(None));
    if rows == 0 {
        selection.unselect_all();
        return None;
    }
    let path = gtk::TreePath::from_indicesv(&[position.clamp(0, rows - 1)]);
    view.set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
    Some(path)
}

/// The position of the highlighted row of `view`.
pub fn selected_position(view: &gtk::TreeView) -> Option<i32> {
    let (model, iter) = view.selection().selected()?;
    model.path(&iter)?.indices().first().copied()
}

/// Rebuilds the rows `view` shows through `filter` for a cause other than a
/// new filter text: `rebuild` sorts and refilters them. The row highlighted
/// before stays highlighted while it is still listed, wherever it now
/// stands. Otherwise the row now at its position is highlighted, or the
/// last row when the list has become shorter, or none when it is empty.
/// With nothing highlighted before, the first row is. Returns the path
/// highlighted.
pub fn rebuild_keeping_selection(
    view: &gtk::TreeView,
    filter: &gtk::TreeModelFilter,
    rebuild: impl FnOnce(),
) -> Option<gtk::TreePath> {
    // The child model's iters last as long as its rows, whatever their order
    // and whether the filter shows them.
    let before = view
        .selection()
        .selected()
        .map(|(_, iter)| filter.convert_iter_to_child_iter(&iter));
    let position = selected_position(view).unwrap_or(0);
    rebuild();
    let kept = before
        .and_then(|child| filter.convert_child_iter_to_iter(&child))
        .and_then(|iter| filter.path(&iter));
    match kept {
        Some(path) => {
            view.selection().set_mode(gtk::SelectionMode::Single);
            view.set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
            Some(path)
        }
        None => select_row_near(view, position),
    }
}

/// The filter of a page whose rows match through folded copies of their
/// text, kept in model columns. With subsequence matching on, rows that
/// match only by subsequence are listed after the others.
pub struct RowFilter {
    words: RefCell<Vec<String>>,
    folded_columns: &'static [u32],
    subsequence: Cell<bool>,
}

impl RowFilter {
    pub fn new(folded_columns: &'static [u32], subsequence: bool) -> Rc<Self> {
        Rc::new(Self {
            words: RefCell::default(),
            folded_columns,
            subsequence: Cell::new(subsequence),
        })
    }

    /// Turns subsequence matching on or off; whether that changed it.
    pub fn set_subsequence(&self, subsequence: bool) -> bool {
        self.subsequence.replace(subsequence) != subsequence
    }

    /// Takes the words of a new filter text.
    pub fn set_text(&self, text: &str) {
        *self.words.borrow_mut() = query_words(text);
    }

    /// How the row at `iter` matches the filter words, or `None` when it
    /// is not listed.
    pub fn row_match(&self, model: &gtk::TreeModel, iter: &gtk::TreeIter) -> Option<Match> {
        let fields: Vec<String> = self
            .folded_columns
            .iter()
            .map(|&column| {
                model
                    .value(iter, column as i32)
                    .get::<String>()
                    .unwrap_or_default()
            })
            .collect();
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        match_fields(&self.words.borrow(), &fields, self.subsequence.get())
    }

    /// Sorts `model` by how its rows match (see [`Match`]), then by the text
    /// in `column` as GTK sorts strings: the page's usual order. Call it
    /// again after `set_text` to sort the rows for the new words.
    pub fn sort(self: &Rc<Self>, model: &gtk::ListStore, column: u32) {
        let row_filter = self.clone();
        model.set_sort_func(gtk::SortColumn::Index(column), move |model, a, b| {
            let text = |iter| {
                model
                    .value(iter, column as i32)
                    .get::<glib::GString>()
                    .unwrap_or_default()
            };
            row_filter
                .row_match(model, a)
                .cmp(&row_filter.row_match(model, b))
                .then_with(|| text(a).as_gstr().collate(text(b)))
        });
    }
}

/// Keeps a page that filters through `row_filter` in step with the
/// `fuzzy-matching` setting while the launcher runs: when another program
/// changes it, the rows are matched and sorted again for the same filter
/// text, keeping the highlighted row (see [`rebuild_keeping_selection`]).
pub fn follow_fuzzy_setting(
    settings: &gio::Settings,
    row_filter: &Rc<RowFilter>,
    model: &gtk::ListStore,
    sort_column: u32,
    filter: &gtk::TreeModelFilter,
    view: &gtk::TreeView,
    path: &Rc<RefCell<gtk::TreePath>>,
) {
    let (row_filter, model, filter, view, path) = (
        row_filter.clone(),
        model.clone(),
        filter.clone(),
        view.clone(),
        path.clone(),
    );
    settings.connect_changed(Some(FUZZY_MATCHING), move |settings, _| {
        if row_filter.set_subsequence(settings.boolean(FUZZY_MATCHING)) {
            let selected = rebuild_keeping_selection(&view, &filter, || {
                row_filter.sort(&model, sort_column);
                filter.refilter();
            });
            *path.borrow_mut() = selected.unwrap_or_else(gtk::TreePath::new_first);
        }
    });
}

/// The setting that turns subsequence matching on.
pub const FUZZY_MATCHING: &str = "fuzzy-matching";

pub fn find(name: &str) -> Option<&'static Page> {
    PAGES.iter().find(|page| page.id.eq_ignore_ascii_case(name))
}

/// What the dialog lends its pages: closing the launcher, and letting go of
/// (`false`) or taking back (`true`) the keyboard and pointer it holds.
pub struct DialogHooks {
    pub quit: Rc<dyn Fn()>,
    pub hold_inputs: Rc<dyn Fn(bool)>,
}

pub fn create(
    page: &'static Page,
    args: &Args,
    settings: &gio::Settings,
    entry: &gtk::Entry,
    hooks: &DialogHooks,
    wm_name: &str,
    is_wayland: bool,
) -> Rc<dyn DialogPage> {
    let quit = hooks.quit.clone();
    match page.id {
        "apps" => Rc::new(ApplicationsPage::new(settings.clone(), entry.clone(), quit)),
        "terminal" => Rc::new(CommandPage::new(entry.clone(), args.has("-q"), quit)),
        "keybindings" => Rc::new(KeybindingsPage::new(
            settings.clone(),
            entry.clone(),
            wm_name.to_owned(),
            quit,
        )),
        "textlist" => TextListPage::new(settings.clone(), entry.clone(), args, quit),
        "notifications" => Rc::new(RoficationPage::new(settings.clone(), entry.clone(), quit)),
        "tracker" => TrackerPage::new(settings.clone(), entry.clone(), quit),
        "windows" => Rc::new(WindowsPage::new(
            settings.clone(),
            entry.clone(),
            wm_name.to_owned(),
            is_wayland,
            quit,
        )),
        "features" => Rc::new(FeaturesPage::new(
            entry.clone(),
            wm_name,
            is_wayland,
            hooks.hold_inputs.clone(),
        )),
        _ => Rc::new(*page),
    }
}

/// The page a page file defines; `key` is the letter that selects it.
pub fn create_from_file(
    file: &PageFile,
    key: Option<char>,
    settings: &gio::Settings,
    entry: &gtk::Entry,
    quit: Rc<dyn Fn()>,
) -> Rc<dyn DialogPage> {
    match file.rows {
        Rows::Apps(_) => Rc::new(AppListPage::new(
            settings.clone(),
            entry.clone(),
            file,
            key,
            quit,
        )),
        Rows::Items(_) | Rows::Command(_) => {
            TextListPage::from_page_file(settings.clone(), entry.clone(), file, key, quit)
        }
    }
}
