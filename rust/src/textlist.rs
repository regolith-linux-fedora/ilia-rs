use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::args::Args;
use crate::filter::{contains_all_words, query_words};
use crate::icons::load_icon_from_name;
use crate::keybindings::navigate;
use crate::pages::{is_enter_key, DialogPage};

const DEFAULT_ICON: &str = "utilities-terminal";
const FALLBACK_ICON: &str = "applications-other";

/// Inputs up to this many lines are filtered on every key; larger ones once
/// typing pauses for `FILTER_DELAY`.
const IMMEDIATE_FILTER_LINES: usize = 10_000;
const FILTER_DELAY: Duration = Duration::from_millis(100);

/// One input line, as shown, as matched and as printed.
struct Line {
    text: String,
    // The lowercased text, when it differs.
    folded: Option<String>,
    // The bytes read, when they are not UTF-8: the text shows them with
    // replacement characters, but the selection prints them back as read.
    raw: Option<Box<[u8]>>,
}

impl Line {
    fn new(bytes: Vec<u8>) -> Self {
        let (text, raw) = match String::from_utf8(bytes) {
            Ok(text) => (text, None),
            Err(error) => {
                let bytes = error.into_bytes();
                let text = String::from_utf8_lossy(&bytes).into_owned();
                (text, Some(bytes.into_boxed_slice()))
            }
        };
        let lowered = text.to_lowercase();
        let folded = (lowered != text).then_some(lowered);
        Self { text, folded, raw }
    }

    fn folded(&self) -> &str {
        self.folded.as_deref().unwrap_or(&self.text)
    }

    fn bytes(&self) -> &[u8] {
        self.raw.as_deref().unwrap_or(self.text.as_bytes())
    }
}

/// The lines on show: those matching the query words, in input order.
#[derive(Debug, Default, PartialEq)]
struct Shown {
    words: Vec<String>,
    lines: Vec<u32>,
}

impl Shown {
    fn all(lines: &[Line]) -> Self {
        Self {
            words: Vec::new(),
            lines: (0..lines.len() as u32).collect(),
        }
    }

    /// The lines matching `words`. When every word shown so far lies within
    /// one of the new words, the new matches are among the lines on show,
    /// and only those are checked.
    fn filtered(&self, lines: &[Line], words: Vec<String>) -> Self {
        let narrows = self
            .words
            .iter()
            .all(|old| words.iter().any(|new| new.contains(old.as_str())));
        let matches = |index: &u32| contains_all_words(lines[*index as usize].folded(), &words);
        let matching = if narrows {
            self.lines.iter().copied().filter(matches).collect()
        } else {
            (0..lines.len() as u32).filter(matches).collect()
        };
        Self {
            words,
            lines: matching,
        }
    }
}

pub struct TextListPage {
    entry: gtk::Entry,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    quit: Rc<dyn Fn()>,
    lines: Vec<Line>,
    // Every line, in input order: the rows for an empty query.
    all_rows: gtk::ListStore,
    shown: RefCell<Shown>,
    // A refilter waiting for typing to pause.
    pending: RefCell<Option<glib::SourceId>>,
    this: Weak<TextListPage>,
}

