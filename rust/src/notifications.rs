use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::icons::{load_icon_from_info, load_icon_from_name};
use crate::keybindings::navigate;
use crate::pages::DialogPage;
use crate::rofication::{RoficationClient, SOCKET_PATH};

const ICON: u32 = 0;
const APP: u32 = 1;
const DETAIL: u32 = 2;
const ID: u32 = 3;
const APP_INFO: u32 = 4;

pub struct RoficationPage {
    inner: Rc<Inner>,
}

struct Inner {
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    selected_notification_id: Rc<Cell<i32>>,
    client: RoficationClient,
    icon_size: i32,
    post_launch_sleep: u64,
    quit: Rc<dyn Fn()>,
}

impl RoficationPage {
    pub fn new(settings: gio::Settings, entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Self {
        let icon_size = settings.int("icon-size");
        let model = gtk::ListStore::new(&[
            Pixbuf::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            gio::DesktopAppInfo::static_type(),
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        filter.set_visible_func(move |tree_model, iter| {
            let query = entry.text().to_lowercase();
            matches_query(
                query.trim(),
                &string_value(tree_model, iter, DETAIL),
                &string_value(tree_model, iter, APP),
            )
        });

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_fixed_height_mode(false);
        view.set_enable_search(false);
        let icon_renderer = gtk::CellRendererPixbuf::new();
        view.append_column(&gtk::TreeViewColumn::with_attributes(
            "Icon",
            &icon_renderer,
            &[("pixbuf", ICON as i32)],
        ));
        let app_renderer = gtk::CellRendererText::new();
        view.append_column(&gtk::TreeViewColumn::with_attributes(
            "App",
            &app_renderer,
            &[("text", APP as i32)],
        ));
        let wrapping_renderer = gtk::CellRendererText::new();
        wrapping_renderer.set_wrap_mode(gtk::pango::WrapMode::Word);
        wrapping_renderer.set_wrap_width(300);
        view.append_column(&gtk::TreeViewColumn::with_attributes(
            "Body",
            &wrapping_renderer,
            &[("text", DETAIL as i32)],
        ));
        view.set_activate_on_single_click(true);

        let post_launch_sleep = settings.int("post-launch-sleep").max(0) as u64;
        let selected_notification_id = Rc::new(Cell::new(-1));
        let filter_for_activation = filter.clone();
        let quit_for_activation = quit.clone();
        view.connect_row_activated(move |_, row_path, _| {
            if let Some(iter) = filter_for_activation.iter(row_path) {
                execute_app_from_selection(
                    &filter_for_activation,
                    &iter,
                    post_launch_sleep,
                    &quit_for_activation,
                );
            }
        });
        let selected_for_selection = selected_notification_id.clone();
        view.selection().connect_changed(move |selection| {
            if let Some((tree_model, iter)) = selection.selected() {
                selected_for_selection.set(parse_int(&string_value(&tree_model, &iter, ID)));
            }
        });

        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        Self {
            inner: Rc::new(Inner {
                model,
                filter,
                view,
                root: scrolled.upcast(),
                path: RefCell::new(gtk::TreePath::new_first()),
                selected_notification_id,
                client: RoficationClient::new(SOCKET_PATH),
                icon_size,
                post_launch_sleep,
                quit,
            }),
        }
    }
}

impl Inner {
    // The fetch blocks the calling thread until the daemon closes the
    // connection; rows are only replaced once the fetch succeeded.
    fn load_notifications(&self) -> Result<(), String> {
        let notifications = self.client.get_notifications()?;
        let theme = gtk::IconTheme::default();
        self.model.clear();

        for notification in notifications {
            let app_info = desktop_app_info(&notification.application);
            let icon = load_icon(
                theme.as_ref(),
                app_info.as_ref(),
                &notification.icon,
                self.icon_size,
            );
            let detail = detail_text(&notification.summary, &notification.body);
            let row = self.model.append();
            self.model.set(
                &row,
                &[
                    (ICON, &icon),
                    (APP, &notification.application),
                    (DETAIL, &detail),
                    (ID, &notification.id.to_string()),
                    (APP_INFO, &app_info),
                ],
            );
        }
        Ok(())
    }

    // Select the stored path and track the first visible notification id.
    fn set_selection(&self) {
        let selection = self.view.selection();
        selection.set_mode(gtk::SelectionMode::Single);
        selection.select_path(&self.path.borrow());

        if let Some(iter) = self.filter.iter_first() {
            self.selected_notification_id
                .set(parse_int(&string_value(&self.filter, &iter, ID)));
        }
    }

    fn delete_selected_notification(&self) {
        let id = self.selected_notification_id.get();
        if id > -1 {
            match self.client.delete_notification_by_id(i64::from(id)) {
                Ok(()) => {
                    // The Vala reload is an unobserved async call, so its
                    // failure is silently dropped.
                    let _ = self.load_notifications();
                    self.selected_notification_id.set(-1);
                    self.set_selection();
                }
                Err(error) => eprintln!("Error: delete_selected_notification failed: {error}"),
            }
        }
    }

    fn delete_all_notifications(&self) {
        // Vala starts from a 10-element null array and appends the visible
        // ids, so the non-empty guard below always holds.
        let mut ids: Vec<Option<String>> = vec![None; 10];
        if let Some(iter) = self.filter.iter_first() {
            loop {
                ids.push(Some(string_value(&self.filter, &iter, ID)));
                if !self.filter.iter_next(&iter) {
                    break;
                }
            }
        }

        if !ids.is_empty() {
            if let Err(error) = self.client.delete_notifications_by_ids(&ids) {
                eprintln!("Error: delete_func failed: {error}");
                return;
            }
        }
        let _ = self.load_notifications();
        self.set_selection();
    }
}

impl DialogPage for RoficationPage {
    fn initialize(&self) {
        let result = self.inner.load_notifications();
        // Vala's `load_notifications.begin()` never yields, so GTask defers
        // its completion callback to a default-priority idle source.
        let inner = self.inner.clone();
        let mut result = Some(result);
        glib::idle_add_local_full(glib::Priority::DEFAULT, move || {
            match result.take() {
                Some(Ok(())) => {
                    inner
                        .model
                        .set_sort_column_id(gtk::SortColumn::Index(APP), gtk::SortType::Ascending);
                    inner.set_selection();
                }
                Some(Err(error)) => eprintln!("Error: load_notifications failed: {error}"),
                None => {}
            }
            glib::ControlFlow::Break
        });
    }
    fn get_root(&self) -> gtk::Widget {
        self.inner.root.clone()
    }
    fn on_entry_changed(&self) {
        self.inner.filter.refilter();
        self.inner.set_selection();
    }
    fn on_entry_activated(&self) {
        let inner = &self.inner;
        let iter = inner.filter.iter(&inner.path.borrow());
        if let Some(iter) = iter {
            execute_app_from_selection(&inner.filter, &iter, inner.post_launch_sleep, &inner.quit);
        }
    }
    fn get_id(&self) -> &'static str {
        "notifications"
    }
    fn get_name(&self) -> &'static str {
        "<u>N</u>otifications"
    }
    fn get_icon_name(&self) -> &'static str {
        "mail-message-new"
    }
    fn get_help(&self) -> &'static str {
        "This dialog manages desktop notifications. Initially all desktop apps are presented. The user may filter the list in the top text box."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [
            ("del", "Delete Notification"),
            ("shift del", "Delete All Notifications"),
        ]
        .into_iter()
        .collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if navigate(&self.inner.view, &self.inner.path, event) {
            return true;
        }
        // Keyval 65535.
        if event.keyval() == gdk::keys::constants::Delete {
            if event.state().contains(gdk::ModifierType::SHIFT_MASK) {
                self.inner.delete_all_notifications();
            } else {
                self.inner.delete_selected_notification();
            }
            return true;
        }
        false
    }
    fn get_keybinding(&self) -> char {
        'n'
    }
    fn show(&self) {
        self.inner.view.grab_focus();
    }
}

