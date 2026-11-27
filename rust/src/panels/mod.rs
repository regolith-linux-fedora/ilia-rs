//! The quick panels: Wi-Fi networks, Bluetooth devices and power profiles,
//! read from their services on the system bus.
//!
//! Every panel lists rows of three text cells (name, detail, state) under
//! the filter entry. The rows come from the service as its answers arrive,
//! so the window is usable before any of them; the panel follows the
//! service's signals for as long as it is open. Enter acts on the
//! highlighted row through a service call.

pub mod bluetooth;
pub mod power;
pub mod wifi;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::filter::{contains_all_words, fold_text, query_words};
use crate::keybindings::navigate;

const NAME: u32 = 0;
const DETAIL: u32 = 1;
const STATE: u32 = 2;
/// What identifies the row while the list changes: an SSID, a device
/// address, a raw profile name.
const KEY: u32 = 3;
const FOLDED_NAME: u32 = 4;
/// Status rows say that something is unavailable; the filter never hides
/// them.
const STATUS: u32 = 5;

/// How long an action waits for the service's reply.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// The reply a reading waits for: as long as the service takes. A service
/// that never answers leaves its part of the panel empty, never blocked.
const READ_TIMEOUT: i32 = i32::MAX;

/// How long a rereading waits for more signals of the same change.
const REFRESH_DELAY: Duration = Duration::from_millis(30);

/// How many signals the [`Journal`] keeps for the answers still awaited.
const JOURNAL_LIMIT: usize = 1024;

/// One listed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub detail: String,
    pub state: String,
    pub key: String,
    pub status: bool,
}

impl Row {
    pub fn new(
        name: impl Into<String>,
        detail: impl Into<String>,
        state: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            detail: detail.into(),
            state: state.into(),
            key: key.into(),
            status: false,
        }
    }

    /// A status row: its own key, never filtered out.
    pub fn status(name: &str, state: &str) -> Self {
        Self {
            name: name.to_owned(),
            detail: String::new(),
            state: state.to_owned(),
            key: format!("\u{1f}status:{name}"),
            status: true,
        }
    }
}

/// The list shared by the panels: the rows in the order the panel gives,
/// filtered by the words of the entry's text, which must all occur in the
/// row's name. The highlight follows its row's key while the rows change.
pub struct PanelList {
    entry: gtk::Entry,
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: Rc<RefCell<gtk::TreePath>>,
    words: Rc<RefCell<Vec<String>>>,
    /// The key of the one row a prompt shows, while it shows.
    prompt: Rc<RefCell<Option<String>>>,
    /// Whether some read the panel made of its service on opening is still
    /// unanswered.
    opening: Cell<bool>,
    /// Whether the highlight is pinned to the first row: from the opening
    /// until the opening reads have answered or Up/Down is pressed, and
    /// again after a change of the filter text while they are unanswered.
    pinned: Rc<Cell<bool>>,
}

