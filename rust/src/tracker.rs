use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::icons::load_icon_from_name;
use crate::keybindings::navigate;
use crate::pages::{is_enter_key, select_first_row, DialogPage};
use crate::sparql::{Connection, Cursor};

const ICON: u32 = 0;
const TITLE: u32 = 1;
const FILE: u32 = 2;

const MINER_SERVICE: &str = "org.freedesktop.Tracker3.Miner.Files";
const FALLBACK_MIME: &str = "application/octet-stream";
const FALLBACK_ICON: &str = "text-x-generic";

/// Most files listed for one search: the best ranked ones.
const MAX_HITS: usize = 1000;
/// A search starts once typing pauses this long, so a burst of keys costs
/// one search.
const SEARCH_DELAY: Duration = Duration::from_millis(150);

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
    search: Search,
    // For the callbacks of searches, which may outlive a key press.
    this: Weak<TrackerPage>,
}

/// The state of the search for the current text. Searches run on worker
/// threads, so typing never waits for the index.
#[derive(Default)]
struct Search {
    // One connection for every search, opened by the first.
    connection: Arc<Mutex<Option<Connection>>>,
    // Waiting for typing to pause.
    timer: RefCell<Option<glib::SourceId>>,
    // The running search.
    running: RefCell<Option<gio::Cancellable>>,
    // Counts text changes: results of a search for older text are dropped.
    generation: Cell<u64>,
    // Enter was pressed while the search was waiting or running.
    activate_when_ready: Cell<bool>,
}