fn string_value(
    model: &impl glib::IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
    column: u32,
) -> String {
    model
        .value(iter, column as i32)
        .get::<String>()
        .unwrap_or_default()
}

// Mirror Vala's `int.parse`: leading digits as a C long, truncated to int.
fn parse_int(text: &str) -> i32 {
    let text = text.trim_start();
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let mut value: i64 = 0;
    for digit in digits.bytes().take_while(u8::is_ascii_digit) {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
    }
    (if negative {
        value.saturating_neg()
    } else {
        value
    }) as i32
}

pub fn detail_text(summary: &str, body: &str) -> String {
    match (summary.is_empty(), body.is_empty()) {
        (false, false) => format!("{summary}\n{body}"),
        (true, false) => body.to_owned(),
        (false, true) => summary.to_owned(),
        (true, true) => "<no content>".to_owned(),
    }
}

// `query` is already lowercased and stripped; match detail or application.
pub fn matches_query(query: &str, detail: &str, application: &str) -> bool {
    query.is_empty()
        || detail.to_lowercase().contains(query)
        || application.to_lowercase().contains(query)
}

fn desktop_app_info(application: &str) -> Option<gio::DesktopAppInfo> {
    let groups = gio::DesktopAppInfo::search(application);
    let id = groups.first()?.first()?;
    gio::DesktopAppInfo::new(id)
}