impl PanelList {
    pub fn new(entry: gtk::Entry) -> Rc<Self> {
        let model = gtk::ListStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            bool::static_type(),
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        let words: Rc<RefCell<Vec<String>>> = Rc::default();
        let prompt: Rc<RefCell<Option<String>>> = Rc::default();
        let (words_for_filter, prompt_for_filter) = (words.clone(), prompt.clone());
        filter.set_visible_func(move |model, iter| {
            let text = |column: u32| {
                model
                    .value(iter, column as i32)
                    .get::<String>()
                    .unwrap_or_default()
            };
            if let Some(key) = prompt_for_filter.borrow().as_deref() {
                return text(KEY) == key;
            }
            model
                .value(iter, STATUS as i32)
                .get::<bool>()
                .unwrap_or(false)
                || contains_all_words(&text(FOLDED_NAME), &words_for_filter.borrow())
        });

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        for (title, column, expand) in [
            ("Name", NAME, true),
            ("Detail", DETAIL, false),
            ("State", STATE, false),
        ] {
            let renderer = gtk::CellRendererText::new();
            let view_column =
                gtk::TreeViewColumn::with_attributes(title, &renderer, &[("text", column as i32)]);
            view_column.set_expand(expand);
            view.append_column(&view_column);
        }
        // Up, Down and the other cursor keys reach the view itself, which
        // moves its cursor: that ends the pinning, wherever the cursor goes.
        let pinned = Rc::new(Cell::new(true));
        let pinned_for_keys = pinned.clone();
        view.connect_move_cursor(move |_, _, _| {
            pinned_for_keys.set(false);
            false
        });
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);
        Rc::new(Self {
            entry,
            model,
            filter,
            view,
            root: scrolled.upcast(),
            path: Rc::new(RefCell::new(gtk::TreePath::new_first())),
            words,
            prompt,
            opening: Cell::new(true),
            pinned,
        })
    }

    pub fn root(&self) -> gtk::Widget {
        self.root.clone()
    }

    /// A click on a row highlights it and acts on it as Enter does, as on
    /// the other pages.
    pub fn connect_activated(&self, on_activate: impl Fn() + 'static) {
        self.view.set_activate_on_single_click(true);
        let (path, pinned) = (self.path.clone(), self.pinned.clone());
        self.view.connect_row_activated(move |view, row, _| {
            pinned.set(false);
            view.set_cursor(row, None::<&gtk::TreeViewColumn>, false);
            *path.borrow_mut() = row.clone();
            on_activate();
        });
    }

    pub fn view(&self) -> &gtk::TreeView {
        &self.view
    }

    pub fn entry(&self) -> &gtk::Entry {
        &self.entry
    }

    /// Replaces the rows. The highlighted row stays highlighted while a
    /// row with its key is listed; otherwise the first row is. While the
    /// highlight is pinned, the first row is.
    pub fn set_rows(&self, rows: &[Row]) {
        let selected = self.selected_key();
        let mut iter = self.model.iter_first();
        for row in rows {
            let folded = fold_text(&row.name);
            let values: [(u32, &dyn ToValue); 6] = [
                (NAME, &row.name),
                (DETAIL, &row.detail),
                (STATE, &row.state),
                (KEY, &row.key),
                (FOLDED_NAME, &folded),
                (STATUS, &row.status),
            ];
            match iter.take() {
                Some(existing) => {
                    self.model.set(&existing, &values);
                    if self.model.iter_next(&existing) {
                        iter = Some(existing);
                    }
                }
                None => {
                    let appended = self.model.append();
                    self.model.set(&appended, &values);
                }
            }
        }
        // Rows left over from a longer list.
        if let Some(existing) = iter {
            while self.model.remove(&existing) {}
        }
        self.filter.refilter();
        if self.pinned.get() && self.prompt.borrow().is_none() {
            self.select_first();
            return;
        }
        match selected.and_then(|key| self.position_of(&key)) {
            Some(path) => self.highlight(&path),
            None => self.select_first(),
        }
    }

    /// The panel (re)opens on a service: its opening reads are under way,
    /// and until they have all answered the highlight stays on the first
    /// row. The panel calls [`Self::end_opening`] once they have, whatever
    /// the answers were; however long that takes.
    pub fn start_opening(&self) {
        self.opening.set(true);
        self.pinned.set(true);
    }

    /// The opening reads have answered (or there are none, without a
    /// service): from now on the highlight follows its row while the rows
    /// change.
    pub fn end_opening(&self) {
        self.opening.set(false);
        self.pinned.set(false);
    }

    /// Takes the words of a new filter text and highlights the first row,
    /// pinned there again while the opening reads are unanswered. While a
    /// prompt shows, its text is no filter.
    pub fn filter_text_changed(&self) {
        if self.prompt.borrow().is_some() {
            return;
        }
        *self.words.borrow_mut() = query_words(self.entry.text().as_str());
        self.pinned.set(self.opening.get());
        self.filter.refilter();
        self.select_first();
    }

    /// Lists only the row with `key` and empties the entry, which now takes
    /// a secret: what is typed is masked, and accessibility tools see a
    /// password field.
    pub fn show_prompt(&self, key: &str) {
        self.prompt.replace(Some(key.to_owned()));
        self.entry.set_visibility(false);
        self.entry.set_input_purpose(gtk::InputPurpose::Password);
        self.entry.set_text("");
        self.filter.refilter();
        self.select_first();
        self.entry.grab_focus_without_selecting();
    }

    /// Ends the prompt: the entry shows text again, empty, every row is
    /// listed and the prompt's row stays highlighted.
    pub fn close_prompt(&self) {
        let Some(key) = self.prompt.take() else {
            return;
        };
        let pinned = self.pinned.get();
        self.entry.set_text("");
        self.pinned.set(pinned);
        self.entry.set_visibility(true);
        self.entry.set_input_purpose(gtk::InputPurpose::FreeForm);
        self.words.borrow_mut().clear();
        self.filter.refilter();
        match self.position_of(&key) {
            Some(path) => self.highlight(&path),
            None => self.select_first(),
        }
    }

    /// The key of the row a prompt shows, while it shows.
    pub fn prompt_key(&self) -> Option<String> {
        self.prompt.borrow().clone()
    }

    /// The key and name of the highlighted row.
    pub fn selected(&self) -> Option<(String, String)> {
        let (model, iter) = self.view.selection().selected()?;
        let text = |column: u32| {
            model
                .value(&iter, column as i32)
                .get::<String>()
                .unwrap_or_default()
        };
        Some((text(KEY), text(NAME)))
    }

    fn selected_key(&self) -> Option<String> {
        self.selected().map(|(key, _)| key)
    }

    /// The listed position of the row with `key`.
    fn position_of(&self, key: &str) -> Option<gtk::TreePath> {
        let iter = self.filter.iter_first()?;
        loop {
            if self
                .filter
                .value(&iter, KEY as i32)
                .get::<String>()
                .ok()
                .as_deref()
                == Some(key)
            {
                return self.filter.path(&iter);
            }
            if !self.filter.iter_next(&iter) {
                return None;
            }
        }
    }

    fn highlight(&self, path: &gtk::TreePath) {
        let selection = self.view.selection();
        selection.set_mode(gtk::SelectionMode::Single);
        // Setting the cursor scrolls to it: a list that changes under a
        // highlight that stays put keeps its scroll position.
        if self.view.cursor().0.as_ref() != Some(path) || !selection.path_is_selected(path) {
            self.view
                .set_cursor(path, None::<&gtk::TreeViewColumn>, false);
        }
        *self.path.borrow_mut() = path.clone();
    }

    fn select_first(&self) {
        if self.filter.iter_first().is_some() {
            self.highlight(&gtk::TreePath::new_first());
        } else {
            self.view.selection().unselect_all();
            *self.path.borrow_mut() = gtk::TreePath::new_first();
        }
    }

    /// Ctrl+n/p/j/k move the highlight, as on the other pages, and end the
    /// pinning as Up and Down do. A moved highlight follows its row from
    /// then on.
    pub fn navigate(&self, event: &gdk::EventKey) -> bool {
        let moved = navigate(&self.view, &self.path, event);
        if moved {
            self.pinned.set(false);
        }
        moved
    }
}

