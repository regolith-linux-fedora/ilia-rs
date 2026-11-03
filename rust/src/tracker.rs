use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use ilia_tracker_sys::{Connection, Cursor};

use crate::icons::load_icon_from_name;
use crate::keybindings::navigate;
use crate::pages::DialogPage;

const ICON: u32 = 0;
const TITLE: u32 = 1;
const FILE: u32 = 2;

const MINER_SERVICE: &str = "org.freedesktop.Tracker3.Miner.Files";
const FALLBACK_MIME: &str = "application/octet-stream";
const FALLBACK_ICON: &str = "text-x-generic";

pub struct TrackerPage {
    entry: gtk::Entry,
    model: gtk::ListStore,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    icon_theme: Option<gtk::IconTheme>,
    icon_size: i32,
    post_launch_sleep: u64,
    quit: Rc<dyn Fn()>,
}

impl TrackerPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Self {
        let icon_size = settings.int("icon-size");
        // `GLib.Thread.usleep` takes an unsigned long, so a negative setting
        // wraps exactly like the C int conversion does.
        let post_launch_sleep = settings.int("post-launch-sleep") as u64;

        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            String::static_type(),
        ]);

        // The view sits directly on the store: results come from Tracker,
        // not from filtering local rows.
        let view = gtk::TreeView::with_model(&model);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        for (title, renderer, attribute, column) in [
            (
                "Icon",
                gtk::CellRendererPixbuf::new().upcast::<gtk::CellRenderer>(),
                "pixbuf",
                ICON,
            ),
            (
                "Title",
                gtk::CellRendererText::new().upcast(),
                "text",
                TITLE,
            ),
            ("Path", gtk::CellRendererText::new().upcast(), "text", FILE),
        ] {
            let column = gtk::TreeViewColumn::with_attributes(
                title,
                &renderer,
                &[(attribute, column as i32)],
            );
            column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&column);
        }
        view.set_activate_on_single_click(true);

        model.set_sort_column_id(gtk::SortColumn::Index(TITLE), gtk::SortType::Ascending);

        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Self {
            entry,
            model,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            icon_theme: gtk::IconTheme::default(),
            icon_size,
            post_launch_sleep,
            quit,
        };
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let post_launch_sleep = self.post_launch_sleep;
        let quit = self.quit.clone();
        self.view.connect_row_activated(move |view, path, _| {
            let Some(model) = view.model() else {
                return;
            };
            if let Some(iter) = model.iter(path) {
                let file = string_value(&model, &iter, FILE as i32);
                execute_app(&file, post_launch_sleep, &quit);
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

    fn full_text_search(&self) {
        let result = search(open_tracker, self.entry.text().as_str(), |hit| {
            self.append(hit)
        });
        if let Err(message) = result {
            eprint!("{}", error_line(&message));
        }
    }

    fn append(&self, hit: Hit) {
        let icon = self.icon_for(&hit.mime);
        let row = self.model.append();
        self.model.set(
            &row,
            &[(ICON, &icon), (TITLE, &hit.title), (FILE, &hit.path)],
        );
    }

    // Try every themed name GIO derives for the MIME type, then the generic
    // text icon, each through the Vala `load_icon_from_name` order.
    fn icon_for(&self, mime: &str) -> Option<Pixbuf> {
        let theme = self.icon_theme.as_ref()?;
        let icon = gio::content_type_get_icon(mime);
        let names = icon
            .downcast_ref::<gio::ThemedIcon>()
            .map(gio::ThemedIcon::names)
            .unwrap_or_default();
        names
            .iter()
            .find_map(|name| load_icon_from_name(theme, name, self.icon_size))
            .or_else(|| load_icon_from_name(theme, FALLBACK_ICON, self.icon_size))
    }
}

impl DialogPage for TrackerPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }
    fn on_entry_changed(&self) {
        // Vala `string.length` counts bytes, not characters.
        if self.entry.text().len() > 2 {
            self.model.clear();
            self.full_text_search();
            self.set_selection();
        }
    }
    fn on_entry_activated(&self) {
        if let Some(iter) = self.model.iter(&self.path.borrow()) {
            let file = string_value(&self.model, &iter, FILE as i32);
            execute_app(&file, self.post_launch_sleep, &self.quit);
        }
    }
    fn get_id(&self) -> &'static str {
        "tracker"
    }
    fn get_name(&self) -> &'static str {
        "<u>F</u>iles"
    }
    fn get_icon_name(&self) -> &'static str {
        "emblem-documents"
    }
    fn get_help(&self) -> &'static str {
        "This dialog allows for searching for files based on content. Type in a text query, hit enter and select from the list of files which contain the search string to open it."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Open File")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.view, &self.path, event) {
            return true;
        }
        // The dialog shell consumes Enter before pages see it, so the Vala
        // empty-model Enter branch is unreachable; keep it as a no-op.
        event.keyval() == gdk::keys::constants::Return
            && self.model.iter_first().is_none()
            && !self.entry.text().is_empty()
    }
    fn get_keybinding(&self) -> char {
        'f'
    }
    fn show(&self) {
        self.view.grab_focus();
    }
}