impl TrackerPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
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

        let page = Rc::new_cyclic(|this| Self {
            entry,
            model,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            icon_theme: gtk::IconTheme::default(),
            icon_size,
            post_launch_sleep,
            quit,
            search: Search::default(),
            this: this.clone(),
        });
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

    /// Highlights the first row: the best match for the text searched.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        *self.path.borrow_mut() = select_first_row(&self.view);
    }

    /// Drops the waiting and the running search; results of the running one
    /// are ignored if they still arrive.
    fn cancel_search(&self) {
        if let Some(timer) = self.search.timer.take() {
            timer.remove();
        }
        if let Some(running) = self.search.running.take() {
            running.cancel();
        }
        self.search.generation.set(self.search.generation.get() + 1);
        self.search.activate_when_ready.set(false);
    }

    fn schedule_search(&self, text: String) {
        let this = self.this.clone();
        let timer = glib::timeout_add_local_once(SEARCH_DELAY, move || {
            if let Some(page) = this.upgrade() {
                page.search.timer.take();
                page.start_search(text);
            }
        });
        self.search.timer.replace(Some(timer));
    }

    fn start_search(&self, text: String) {
        let generation = self.search.generation.get();
        let cancellable = gio::Cancellable::new();
        self.search.running.replace(Some(cancellable.clone()));
        let connection = self.search.connection.clone();
        let this = self.this.clone();
        glib::MainContext::default().spawn_local(async move {
            let (hits, error) =
                gio::spawn_blocking(move || search(&connection, &text, &cancellable))
                    .await
                    .unwrap_or_else(|_| (Vec::new(), Some("file search panicked".to_owned())));
            let Some(page) = this.upgrade() else {
                return;
            };
            if page.search.generation.get() != generation {
                return; // Superseded by newer text.
            }
            page.search.running.take();
            if let Some(message) = error {
                eprint!("{}", error_line(&message));
            }
            page.show_hits(hits);
            if page.search.activate_when_ready.take() {
                page.open_selected();
            }
        });
    }

    /// Enter while the search for the current text is waiting or running:
    /// start it at once and open the selected hit when it arrives, rather
    /// than a row left from earlier text. False when there is nothing to
    /// wait for.
    fn activate_when_ready(&self) -> bool {
        if let Some(timer) = self.search.timer.take() {
            timer.remove();
            self.start_search(self.entry.text().into());
        }
        let waiting = self.search.running.borrow().is_some();
        self.search.activate_when_ready.set(waiting);
        waiting
    }

    fn show_hits(&self, hits: Vec<Hit>) {
        self.model.clear();
        // Hits of one MIME type share their icon.
        let mut icons = HashMap::new();
        for hit in hits {
            let icon = icons
                .entry(hit.mime)
                .or_insert_with_key(|mime| self.icon_for(mime));
            let row = self.model.append();
            self.model.set(
                &row,
                &[(ICON, &*icon), (TITLE, &hit.title), (FILE, &hit.path)],
            );
        }
        self.set_selection();
    }

    fn open_selected(&self) {
        if let Some((model, iter)) = self.view.selection().selected() {
            let file = string_value(&model, &iter, FILE as i32);
            execute_app(&file, self.post_launch_sleep, &self.quit);
        }
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
        self.cancel_search();
        let text = self.entry.text();
        // Shorter text is not searched and leaves the list as it is. Vala
        // `string.length` counts bytes, not characters.
        if text.len() > 2 {
            self.schedule_search(text.into());
        }
    }
    fn on_entry_activated(&self) {
        if !self.activate_when_ready() {
            self.open_selected();
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
        if !is_enter_key(&event.keyval()) {
            return false;
        }
        // Typed text that matches no file is nothing this page can open, so
        // Enter then does nothing.
        self.activate_when_ready()
            || (self.model.iter_first().is_none() && !self.entry.text().is_empty())
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

impl Rows for Cursor<'_> {
    fn next(&mut self) -> Result<bool, String> {
        Cursor::next(self).map_err(|error| error.message().to_owned())
    }
    fn get_string(&self, column: i32) -> Option<&str> {
        Cursor::get_string(self, column)
    }
}

/// Searches the file index for `text`, on a worker thread. Returns the hits
/// read, and the error that stopped the search, if any: the SPARQL library
/// or the index may be unavailable.
fn search(
    connection: &Mutex<Option<Connection>>,
    text: &str,
    cancellable: &gio::Cancellable,
) -> (Vec<Hit>, Option<String>) {
    let mut connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
    let mut hits = Vec::new();
    let result = run_search(&mut connection, text, cancellable, |hit| hits.push(hit));
    if result.is_err() && !cancellable.is_cancelled() {
        // The endpoint may have gone away: connect again next time.
        *connection = None;
    }
    (hits, result.err())
}

fn run_search(
    connection: &mut Option<Connection>,
    text: &str,
    cancellable: &gio::Cancellable,
    sink: impl FnMut(Hit),
) -> Result<(), String> {
    let message = |error: glib::Error| error.message().to_owned();
    let connection = match connection {
        Some(connection) => connection,
        None => connection.insert(Connection::bus_new(MINER_SERVICE).map_err(message)?),
    };
    let mut cursor = connection
        .query(&search_query(), &[("text", text)], cancellable)
        .map_err(message)?;
    collect_hits(&mut cursor, sink)
}

/// The search text is bound as the `~text` parameter, so quotes in it are
/// searched for literally instead of breaking the query.
///
/// A match on the file name is the file (`nfo:FileDataObject`, which has the
/// URL); a match on the content is its document (`nie:InformationElement`,
/// stored as the file, which has the title and MIME type). Each column is
/// read from whichever of the two has it, so content matches are found too.
fn search_query() -> String {
    format!(
        "SELECT DISTINCT \
         COALESCE(nie:url(?f), nie:url(nie:isStoredAs(?f))) \
         COALESCE(nie:title(?f), nie:title(nie:interpretedAs(?f))) \
         COALESCE(nie:mimeType(?f), nie:mimeType(nie:interpretedAs(?f))) \
         WHERE {{ ?f fts:match ~text }} ORDER BY DESC(fts:rank(?f)) LIMIT {MAX_HITS}"
    )
}

fn error_line(message: &str) -> String {
    format!("{message}\n")
}

/// Hand each row to `sink` as it is read. The first error stops the walk;
/// rows already delivered stay delivered.
fn collect_hits(rows: &mut impl Rows, mut sink: impl FnMut(Hit)) -> Result<(), String> {
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
        basename, collect_hits, error_line, hit_from_row, search_query, Hit, Rows, MINER_SERVICE,
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
    fn search_query_binds_the_text_is_bounded_and_reads_content_matches() {
        assert_eq!(
            search_query(),
            "SELECT DISTINCT \
             COALESCE(nie:url(?f), nie:url(nie:isStoredAs(?f))) \
             COALESCE(nie:title(?f), nie:title(nie:interpretedAs(?f))) \
             COALESCE(nie:mimeType(?f), nie:mimeType(nie:interpretedAs(?f))) \
             WHERE { ?f fts:match ~text } ORDER BY DESC(fts:rank(?f)) LIMIT 1000"
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
    fn rows_without_url_are_skipped() {
        let mut hits = Vec::new();
        let mut rows = fake(
            vec![
                [Some("file:///a/b.pdf"), None, Some("application/pdf")],
                [None, Some("ghost"), None],
                [Some("file:///c"), Some("C"), None],
            ],
            None,
        );
        assert_eq!(collect_hits(&mut rows, |hit| hits.push(hit)), Ok(()));
        assert_eq!(
            hits,
            vec![
                hit("b.pdf", "/a/b.pdf", "application/pdf"),
                hit("C", "/c", "application/octet-stream"),
            ]
        );
    }

    #[test]
    fn errors_are_reported_as_one_line() {
        let message = format!("The name {MINER_SERVICE} was not provided by any .service files");
        assert_eq!(error_line(&message), format!("{message}\n"));
        assert!(error_line(&message).contains("org.freedesktop.Tracker3.Miner.Files"));
    }

    #[test]
    fn errors_keep_rows_collected_so_far() {
        let mut hits = Vec::new();
        let mut rows = fake(
            vec![
                [Some("file:///one"), None, None],
                [Some("file:///two"), None, None],
            ],
            Some(1),
        );
        let result = collect_hits(&mut rows, |hit| hits.push(hit));
        assert_eq!(result, Err("cursor broke".to_owned()));
        assert_eq!(hits, vec![hit("one", "/one", "application/octet-stream")]);

        let mut hits = Vec::new();
        let mut rows = fake(
            vec![
                [Some("file:///one"), None, None],
                [Some("http://x/two"), None, None],
                [Some("file:///three"), None, None],
            ],
            None,
        );
        let result = collect_hits(&mut rows, |hit| hits.push(hit));
        assert!(result.is_err());
        assert_eq!(hits, vec![hit("one", "/one", "application/octet-stream")]);
    }
}