/// Where a panel's service answers on the system bus.
#[derive(Clone)]
pub struct Service {
    pub connection: gio::DBusConnection,
    /// The well-known name the panel calls.
    pub name: &'static str,
    /// The unique name that owns it, whose signals the panel follows.
    pub owner: String,
}

impl Service {
    /// Calls `method` and waits as long as the service takes. Every call
    /// waits on its own: one that is never answered holds back no other.
    /// A reply that is not of `reply_type` counts as an error.
    pub async fn read(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        parameters: Option<&glib::Variant>,
        reply_type: &str,
    ) -> Result<glib::Variant, glib::Error> {
        self.connection
            .call_future(
                Some(self.name),
                path,
                interface,
                method,
                parameters,
                Some(glib::VariantTy::new(reply_type).expect("a valid reply type")),
                gio::DBusCallFlags::NO_AUTO_START,
                READ_TIMEOUT,
            )
            .await
    }

    /// Every property of `interface` on the object at `path`.
    pub async fn properties(&self, path: &str, interface: &str) -> Result<Properties, glib::Error> {
        let reply = self
            .read(
                path,
                PROPERTIES,
                "GetAll",
                Some(&(interface,).to_variant()),
                "(a{sv})",
            )
            .await?;
        Ok(Properties::from_variant(&reply.child_value(0)))
    }

    /// Calls `method` as an action and waits for the reply for at most
    /// five seconds. Fails with what to report: the service's error
    /// message, or `no reply`.
    pub async fn act(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        parameters: Option<glib::Variant>,
    ) -> Result<glib::Variant, String> {
        let reply = self
            .connection
            .call_future(
                Some(self.name),
                path,
                interface,
                method,
                parameters.as_ref(),
                None,
                // A polkit agent may ask the user before the service acts.
                gio::DBusCallFlags::NO_AUTO_START
                    | gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
                ACTION_TIMEOUT.as_millis() as i32,
            )
            .await;
        reply.map_err(|error| failure_message(&error))
    }

    /// Sets a property as an action, like [`Self::act`].
    pub async fn set_property(
        &self,
        path: &str,
        interface: &str,
        property: &str,
        value: glib::Variant,
    ) -> Result<(), String> {
        let parameters = (interface, property, value).to_variant();
        self.act(path, PROPERTIES, "Set", Some(parameters))
            .await
            .map(|_| ())
    }