fn load_icon(
    theme: Option<&gtk::IconTheme>,
    app_info: Option<&gio::DesktopAppInfo>,
    icon_name: &str,
    icon_size: i32,
) -> Option<Pixbuf> {
    if icon_size == 0 {
        return None;
    }
    let theme = theme?;
    match app_info {
        Some(app_info) => load_icon_from_info(theme, app_info, icon_size),
        None => load_icon_from_name(theme, icon_name, icon_size),
    }
}

fn execute_app_from_selection(
    filter: &gtk::TreeModelFilter,
    iter: &gtk::TreeIter,
    post_launch_sleep: u64,
    quit: &Rc<dyn Fn()>,
) {
    let app_info = filter
        .value(iter, APP_INFO as i32)
        .get::<Option<gio::DesktopAppInfo>>()
        .ok()
        .flatten();
    // Vala passes a missing app info straight to `launch_uris`, which fails
    // its type check without setting an error, so the dialog still quits.
    let result = match &app_info {
        Some(app_info) => app_info.launch_uris(&[], None::<&gio::AppLaunchContext>),
        None => Ok(()),
    };
    match result {
        Ok(()) => {
            glib::usleep(post_launch_sleep);
            quit();
        }
        Err(error) => eprintln!("{}", error.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::{detail_text, matches_query, parse_int};

    #[test]
    fn detail_text_covers_every_branch() {
        assert_eq!(
            detail_text("Build ready", "alpha body"),
            "Build ready\nalpha body"
        );
        assert_eq!(detail_text("", "alpha body"), "alpha body");
        assert_eq!(detail_text("Build ready", ""), "Build ready");
        assert_eq!(detail_text("", ""), "<no content>");
    }

    #[test]
    fn filter_matches_application_or_detail() {
        let builder = ("Build ready\nalpha body", "Builder");
        let messenger = ("Chat\nbeta body", "Messenger");
        let visible = |query: &str| {
            [builder, messenger]
                .iter()
                .filter(|(detail, app)| matches_query(query, detail, app))
                .map(|(_, app)| *app)
                .collect::<Vec<_>>()
        };
        // Application-only match.
        assert_eq!(visible("messenger"), ["Messenger"]);
        // Body-only match.
        assert_eq!(visible("alpha body"), ["Builder"]);
        // Summary text is part of the detail column.
        assert_eq!(visible("chat"), ["Messenger"]);
        assert_eq!(visible("body"), ["Builder", "Messenger"]);
        assert_eq!(visible(""), ["Builder", "Messenger"]);
        assert!(visible("nothing").is_empty());
        // Stored text is lowercased before matching.
        assert!(matches_query("build", "", "BUILDER"));
    }

    #[test]
    fn id_parsing_follows_int_parse() {
        assert_eq!(parse_int("11"), 11);
        assert_eq!(parse_int("-4"), -4);
        assert_eq!(parse_int(""), 0);
        assert_eq!(parse_int("4294967307"), 11);
    }
}