/// One Tracker result after the Vala per-row fallbacks.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    title: String,
    path: String,
    mime: String,
}

/// The row source walked by the search; Tracker cursors in production.
trait Rows {
    fn next(&mut self) -> Result<bool, String>;
    fn get_string(&self, column: i32) -> Option<&str>;
}

struct TrackerRows {
    // Declared first so the cursor is released before its connection.
    cursor: Cursor,
    _connection: Connection,
}

impl Rows for TrackerRows {
    fn next(&mut self) -> Result<bool, String> {
        self.cursor.next().map_err(|error| error.to_string())
    }
    fn get_string(&self, column: i32) -> Option<&str> {
        self.cursor.get_string(column)
    }
}

fn open_tracker(query: &str) -> Result<TrackerRows, String> {
    let connection = Connection::bus_new(MINER_SERVICE, None).map_err(|error| error.to_string())?;
    let cursor = connection.query(query).map_err(|error| error.to_string())?;
    Ok(TrackerRows {
        cursor,
        _connection: connection,
    })
}

/// The SPARQL sent to Tracker; the entry text is interpolated unescaped,
/// exactly as the Vala page does.
fn query_string(term: &str) -> String {
    format!(
        "SELECT DISTINCT nie:url(?f) nie:title(?f) nie:mimeType(?f) WHERE {{ ?f fts:match '{term}' }}"
    )
}

fn error_line(message: &str) -> String {
    format!("{message}\n")
}

/// Run the query and hand each row to `sink` as it is read. The first error
/// stops the walk; rows already delivered stay delivered.
fn search<R: Rows>(
    open: impl FnOnce(&str) -> Result<R, String>,
    term: &str,
    mut sink: impl FnMut(Hit),
) -> Result<(), String> {
    let mut rows = open(&query_string(term))?;
    while rows.next()? {
        let hit = hit_from_row(rows.get_string(0), rows.get_string(1), rows.get_string(2))
            .map_err(|error| error.message().to_owned())?;
        if let Some(hit) = hit {
            sink(hit);
        }
    }
    Ok(())
}

/// Rows without a URL are skipped; a missing title becomes the path's
/// basename and a missing MIME type becomes `application/octet-stream`.
fn hit_from_row(
    url: Option<&str>,
    title: Option<&str>,
    mime: Option<&str>,
) -> Result<Option<Hit>, glib::Error> {
    let Some(url) = url else {
        return Ok(None);
    };
    let (path, _) = glib::filename_from_uri(url)?;
    let title = title.map_or_else(|| basename(&path), str::to_owned);
    Ok(Some(Hit {
        title,
        path: path.to_string_lossy().into_owned(),
        mime: mime.unwrap_or(FALLBACK_MIME).to_owned(),
    }))
}

// `g_path_get_basename` semantics (glib-rs keeps its wrapper private):
// trailing slashes are ignored, an all-slash path is `/`, empty is `.`.
fn basename(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return ".".to_owned();
    }
    let Some(end) = bytes.iter().rposition(|&byte| byte != b'/') else {
        return "/".to_owned();
    };
    let start = bytes[..end]
        .iter()
        .rposition(|&byte| byte == b'/')
        .map_or(0, |slash| slash + 1);
    String::from_utf8_lossy(&bytes[start..=end]).into_owned()
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