impl TextListPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        args: &Args,
        quit: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        // `-n` drops the icon column.
        let render_icon = !args.has("-n");
        // A bare `-i` clears the icon, which falls back to the generic one.
        let icon = if args.has("-i") {
            args.value("-i")
        } else {
            Some(DEFAULT_ICON)
        };

        let view = gtk::TreeView::new();
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        if render_icon {
            // Every row shows the same icon, so it is set on the renderer
            // rather than stored in each row.
            let icon_renderer = gtk::CellRendererPixbuf::new();
            icon_renderer.set_pixbuf(item_icon(icon, settings.int("icon-size")).as_ref());
            let icon_column = gtk::TreeViewColumn::with_attributes("Icon", &icon_renderer, &[]);
            icon_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&icon_column);
        }
        let text_renderer = gtk::CellRendererText::new();
        let text_column =
            gtk::TreeViewColumn::with_attributes("Name", &text_renderer, &[("text", 0)]);
        text_column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        view.append_column(&text_column);
        view.set_activate_on_single_click(true);

        // Startup blocks here until stdin reaches EOF, so the rows are
        // complete before the first selection and before the window shows.
        let lines: Vec<Line> = read_items(io::stdin().lock())
            .into_iter()
            .map(Line::new)
            .collect();
        let all_rows = rows_store(lines.iter());
        view.set_model(Some(&all_rows));

        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Rc::new_cyclic(|this| Self {
            entry,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            quit,
            shown: RefCell::new(Shown::all(&lines)),
            lines,
            all_rows,
            pending: RefCell::new(None),
            this: this.clone(),
        });
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let this = self.this.clone();
        self.view.connect_row_activated(move |_, path, _| {
            if let Some(page) = this.upgrade() {
                page.print_row(path);
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

    /// The index of the line shown at `path`.
    fn line_index(&self, path: &gtk::TreePath) -> Option<u32> {
        let row = usize::try_from(*path.indices().first()?).ok()?;
        self.shown.borrow().lines.get(row).copied()
    }

    fn print_row(&self, path: &gtk::TreePath) {
        if let Some(index) = self.line_index(path) {
            print_line(self.lines[index as usize].bytes(), &self.quit);
        }
    }

    fn selected_path(&self) -> Option<gtk::TreePath> {
        let (model, iter) = self.view.selection().selected()?;
        model.path(&iter)
    }

    /// Shows the lines matching the current text. The rows are built before
    /// the view gets them, so a large result costs one model change instead
    /// of a signal per row. A highlighted line that still matches stays
    /// highlighted.
    fn refilter(&self) {
        if let Some(timer) = self.pending.take() {
            timer.remove();
        }
        let words = query_words(self.entry.text().as_str());
        if words != self.shown.borrow().words {
            let highlighted = self.selected_path().and_then(|path| self.line_index(&path));
            let shown = self.shown.borrow().filtered(&self.lines, words);
            let rows = if shown.words.is_empty() {
                self.all_rows.clone()
            } else {
                rows_store(shown.lines.iter().map(|index| &self.lines[*index as usize]))
            };
            let position = highlighted.and_then(|line| shown.lines.binary_search(&line).ok());
            *self.shown.borrow_mut() = shown;
            self.view.set_model(Some(&rows));
            if let Some(position) = position {
                let path = gtk::TreePath::from_indicesv(&[position as i32]);
                self.view.selection().select_path(&path);
            }
        }
        self.set_selection();
    }

    /// Applies a refilter still waiting for typing to pause, before a key
    /// acts on the rows.
    fn flush(&self) {
        if self.pending.borrow().is_some() {
            self.refilter();
        }
    }
}

impl DialogPage for TextListPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        if self.lines.len() <= IMMEDIATE_FILTER_LINES {
            self.refilter();
            return;
        }
        // Large inputs are filtered once typing pauses, so a burst of keys
        // costs one pass over the lines.
        if let Some(timer) = self.pending.take() {
            timer.remove();
        }
        let this = self.this.clone();
        let timer = glib::timeout_add_local_once(FILTER_DELAY, move || {
            if let Some(page) = this.upgrade() {
                page.pending.take();
                page.refilter();
            }
        });
        self.pending.replace(Some(timer));
    }
    fn on_entry_activated(&self) {
        self.flush();
        if let Some(path) = self.selected_path() {
            self.print_row(&path);
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
        "This dialog presents a filterable list of items to select from. If no item matches, enter returns the typed text."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Select Item")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        let enter = is_enter_key(&event.keyval());
        if enter || event.state().contains(gdk::ModifierType::CONTROL_MASK) {
            self.flush();
        }
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        if enter && self.shown.borrow().lines.is_empty() && !self.entry.text().is_empty() {
            print_line(self.entry.text().as_bytes(), &self.quit);
            return true;
        }
        false
    }
    fn get_keybinding(&self) -> char {
        't'
    }
    fn show(&self) {
        self.flush();
        self.view.grab_focus();
    }
}

/// Reads newline-terminated items until EOF, as bytes. Like the C-string
/// lines of the reference, an item ends at its first NUL byte; `\r` is kept
/// verbatim.
fn read_items(mut reader: impl BufRead) -> Vec<Vec<u8>> {
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
                items.push(line[..end].to_vec());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    items
}

/// A list of `lines` for the view.
fn rows_store<'a>(lines: impl Iterator<Item = &'a Line>) -> gtk::ListStore {
    let store = gtk::ListStore::new(&[String::static_type()]);
    for line in lines {
        store.insert_with_values(None, &[(0, &line.text)]);
    }
    store
}

fn item_icon(icon: Option<&str>, icon_size: i32) -> Option<Pixbuf> {
    let theme = gtk::IconTheme::default()?;
    icon.filter(|icon| !icon.is_empty())
        .and_then(|icon| load_icon_from_name(&theme, icon, icon_size))
        .or_else(|| load_icon_from_name(&theme, FALLBACK_ICON, icon_size))
}

