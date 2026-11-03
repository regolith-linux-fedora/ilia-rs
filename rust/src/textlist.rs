use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::args::Args;
use crate::icons::load_icon_from_name;
use crate::keybindings::navigate;
use crate::pages::DialogPage;

const DEFAULT_ICON: &str = "utilities-terminal";
const FALLBACK_ICON: &str = "applications-other";

pub struct TextListPage {
    entry: gtk::Entry,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    name_column: i32,
    quit: Rc<dyn Fn()>,
}

impl TextListPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        args: &Args,
        quit: Rc<dyn Fn()>,
    ) -> Self {
        // `-n` drops the icon column so the item text becomes column zero.
        let render_icon = !args.has("-n");
        let name_column = if render_icon { 1 } else { 0 };
        // A bare `-i` clears the icon, which falls back to the generic one.
        let icon = if args.has("-i") {
            args.value("-i")
        } else {
            Some(DEFAULT_ICON)
        };

        let model = if render_icon {
            gtk::ListStore::new(&[Pixbuf::static_type(), String::static_type()])
        } else {
            gtk::ListStore::new(&[String::static_type()])
        };
        let filter = gtk::TreeModelFilter::new(&model, None);
        let entry_for_filter = entry.clone();
        filter.set_visible_func(move |tree_model, iter| {
            let item = tree_model
                .value(iter, name_column)
                .get::<String>()
                .unwrap_or_default();
            item_matches(entry_for_filter.text().as_str(), &item)
        });

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        if render_icon {
            let icon_renderer = gtk::CellRendererPixbuf::new();
            let icon_column =
                gtk::TreeViewColumn::with_attributes("Icon", &icon_renderer, &[("pixbuf", 0)]);
            icon_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&icon_column);
        }
        let text_renderer = gtk::CellRendererText::new();
        let text_column =
            gtk::TreeViewColumn::with_attributes("Name", &text_renderer, &[("text", name_column)]);
        text_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&text_column);
        view.set_activate_on_single_click(true);

        // Startup blocks here until stdin reaches EOF, so the model is
        // complete before the first selection and before the window shows.
        let pixbuf = if render_icon {
            item_icon(icon, settings.int("icon-size"))
        } else {
            None
        };
        for item in read_items(io::stdin().lock()) {
            let row = model.append();
            match &pixbuf {
                Some(pixbuf) => model.set(&row, &[(0, pixbuf), (1, &item)]),
                None => model.set(&row, &[(name_column as u32, &item)]),
            }
        }

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
            name_column,
            quit,
        };
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let name_column = self.name_column;
        let quit = self.quit.clone();
        self.view.connect_row_activated(move |view, path, _| {
            let Some(model) = view.model() else {
                return;
            };
            if let Some(iter) = model.iter(path) {
                print_selection(&model, &iter, name_column, &quit);
            }
        });
    }

    fn set_selection(&self) {
        let selection = self.view.selection();
        let (selected, _) = selection.selected_rows();
        if let Some(path) = selected.first() {
            self.view
                .scroll_to_cell(Some(path), None::<&gtk::TreeViewColumn>, false, 0.0, 0.0);
            return;
        }
        selection.set_mode(gtk::SelectionMode::Single);
        selection.select_path(&self.path.borrow());
    }
}

impl DialogPage for TextListPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        self.filter.refilter();
        self.set_selection();
    }
    fn on_entry_activated(&self) {
        let iter = self.filter.iter(&self.path.borrow());
        if let Some(iter) = iter {
            print_selection(&self.filter, &iter, self.name_column, &self.quit);
        }
    }
    fn get_id(&self) -> &'static str {
        "textlist"
    }
    fn get_name(&self) -> &'static str {
        "TextList"
    }
    fn get_icon_name(&self) -> &'static str {
        DEFAULT_ICON
    }
    fn get_help(&self) -> &'static str {
        "This dialog presents a filterable list of items to select from."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Select Item")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        if event.keyval() == gdk::keys::constants::Return
            && self.filter.iter_first().is_none()
            && !self.entry.text().is_empty()
        {
            print_line(self.entry.text().as_str(), &self.quit);
            return true;
        }
        false
    }
    fn get_keybinding(&self) -> char {
        't'
    }
    fn show(&self) {
        self.view.grab_focus();
    }
}

/// Reads newline-terminated items until EOF. Like the C-string lines of the
/// reference, an item ends at its first NUL byte; `\r` is kept verbatim.
fn read_items(mut reader: impl BufRead) -> Vec<String> {
    let mut items = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                let end = line
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(line.len());
                items.push(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    items
}

/// Case-insensitive substring match of the stripped query; an empty query
/// matches every item.
fn item_matches(query: &str, item: &str) -> bool {
    let query = query.to_lowercase();
    let query = query.trim();
    query.is_empty() || item.to_lowercase().contains(query)
}

fn item_icon(icon: Option<&str>, icon_size: i32) -> Option<Pixbuf> {
    let theme = gtk::IconTheme::default()?;
    icon.filter(|icon| !icon.is_empty())
        .and_then(|icon| load_icon_from_name(&theme, icon, icon_size))
        .or_else(|| load_icon_from_name(&theme, FALLBACK_ICON, icon_size))
}

fn print_selection(
    model: &impl IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
    name_column: i32,
    quit: &Rc<dyn Fn()>,
) {
    if let Ok(item) = model.value(iter, name_column).get::<String>() {
        print_line(&item, quit);
    }
}

fn print_line(line: &str, quit: &Rc<dyn Fn()>) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    drop(stdout);
    quit();
}

#[cfg(test)]
mod tests {
    use super::{item_matches, read_items};

    const FIXTURE: &str = "Alpha\nNeedle Item\nOmega\n";

    // The entry-activation row: the stored first path over the visible rows.
    fn selected<'a>(items: &'a [String], query: &str) -> Option<&'a str> {
        items
            .iter()
            .filter(|item| item_matches(query, item))
            .map(String::as_str)
            .next()
    }

    #[test]
    fn items_are_read_line_by_line_to_eof() {
        assert_eq!(
            read_items(FIXTURE.as_bytes()),
            ["Alpha", "Needle Item", "Omega"]
        );
        assert_eq!(read_items("a\n\nb".as_bytes()), ["a", "", "b"]);
        assert_eq!(read_items("dos\r\n".as_bytes()), ["dos\r"]);
        assert_eq!(read_items("nul\0tail\n".as_bytes()), ["nul"]);
        assert!(read_items("".as_bytes()).is_empty());
    }

    #[test]
    fn filter_is_case_insensitive_stripped_substring() {
        assert!(item_matches("", "Alpha"));
        assert!(item_matches("   ", "Alpha"));
        assert!(item_matches("needle", "Needle Item"));
        assert!(item_matches("  NEEDLE ", "Needle Item"));
        assert!(item_matches("le it", "Needle Item"));
        assert!(!item_matches("needle", "Alpha"));
        assert!(!item_matches("needle item x", "Needle Item"));
        // Inner whitespace is significant; only the ends are stripped.
        assert!(!item_matches("needleitem", "Needle Item"));
    }

    #[test]
    fn selection_follows_first_visible_row() {
        let items = read_items(FIXTURE.as_bytes());
        assert_eq!(selected(&items, ""), Some("Alpha"));
        assert_eq!(selected(&items, "needle"), Some("Needle Item"));
        assert_eq!(selected(&items, "OMEGA"), Some("Omega"));
        assert_eq!(selected(&items, "a"), Some("Alpha"));
        assert_eq!(selected(&items, "unmatched free text"), None);
        assert_eq!(selected(&[], ""), None);
    }
}