// Open the file with its default handler, then wait `post-launch-sleep`
// microseconds and quit whether or not the launch worked.
fn execute_app(file: &str, post_launch_sleep: u64, quit: &Rc<dyn Fn()>) {
    // gio-rs folds the C `FALSE`-without-`GError` return into `Ok`, so only
    // the message-bearing failure line is reachable here.
    if let Err(error) = glib::filename_to_uri(file, None)
        .and_then(|uri| gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>))
    {
        eprintln!("Error: execute_app failed: {}", error.message());
    }
    glib::usleep(post_launch_sleep);
    quit();
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        basename, error_line, hit_from_row, query_string, search, Hit, Rows, MINER_SERVICE,
    };

    struct FakeRows {
        rows: Vec<[Option<&'static str>; 3]>,
        current: Option<usize>,
        fail_after: Option<usize>,
    }

    impl Rows for FakeRows {
        fn next(&mut self) -> Result<bool, String> {
            let next = self.current.map_or(0, |index| index + 1);
            if self.fail_after == Some(next) {
                return Err("cursor broke".to_owned());
            }
            self.current = Some(next);
            Ok(next < self.rows.len())
        }
        fn get_string(&self, column: i32) -> Option<&str> {
            self.rows[self.current?][column as usize]
        }
    }

    fn fake(rows: Vec<[Option<&'static str>; 3]>, fail_after: Option<usize>) -> FakeRows {
        FakeRows {
            rows,
            current: None,
            fail_after,
        }
    }

    fn hit(title: &str, path: &str, mime: &str) -> Hit {
        Hit {
            title: title.to_owned(),
            path: path.to_owned(),
            mime: mime.to_owned(),
        }
    }

    #[test]
    fn query_interpolates_raw_entry_text() {
        assert_eq!(
            query_string("regolith"),
            "SELECT DISTINCT nie:url(?f) nie:title(?f) nie:mimeType(?f) WHERE { ?f fts:match 'regolith' }"
        );
        assert_eq!(
            query_string("it's"),
            "SELECT DISTINCT nie:url(?f) nie:title(?f) nie:mimeType(?f) WHERE { ?f fts:match 'it's' }"
        );
    }

    #[test]
    fn rows_decode_uri_and_apply_fallbacks() {
        assert_eq!(
            hit_from_row(
                Some("file:///tmp/My%20Notes.txt"),
                Some("Notes"),
                Some("text/plain")
            )
            .unwrap(),
            Some(hit("Notes", "/tmp/My Notes.txt", "text/plain"))
        );
        assert_eq!(
            hit_from_row(Some("file:///tmp/My%20Notes.txt"), None, None).unwrap(),
            Some(hit(
                "My Notes.txt",
                "/tmp/My Notes.txt",
                "application/octet-stream"
            ))
        );
        assert_eq!(hit_from_row(None, Some("x"), Some("y")).unwrap(), None);
        assert!(hit_from_row(Some("https://example.com/a"), None, None).is_err());
    }

    #[test]
    fn basename_follows_g_path_get_basename() {
        assert_eq!(basename(Path::new("/tmp/a.txt")), "a.txt");
        assert_eq!(basename(Path::new("/tmp/dir/")), "dir");
        assert_eq!(basename(Path::new("/tmp/..")), "..");
        assert_eq!(basename(Path::new("///")), "/");
        assert_eq!(basename(Path::new("")), ".");
        assert_eq!(basename(Path::new("plain")), "plain");
    }

    #[test]
    fn search_sends_query_and_skips_rows_without_url() {
        let mut sent = String::new();
        let mut hits = Vec::new();
        let result = search(
            |query| {
                sent = query.to_owned();
                Ok(fake(
                    vec![
                        [Some("file:///a/b.pdf"), None, Some("application/pdf")],
                        [None, Some("ghost"), None],
                        [Some("file:///c"), Some("C"), None],
                    ],
                    None,
                ))
            },
            "abc",
            |hit| hits.push(hit),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(sent, query_string("abc"));
        assert_eq!(
            hits,
            vec![
                hit("b.pdf", "/a/b.pdf", "application/pdf"),
                hit("C", "/c", "application/octet-stream"),
            ]
        );
    }

    #[test]
    fn no_miner_error_reports_message_line_without_rows() {
        let message = format!("The name {MINER_SERVICE} was not provided by any .service files");
        let mut hits = Vec::new();
        let result = search(
            |_| Err::<super::TrackerRows, _>(message.clone()),
            "fixture query",
            |hit| hits.push(hit),
        );
        assert_eq!(result, Err(message.clone()));
        assert!(hits.is_empty());
        assert_eq!(error_line(&message), format!("{message}\n"));
        assert!(error_line(&message).contains("org.freedesktop.Tracker3.Miner.Files"));
    }

    #[test]
    fn errors_keep_rows_collected_so_far() {
        let mut hits = Vec::new();
        let result = search(
            |_| {
                Ok(fake(
                    vec![
                        [Some("file:///one"), None, None],
                        [Some("file:///two"), None, None],
                    ],
                    Some(1),
                ))
            },
            "abc",
            |hit| hits.push(hit),
        );
        assert_eq!(result, Err("cursor broke".to_owned()));
        assert_eq!(hits, vec![hit("one", "/one", "application/octet-stream")]);

        let mut hits = Vec::new();
        let result = search(
            |_| {
                Ok(fake(
                    vec![
                        [Some("file:///one"), None, None],
                        [Some("http://x/two"), None, None],
                        [Some("file:///three"), None, None],
                    ],
                    None,
                ))
            },
            "abc",
            |hit| hits.push(hit),
        );
        assert!(result.is_err());
        assert_eq!(hits, vec![hit("one", "/one", "application/octet-stream")]);
    }
}