fn print_line(line: &[u8], quit: &Rc<dyn Fn()>) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(line);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
    drop(stdout);
    quit();
}

#[cfg(test)]
mod tests {
    use super::{read_items, Line, Shown};
    use crate::filter::query_words;

    const FIXTURE: &str = "Alpha\nNeedle Item\nOmega\n";

    fn lines(items: &[&str]) -> Vec<Line> {
        items
            .iter()
            .map(|item| Line::new(item.as_bytes().to_vec()))
            .collect()
    }

    // The texts of the lines a query shows, starting from every line.
    fn shown<'a>(lines: &'a [Line], query: &str) -> Vec<&'a str> {
        Shown::all(lines)
            .filtered(lines, query_words(query))
            .lines
            .iter()
            .map(|index| lines[*index as usize].text.as_str())
            .collect()
    }

    #[test]
    fn items_are_read_line_by_line_to_eof() {
        assert_eq!(
            read_items(FIXTURE.as_bytes()),
            [&b"Alpha"[..], b"Needle Item", b"Omega"]
        );
        assert_eq!(read_items(&b"a\n\nb"[..]), [&b"a"[..], b"", b"b"]);
        assert_eq!(read_items(&b"dos\r\n"[..]), [b"dos\r"]);
        assert_eq!(read_items(&b"nul\0tail\n"[..]), [b"nul"]);
        assert_eq!(read_items(&b"caf\xe9\n"[..]), [b"caf\xe9"]);
        assert!(read_items(&b""[..]).is_empty());
    }

    #[test]
    fn lines_that_are_not_utf8_print_back_as_read() {
        let line = Line::new(b"CAF\xe9-latin1.txt".to_vec());
        assert_eq!(line.text, "CAF\u{fffd}-latin1.txt");
        assert_eq!(line.folded(), "caf\u{fffd}-latin1.txt");
        assert_eq!(line.bytes(), b"CAF\xe9-latin1.txt");
        let line = Line::new("Café".as_bytes().to_vec());
        assert_eq!(line.bytes(), "Café".as_bytes());
        assert!(line.raw.is_none());
    }

    #[test]
    fn filter_matches_every_word_ignoring_case() {
        let items = lines(&["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, ""), ["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, "   "), ["Alpha", "Needle Item"]);
        assert_eq!(shown(&items, "  NEEDLE "), ["Needle Item"]);
        assert_eq!(shown(&items, "le it"), ["Needle Item"]);
        assert!(shown(&items, "needle item x").is_empty());
        // Whitespace separates words; it is not matched itself.
        assert!(shown(&items, "needleitem").is_empty());
        assert_eq!(shown(&items, "item NEEDLE"), ["Needle Item"]);
        let items = lines(&["Éléphant Café à Paris", "Café elsewhere"]);
        assert_eq!(shown(&items, "CAFÉ ÉLÉ"), ["Éléphant Café à Paris"]);
    }

    #[test]
    fn selection_follows_first_visible_row() {
        let items = lines(&["Alpha", "Needle Item", "Omega"]);
        let first = |query: &str| shown(&items, query).first().copied();
        assert_eq!(first(""), Some("Alpha"));
        assert_eq!(first("needle"), Some("Needle Item"));
        assert_eq!(first("OMEGA"), Some("Omega"));
        assert_eq!(first("a"), Some("Alpha"));
        assert_eq!(first("unmatched free text"), None);
        assert_eq!(shown(&[], "").first(), None);
    }

    #[test]
    fn typing_on_narrows_and_editing_widens_the_rows() {
        let items = lines(&["needle a", "needle b", "hay", "Needle backup", "b needle"]);
        let all = Shown::all(&items);
        let needle = all.filtered(&items, query_words("needle"));
        assert_eq!(needle.lines, [0, 1, 3, 4]);
        let narrowed = needle.filtered(&items, query_words("needle b"));
        assert_eq!(narrowed.lines, [1, 3, 4]);
        // Removing a word, or changing it to one the shown rows were not
        // filtered for, checks every line again.
        assert_eq!(narrowed.filtered(&items, query_words("hay")).lines, [2]);
        assert_eq!(
            narrowed.filtered(&items, query_words("")).lines,
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            narrowed
                .filtered(&items, query_words("needle backup"))
                .lines,
            [3]
        );
    }
}