    /// Runs `on_signal` for every signal the owner sends on one of
    /// `interfaces`, in the order sent, until the subscription is dropped.
    pub fn follow(
        &self,
        interfaces: &[&str],
        on_signal: impl Fn(&Signal) + 'static,
    ) -> Subscription {
        let on_signal = Rc::new(on_signal);
        let ids = interfaces
            .iter()
            .map(|interface| {
                let on_signal = on_signal.clone();
                self.connection.signal_subscribe(
                    Some(&self.owner),
                    Some(interface),
                    None,
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_, _, path, interface, member, parameters| {
                        on_signal(&Signal {
                            path,
                            interface,
                            member,
                            parameters,
                        });
                    },
                )
            })
            .collect();
        Subscription {
            connection: self.connection.clone(),
            ids,
        }
    }

    /// Whether `other` is the same owner of the same name.
    pub fn same_as(&self, other: &Service) -> bool {
        self.name == other.name && self.owner == other.owner
    }
}

/// A signal from a panel's service.
pub struct Signal<'a> {
    pub path: &'a str,
    pub interface: &'a str,
    pub member: &'a str,
    pub parameters: &'a glib::Variant,
}

/// A signal kept for later, in the [`Journal`].
pub struct KeptSignal {
    path: String,
    interface: String,
    member: String,
    parameters: glib::Variant,
}

impl KeptSignal {
    fn keep(signal: &Signal) -> Self {
        Self {
            path: signal.path.to_owned(),
            interface: signal.interface.to_owned(),
            member: signal.member.to_owned(),
            parameters: signal.parameters.clone(),
        }
    }

    pub fn signal(&self) -> Signal<'_> {
        Signal {
            path: &self.path,
            interface: &self.interface,
            member: &self.member,
            parameters: &self.parameters,
        }
    }
}

/// The signals of the current owner, kept so that an answer is never
/// older than what was announced: an answer may describe the service as
/// it was when the question was asked, so the signals that came after
/// the question are applied again on top of it.
#[derive(Default)]
pub struct Journal {
    /// How many signals came so far.
    count: Cell<u64>,
    /// The latest signals, each with its count.
    kept: RefCell<VecDeque<(u64, KeptSignal)>>,
    /// The count of the latest signal no longer kept.
    forgotten: Cell<u64>,
}

impl Journal {
    /// Where the journal stands as a question is asked.
    pub fn mark(&self) -> u64 {
        self.count.get()
    }

    pub fn record(&self, signal: &Signal) {
        let count = self.count.get() + 1;
        self.count.set(count);
        let mut kept = self.kept.borrow_mut();
        kept.push_back((count, KeptSignal::keep(signal)));
        while kept.len() > JOURNAL_LIMIT {
            if let Some((count, _)) = kept.pop_front() {
                self.forgotten.set(count);
            }
        }
    }

    /// Runs `apply` on every signal that came after `mark`, oldest first.
    /// Fails when some of them are no longer kept: the answer to a
    /// question asked at `mark` can then not be brought up to date.
    pub fn replay_since(&self, mark: u64, mut apply: impl FnMut(&Signal)) -> bool {
        if mark < self.forgotten.get() {
            return false;
        }
        for (count, signal) in self.kept.borrow().iter() {
            if *count > mark {
                apply(&signal.signal());
            }
        }
        true
    }

    /// Forgets everything, for a new owner.
    pub fn clear(&self) {
        self.kept.borrow_mut().clear();
        self.forgotten.set(self.count.get());
    }
}

impl Signal<'_> {
    /// What a property change announces: the interface, the changed
    /// values, and the names of properties whose values it left out. Both
    /// the standard `org.freedesktop.DBus.Properties` signal and the
    /// per-interface `PropertiesChanged` some services send count.
    pub fn property_changes(&self) -> Option<(String, Properties, Vec<String>)> {
        if self.member != "PropertiesChanged" {
            return None;
        }
        if self.interface == PROPERTIES {
            if self.parameters.type_().as_str() != "(sa{sv}as)" {
                return None;
            }
            let interface = self.parameters.child_value(0).str()?.to_owned();
            let changed = Properties::from_variant(&self.parameters.child_value(1));
            return Some((interface, changed, strings(&self.parameters.child_value(2))));
        }
        if self.parameters.type_().as_str() != "(a{sv})" {
            return None;
        }
        Some((
            self.interface.to_owned(),
            Properties::from_variant(&self.parameters.child_value(0)),
            Vec::new(),
        ))
    }
}

