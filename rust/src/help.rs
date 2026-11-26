//! The Help page: always the last tab, it describes the page that was in
//! view right before it, with that page's help text and its keys, then the
//! keys every page shares.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;

use crate::keybindings::navigate;
use crate::pages::{select_first_row, DialogPage};

/// The letter that selects the Help page with Alt, unless another page
/// has it.
pub const LETTER: char = 'h';

/// The keys that work on every page, listed after the described page's
/// own keys.
const SHARED_KEYS: &[(&str, &str)] = &[
    ("↑ ↓", "Change Selected Item"),
    ("Page Up Page Down", "Move One Page"),
    ("Home End", "Select First or Last Item"),
    ("Ctrl+p Ctrl+n", "Change Selected Item (emacs style)"),
    ("Ctrl+k Ctrl+j", "Change Selected Item (vim style)"),
    ("← →", "Switch Page"),
];

/// The dialog's own keys, which close the list.
const DIALOG_KEYS: &[(&str, &str)] = &[
    ("Alt -", "Decrease Dialog Size"),
    ("Alt +", "Increase Dialog Size"),
    ("Esc", "Exit"),
];

/// The rows the list keeps room for however small the dialog, so that Page
/// Up and Page Down still move by more than one row.
const MIN_VISIBLE_ROWS: i32 = 4;

const KEY: u32 = 0;
const ACTION: u32 = 1;

/// The text of a tab title given as markup, without its markup: the
/// underline of the letter that selects the page goes.
fn plain_name(markup: &str) -> String {
    gtk::pango::parse_markup(markup, '\0')
        .map(|(_, text, _)| text.to_string())
        .unwrap_or_else(|_| markup.to_owned())
}

/// The rows of the key list for a page whose own keys are `own`, in a
/// dialog whose tabs are `tabs`: each tab's plain title and the letter
/// that selects it, if any, in tab order.
fn key_rows(
    own: &BTreeMap<&'static str, &'static str>,
    tabs: &[(String, Option<char>)],
) -> Vec<(String, String)> {
    // The map keeps the page's keys ordered by their bytes.
    let own_rows = own
        .iter()
        .map(|(key, action)| ((*key).to_owned(), (*action).to_owned()));
    let shared = SHARED_KEYS
        .iter()
        .map(|(key, action)| ((*key).to_owned(), (*action).to_owned()));
    let letters = tabs.iter().filter_map(|(name, letter)| {
        letter.map(|letter| (format!("Alt+{letter}"), format!("Show {name}")))
    });
    let dialog = DIALOG_KEYS
        .iter()
        .map(|(key, action)| ((*key).to_owned(), (*action).to_owned()));
    own_rows
        .chain(shared)
        .chain(letters)
        .chain(dialog)
        .collect()
}

pub struct HelpPage {
    key: Option<char>,
    // Every tab's plain title and letter, in tab order, Help included.
    tabs: Vec<(String, Option<char>)>,
    root: gtk::Widget,
    // Each page's title and help text, one stack child per page.
    texts: gtk::Stack,
    // The room the texts scroll in, back at the top for each page.
    text_area: gtk::ScrolledWindow,
    model: gtk::ListStore,
    view: gtk::TreeView,
    path: RefCell<gtk::TreePath>,
    // The page described.
    described: RefCell<Option<Rc<dyn DialogPage>>>,
}

impl HelpPage {
    /// The Help page of a dialog showing `pages`, in tab order, under the
    /// tab titles `names` (markup); `key` is Help's own letter.
    pub fn new(key: Option<char>, pages: &[Rc<dyn DialogPage>], names: &[String]) -> Self {
        let mut tabs: Vec<(String, Option<char>)> = pages
            .iter()
            .zip(names)
            .map(|(page, name)| (plain_name(name), page.get_keybinding()))
            .collect();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 5);

        // Every page's text is in place from the start, the widest setting
        // the width it takes: describing another page never makes the dialog
        // wider.
        let texts = gtk::Stack::new();
        texts.set_hhomogeneous(true);
        texts.set_vhomogeneous(false);
        for (index, (page, (name, _))) in pages.iter().zip(&tabs).enumerate() {
            let text_box = gtk::Box::new(gtk::Orientation::Vertical, 5);
            // The page's name, as plain text.
            let title = gtk::Label::new(Some(name));
            title.style_context().add_class("help_heading");
            title.set_xalign(0.0);
            title.set_line_wrap(true);
            title.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
            text_box.pack_start(&title, false, false, 0);
            let text = gtk::Label::new(Some(page.get_help()));
            text.set_xalign(0.0);
            text.set_line_wrap(true);
            text.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
            text_box.pack_start(&text, false, false, 0);
            texts.add_named(&text_box, &index.to_string());
        }
        // The text takes the room it needs above the list when the dialog
        // has it, but scrolls rather than hold the dialog at a bigger size:
        // a page's name can run to any number of lines.
        let text_area =
            gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        text_area.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        text_area.set_propagate_natural_height(true);
        let viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        viewport.set_shadow_type(gtk::ShadowType::None);
        viewport.add(&texts);
        text_area.add(&viewport);
        root.pack_start(&text_area, false, false, 0);