/// Signal subscriptions, ended when dropped.
pub struct Subscription {
    connection: gio::DBusConnection,
    ids: Vec<gio::SignalSubscriptionId>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            self.connection.signal_unsubscribe(id);
        }
    }
}

pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
pub const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

/// Follows who owns `name` on the system bus (as `DBUS_SYSTEM_BUS_ADDRESS`
/// names it when set) for as long as the launcher runs. `on_change` learns
/// the owner once it is known, and again whenever it changes: the service
/// to call, or `None` while nothing owns the name.
pub fn watch(name: &'static str, on_change: impl Fn(Option<Service>) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        let Ok(connection) = gio::bus_get_future(gio::BusType::System).await else {
            on_change(None);
            return;
        };
        // `None` until known; then the owner, or `None` for no owner.
        let current: Rc<RefCell<Option<Option<String>>>> = Rc::default();
        let apply = {
            let (connection, current) = (connection.clone(), current.clone());
            Rc::new(move |owner: Option<String>| {
                let owner = owner.filter(|owner| !owner.is_empty());
                if current.borrow().as_ref() == Some(&owner) {
                    return;
                }
                current.replace(Some(owner.clone()));
                on_change(owner.map(|owner| Service {
                    connection: connection.clone(),
                    name,
                    owner,
                }));
            })
        };
        let apply_change = apply.clone();
        // Subscribed before the question below is asked, so that no change
        // falls between the two, and kept for the rest of the run.
        connection.signal_subscribe(
            Some(BUS),
            Some(BUS),
            Some("NameOwnerChanged"),
            Some(BUS_PATH),
            Some(name),
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| {
                if let Some((_, _, owner)) = parameters.get::<(String, String, String)>() {
                    apply_change(Some(owner));
                }
            },
        );
        let owner = name_owner(&connection, name).await;
        // A change that arrived first is newer than this answer.
        if current.borrow().is_none() {
            apply(owner);
        }
    });
}

async fn name_owner(connection: &gio::DBusConnection, name: &str) -> Option<String> {
    let reply = connection
        .call_future(
            Some(BUS),
            BUS_PATH,
            BUS,
            "GetNameOwner",
            Some(&(name,).to_variant()),
            Some(glib::VariantTy::new("(s)").unwrap()),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
        .ok()?;
    reply.get::<(String,)>().map(|(owner,)| owner)
}

/// Reports on standard error that the action of the row `name` failed.
pub fn report_failure(name: &str, message: &str) {
    eprintln!("{}", failure_line(name, message, None));
}

/// Like [`report_failure`] for an action that carried `secret`, which the
/// line never repeats, even when the service echoes it.
pub fn report_failure_hiding(name: &str, message: &str, secret: &str) {
    eprintln!("{}", failure_line(name, message, Some(secret)));
}

/// `ilia: <name>: <message>` as one physical line: line breaks become
/// spaces, and `secret` is cut out wherever it would appear. It is cut out
/// of the text as the service sent it, before anything changes that text,
/// and then, from the joined line, both as typed and as joining would
/// write it (a secret may hold a line break itself, or the service may
/// echo it with one).
fn failure_line(name: &str, message: &str, secret: Option<&str>) -> String {
    let line = format!("ilia: {name}: {message}");
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return one_line(&line);
    };
    let joined = one_line(secret);
    let forms = [secret, joined.as_str()];
    let line = without(&line, &forms[..1]);
    without(&one_line(&line), &forms)
}

const HIDDEN: &str = "[hidden]";

/// `text` with every occurrence of each of `forms` cut out, until none is
/// left: a cut can join the pieces around it into a new occurrence. Each
/// is masked as [`HIDDEN`], unless the mask could itself make one up, or
/// cutting goes on for long; then it is removed outright, which always
/// ends since the text gets shorter.
fn without(text: &str, forms: &[&str]) -> String {
    let forms: Vec<&str> = forms
        .iter()
        .copied()
        .filter(|form| !form.is_empty())
        .collect();
    let mut mask = if forms.iter().any(|form| HIDDEN.contains(form)) {
        ""
    } else {
        HIDDEN
    };
    let mut text = text.to_owned();
    let mut rounds = 0;
    while let Some(form) = forms.iter().find(|form| text.contains(**form)) {
        rounds += 1;
        if rounds > 8 {
            mask = "";
        }
        text = text.replace(form, mask);
    }
    text
}