        let heading = gtk::Label::new(Some("Keys"));
        heading.style_context().add_class("help_heading");
        heading.set_xalign(0.0);
        root.pack_start(&heading, false, false, 0);

        let model = gtk::ListStore::new(&[glib::Type::STRING, glib::Type::STRING]);
        let view = gtk::TreeView::with_model(&model);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        // Without a search column Ctrl+f cannot open GTK's search box either
        // (the entry, which takes the key on the other pages, is off here).
        view.set_search_column(-1);
        view.selection().set_mode(gtk::SelectionMode::Single);
        for (column_index, title) in [(KEY, "Key"), (ACTION, "Function")] {
            let renderer = gtk::CellRendererText::new();
            let column = gtk::TreeViewColumn::with_attributes(
                title,
                &renderer,
                &[("text", column_index as i32)],
            );
            // A fixed-height list needs fixed-size columns.
            column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&column);
        }
        view.connect_realize(|view| view.columns_autosize());
        // The list scrolls in the room the text above it leaves, so it never
        // makes the dialog bigger.
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        // A row is a line of text and the cell's padding above and below it.
        let (_, line_height) = view.create_pango_layout(Some("Ag")).pixel_size();
        let (_, cell_padding) = gtk::CellRendererText::new().padding();
        scrolled.set_min_content_height(MIN_VISIBLE_ROWS * (line_height + 2 * cell_padding));
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);
        root.pack_start(&scrolled, true, true, 0);

        tabs.push(("Help".to_owned(), key));
        Self {
            key,
            tabs,
            root: root.upcast(),
            texts,
            text_area,
            model,
            view,
            path: RefCell::new(gtk::TreePath::new_first()),
            described: RefCell::new(None),
        }
    }

    /// Shows the help of `page`, whose tab is at `index`, and highlights the
    /// first row of its keys.
    pub fn describe(&self, index: usize, page: &Rc<dyn DialogPage>) {
        self.texts.set_visible_child_name(&index.to_string());
        let scroll = self.text_area.vadjustment();
        scroll.set_value(scroll.lower());
        self.model.clear();
        for (key, action) in key_rows(&page.get_keybindings(), &self.tabs) {
            let row = self.model.append();
            self.model.set(&row, &[(KEY, &key), (ACTION, &action)]);
        }
        *self.path.borrow_mut() = select_first_row(&self.view);
        *self.described.borrow_mut() = Some(page.clone());
    }
}

impl DialogPage for HelpPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn get_id(&self) -> &str {
        "help"
    }

    fn get_name(&self) -> &str {
        if self.key.is_some() {
            "<u>H</u>elp"
        } else {
            "Help"
        }
    }

    fn get_icon_name(&self) -> &str {
        "help-browser"
    }

    fn get_help(&self) -> &str {
        ""
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        BTreeMap::new()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        navigate(&self.view, &self.path, event)
    }

    /// The described page's own keys stay with ilia on X11 even when i3
    /// binds them, as on that page; here they do nothing.
    fn uses_key(&self, event: &gdk::EventKey) -> bool {
        self.described
            .borrow()
            .as_ref()
            .is_some_and(|page| page.uses_key(event))
    }

    fn get_keybinding(&self) -> Option<char> {
        self.key
    }

    fn show(&self) {
        self.view.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::{key_rows, plain_name};
    use std::collections::BTreeMap;

    #[test]
    fn plain_name_drops_markup() {
        assert_eq!(plain_name("<u>A</u>pplications"), "Applications");
        assert_eq!(plain_name("Ca<u>l</u>culator"), "Calculator");
        assert_eq!(plain_name("Tools &lt;b&gt; &amp; _Ω"), "Tools <b> & _Ω");
    }

    #[test]
    fn key_rows_list_own_shared_letter_and_dialog_keys_in_order() {
        let own: BTreeMap<_, _> = [("enter", "Launch"), ("ctrl+z", "Undo"), ("ctrl+=", "Add")]
            .into_iter()
            .collect();
        let tabs = vec![
            ("Applications".to_owned(), Some('a')),
            ("No Letter".to_owned(), None),
            ("Help".to_owned(), Some('h')),
        ];
        let rows = key_rows(&own, &tabs);
        let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ctrl+=",
                "ctrl+z",
                "enter",
                "↑ ↓",
                "Page Up Page Down",
                "Home End",
                "Ctrl+p Ctrl+n",
                "Ctrl+k Ctrl+j",
                "← →",
                "Alt+a",
                "Alt+h",
                "Alt -",
                "Alt +",
                "Esc",
            ]
        );
        assert_eq!(rows[9].1, "Show Applications");
        assert_eq!(rows[10].1, "Show Help");
        assert_eq!(rows[13].1, "Exit");
    }

    #[test]
    fn key_rows_without_own_keys_start_with_the_shared_keys() {
        let rows = key_rows(&BTreeMap::new(), &[("Help".to_owned(), None)]);
        assert_eq!(
            rows[0],
            ("↑ ↓".to_owned(), "Change Selected Item".to_owned())
        );
        assert_eq!(rows.len(), 9);
    }
}