/// The text with every line break (CR LF, LF, CR, and the other Unicode
/// line separators) replaced by a space.
fn one_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(
        [
            '\n', '\r', '\u{b}', '\u{c}', '\u{85}', '\u{2028}', '\u{2029}',
        ],
        " ",
    )
}

/// What the service said, or `no reply` when it said nothing in time.
pub fn failure_message(error: &glib::Error) -> String {
    if error.matches(gio::IOErrorEnum::TimedOut) {
        return "no reply".to_owned();
    }
    strip_remote_error(error.message()).to_owned()
}

/// The service's own message: GDBus puts `GDBus.Error:<error name>: ` in
/// front of it.
fn strip_remote_error(message: &str) -> &str {
    message
        .strip_prefix("GDBus.Error:")
        .and_then(|rest| rest.split_once(": "))
        .map_or(message, |(_, text)| text)
}

/// The properties of one interface, as `GetAll` returns them, unboxed.
#[derive(Clone, Debug, Default)]
pub struct Properties(pub HashMap<String, glib::Variant>);

impl Properties {
    /// The properties of an `a{sv}` dictionary.
    pub fn from_variant(dictionary: &glib::Variant) -> Self {
        let mut properties = HashMap::new();
        if dictionary.type_().as_str() == "a{sv}" {
            for entry in dictionary.iter() {
                if let Some(name) = entry.child_value(0).str() {
                    let value = entry.child_value(1);
                    properties.insert(name.to_owned(), unboxed(value));
                }
            }
        }
        Self(properties)
    }

    /// Takes the values of `changes`, and forgets the properties named in
    /// `invalidated`.
    pub fn merge(&mut self, changes: &Properties, invalidated: &[String]) {
        for (name, value) in &changes.0 {
            self.0.insert(name.clone(), value.clone());
        }
        for name in invalidated {
            self.0.remove(name);
        }
    }

    pub fn get(&self, name: &str) -> Option<glib::Variant> {
        self.0.get(name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn boolean(&self, name: &str) -> bool {
        self.get(name)
            .and_then(|value| value.get::<bool>())
            .unwrap_or(false)
    }

    pub fn string(&self, name: &str) -> String {
        self.get(name)
            .and_then(|value| value.str().map(str::to_owned))
            .unwrap_or_default()
    }

    pub fn number(&self, name: &str) -> u32 {
        self.get(name).and_then(|value| number(&value)).unwrap_or(0)
    }

    pub fn bytes(&self, name: &str) -> Vec<u8> {
        self.get(name)
            .and_then(|value| value.fixed_array::<u8>().ok().map(<[u8]>::to_vec))
            .unwrap_or_default()
    }

    pub fn paths(&self, name: &str) -> Vec<String> {
        self.get(name)
            .map(|value| strings(&value))
            .unwrap_or_default()
    }
}

/// The value of `key` in a dictionary with string keys, unboxed when the
/// dictionary holds variants.
pub fn lookup(dictionary: &glib::Variant, key: &str) -> Option<glib::Variant> {
    let type_ = dictionary.type_();
    if !type_.is_array() || !type_.element().is_dict_entry() {
        return None;
    }
    dictionary
        .iter()
        .find(|entry| entry.child_value(0).str() == Some(key))
        .map(|entry| unboxed(entry.child_value(1)))
}

/// The value inside a variant; any other value as it is.
fn unboxed(value: glib::Variant) -> glib::Variant {
    if value.type_() == glib::VariantTy::VARIANT {
        value.as_variant().unwrap_or(value)
    } else {
        value
    }
}

/// An unsigned number of any integer type.
pub fn number(value: &glib::Variant) -> Option<u32> {
    value
        .get::<u8>()
        .map(u32::from)
        .or_else(|| value.get::<u16>().map(u32::from))
        .or_else(|| value.get::<u32>())
        .or_else(|| value.get::<i16>().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| value.get::<i32>().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| value.get::<u64>().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| value.get::<i64>().and_then(|n| u32::try_from(n).ok()))
}

/// The strings or object paths of an array.
pub fn strings(value: &glib::Variant) -> Vec<String> {
    if !value.is_container() {
        return Vec::new();
    }
    value
        .iter()
        .filter_map(|item| item.str().map(str::to_owned))
        .collect()
}

/// One owner of a panel's service at a time: answers and signals that
/// belong to an earlier owner are ignored.
#[derive(Default)]
pub struct Runs(Cell<u64>);

impl Runs {
    /// Starts a new run, which outdates every earlier one.
    pub fn next(&self) -> u64 {
        self.0.set(self.0.get() + 1);
        self.0.get()
    }

    pub fn is_current(&self, run: u64) -> bool {
        self.0.get() == run
    }

    pub fn current(&self) -> u64 {
        self.0.get()
    }
}

/// Orders the answers about one thing: an answer to an earlier question
/// never replaces an answer to a later one.
#[derive(Default)]
pub struct Answers {
    asked: Cell<u64>,
    taken: RefCell<HashMap<String, u64>>,
}

impl Answers {
    /// A number for a question about to be asked.
    pub fn ask(&self) -> u64 {
        self.asked.set(self.asked.get() + 1);
        self.asked.get()
    }

    /// Whether the answer to question `number` about `what` is newer than
    /// the one taken already; if so, it is the one taken now.
    pub fn take(&self, what: &str, number: u64) -> bool {
        let mut taken = self.taken.borrow_mut();
        let last = taken.entry(what.to_owned()).or_insert(0);
        if number <= *last {
            return false;
        }
        *last = number;
        true
    }

    pub fn clear(&self) {
        self.taken.borrow_mut().clear();
    }
}

/// Runs a job soon after it is asked for; requests that come together
/// make one run.
#[derive(Default)]
pub struct Soon(Rc<Cell<bool>>);

impl Soon {
    pub fn request(&self, job: impl FnOnce() + 'static) {
        if self.0.replace(true) {
            return;
        }
        let scheduled = self.0.clone();
        glib::timeout_add_local_once(REFRESH_DELAY, move || {
            scheduled.set(false);
            job();
        });
    }
}

thread_local! {
    static EXIT_HOOKS: RefCell<Vec<Box<dyn FnOnce()>>> = RefCell::default();
}

/// Runs `hook` when the launcher is about to exit, whichever way it leaves.
pub fn at_exit(hook: impl FnOnce() + 'static) {
    EXIT_HOOKS.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

/// Runs the hooks [`at_exit`] registered. The main loop has stopped; the
/// hooks finish their work before returning.
pub fn finish() {
    let hooks = EXIT_HOOKS.with(|hooks| std::mem::take(&mut *hooks.borrow_mut()));
    for hook in hooks {
        hook();
    }
}

/// The keys that act on the highlighted row of a panel.
pub fn is_activation_key(event: &gdk::EventKey) -> bool {
    crate::pages::is_enter_key(&event.keyval())
}

/// An action in progress: Enter does nothing more until it ends.
#[derive(Default)]
pub struct Busy(Cell<bool>);

impl Busy {
    /// Marks an action as started, unless one is in progress already.
    pub fn start(&self) -> bool {
        !self.0.replace(true)
    }

    pub fn end(&self) {
        self.0.set(false);
    }

    pub fn is_set(&self) -> bool {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        failure_line, failure_message, strip_remote_error, Answers, Journal, Properties, Row,
        Signal, JOURNAL_LIMIT,
    };
    use glib::ToVariant;

    #[test]
    fn remote_error_names_are_stripped() {
        assert_eq!(
            strip_remote_error("GDBus.Error:org.example.Error.Rejected: service refused action"),
            "service refused action"
        );
        assert_eq!(strip_remote_error("GDBus.Error:org.x.Failed: a: b"), "a: b");
        assert_eq!(strip_remote_error("plain message"), "plain message");
    }

    #[test]
    fn timeouts_read_no_reply() {
        let error = glib::Error::new(gio::IOErrorEnum::TimedOut, "Timeout was reached");
        assert_eq!(failure_message(&error), "no reply");
        let error = glib::Error::new(
            gio::IOErrorEnum::Failed,
            "GDBus.Error:org.x.Failed: refused",
        );
        assert_eq!(failure_message(&error), "refused");
    }

    #[test]
    fn failure_lines_stay_one_line_without_the_secret() {
        assert_eq!(
            failure_line("Cafe\nBar", "refused\r\nnext\nlast\r", None),
            "ilia: Cafe Bar: refused next last "
        );
        assert_eq!(
            failure_line(
                "Net",
                "bad key harvest moon\nagain harvest moon",
                Some("harvest moon")
            ),
            "ilia: Net: bad key [hidden] again [hidden]"
        );
        // A secret split by a line break is caught once the line is joined.
        let line = failure_line("Net", "harvest\nmoon", Some("harvest moon"));
        assert!(!line.contains("harvest moon"), "{line}");
        // Cutting out one occurrence cannot leave another behind.
        let line = failure_line("Net", "aaaaaaaaaaaaaaaab", Some("aaaaaaaab"));
        assert!(!line.contains("aaaaaaaab"), "{line}");
        let line = failure_line("Net", "x [hidden] y", Some("hidden] y"));
        assert!(!line.contains("hidden] y"), "{line}");
        // A secret holding a line break is cut out before the line is
        // joined, and as joined after: echoed as typed, with another line
        // break, repeated and overlapping.
        for secret in ["saffron\nsaffron", "saffron\u{2028}saffron"] {
            for echo in [secret, "saffron\r\nsaffron", "saffron\u{85}saffron"] {
                let message = format!("refused {echo} | {echo} | {echo}{}", &echo[7..]);
                let line = failure_line("Net", &message, Some(secret));
                assert!(!line.contains("saffron saffron"), "{line}");
                assert!(!line.contains(secret), "{line}");
                assert!(
                    line.starts_with("ilia: Net: refused [hidden] | [hidden]"),
                    "{line}"
                );
                assert_eq!(line.lines().count(), 1, "{line}");
            }
        }
        // An ordinary secret echoed with a line break inserted.
        let line = failure_line(
            "Net",
            "bad saffron\u{2028}saffron!",
            Some("saffron saffron"),
        );
        assert_eq!(line, "ilia: Net: bad [hidden]!");
        // The forms may overlap one another.
        let line = failure_line("Net", "a\nb a b\na", Some("a\nb"));
        assert!(!line.contains("a b") && !line.contains("a\nb"), "{line}");
    }

    #[test]
    fn later_answers_win() {
        let answers = Answers::default();
        let (first, second) = (answers.ask(), answers.ask());
        assert!(answers.take("/a", second));
        assert!(!answers.take("/a", first));
        assert!(answers.take("/b", first));
        let third = answers.ask();
        assert!(answers.take("/a", third));
    }

    #[test]
    fn property_changes_merge() {
        let mut properties = Properties::from_variant(
            &glib::VariantDict::new(None)
                .tap(|d| d.insert("A", 1u32))
                .tap(|d| d.insert("B", "x"))
                .end(),
        );
        let changes = Properties::from_variant(
            &glib::VariantDict::new(None)
                .tap(|d| d.insert("A", 2u32))
                .end(),
        );
        properties.merge(&changes, &["B".to_owned()]);
        assert_eq!(properties.number("A"), 2);
        assert!(!properties.has("B"));
    }

    trait Tap: Sized {
        fn tap(self, f: impl FnOnce(&Self)) -> Self {
            f(&self);
            self
        }
    }
    impl Tap for glib::VariantDict {}

    fn record(journal: &Journal, path: &str) {
        let parameters = ().to_variant();
        journal.record(&Signal {
            path,
            interface: "org.example",
            member: "Changed",
            parameters: &parameters,
        });
    }

    fn replayed(journal: &Journal, mark: u64) -> Option<Vec<String>> {
        let mut paths = Vec::new();
        journal
            .replay_since(mark, |signal| paths.push(signal.path.to_owned()))
            .then_some(paths)
    }

    #[test]
    fn the_journal_replays_what_came_after_a_question() {
        let journal = Journal::default();
        record(&journal, "/a");
        let mark = journal.mark();
        record(&journal, "/b");
        record(&journal, "/c");
        assert_eq!(replayed(&journal, mark).unwrap(), ["/b", "/c"]);
        assert_eq!(
            replayed(&journal, journal.mark()).unwrap(),
            Vec::<String>::new()
        );
        // Questions asked before a new owner have nothing to catch up on.
        journal.clear();
        assert!(replayed(&journal, mark).is_none());
        let mark = journal.mark();
        record(&journal, "/d");
        assert_eq!(replayed(&journal, mark).unwrap(), ["/d"]);
        // A question older than what is kept cannot be brought up to date.
        for _ in 0..JOURNAL_LIMIT {
            record(&journal, "/e");
        }
        assert!(replayed(&journal, mark).is_none());
        let mark = journal.mark() - 3;
        assert_eq!(replayed(&journal, mark).unwrap().len(), 3);
    }

    #[test]
    fn status_rows_have_their_own_keys() {
        let row = Row::status("Wi-Fi is off", "Off");
        assert!(row.status);
        assert_eq!((row.detail.as_str(), row.state.as_str()), ("", "Off"));
        assert_ne!(
            row.key,
            Row::new("Wi-Fi is off", "", "", "Wi-Fi is off").key
        );
    }
}
