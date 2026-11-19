//! The Features page: Regolith's optional feature packages for the running
//! session, installed and removed through PackageKit.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;

use crate::filter::fold_text;
use crate::keybindings::navigate;
use crate::packagekit::{self, Event, Transaction};
use crate::pages::{is_enter_key, select_first_row, DialogPage};

const KIND: u32 = 0;
/// The package name, a line break, then its summary.
const TEXT: u32 = 1;
const STATE: u32 = 2;
const NAME: u32 = 3;
const FOLDED_NAME: u32 = 4;
const FOLDED_SUMMARY: u32 = 5;

const TITLE: &str = "F<u>e</u>atures";
const ICON: &str = "system-software-install";
const UNAVAILABLE: &str = "Package management is not available";
const INSTALLED: &str = "Installed";

/// The session the launcher runs in: some features exist only for one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Session {
    X11,
    Wayland,
    Unknown,
}

impl Session {
    pub fn from_wm_name(wm_name: &str) -> Self {
        match wm_name {
            "i3" => Self::X11,
            "sway" => Self::Wayland,
            _ => Self::Unknown,
        }
    }
}

/// The groups of features, in the order the page lists them.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    Looks,
    BarBlocks,
    Compositors,
    WindowManager,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Looks => "Looks",
            Self::BarBlocks => "Bar blocks",
            Self::Compositors => "Compositors",
            Self::WindowManager => "Window manager",
        }
    }
}

/// The name prefixes of feature packages, their group, and whether they
/// belong to the X11 and to the Wayland session.
const PREFIXES: &[(&str, Kind, bool, bool)] = &[
    ("regolith-look-", Kind::Looks, true, true),
    ("i3xrocks-", Kind::BarBlocks, true, false),
    ("regolith-compositor-", Kind::Compositors, true, false),
    ("regolith-wm-", Kind::WindowManager, true, true),
    ("regolith-i3-", Kind::WindowManager, true, false),
    ("regolith-sway-", Kind::WindowManager, false, true),
];

/// Packages with these name endings are parts of a feature, not features.
const PART_SUFFIXES: &[&str] = &["-config", "-session", "-default-style", "-loader"];

/// The words that start the summary of a transitional package.
const TRANSITIONAL: &str = "transitional package";

/// The group of the feature package `name` in `session`, or `None` when it
/// is no feature package there. Without a known session only the features
/// of both sessions are listed.
pub fn kind_of(name: &str, session: Session) -> Option<Kind> {
    if PART_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return None;
    }
    let &(_, kind, x11, wayland) = PREFIXES
        .iter()
        .find(|(prefix, ..)| name.starts_with(prefix))?;
    let listed = match session {
        Session::X11 => x11,
        Session::Wayland => wayland,
        Session::Unknown => x11 && wayland,
    };
    listed.then_some(kind)
}

/// Whether a summary begins with the words "transitional package", in any
/// letter case.
pub fn is_transitional(summary: &str) -> bool {
    let mut characters = summary.chars();
    let starts = characters
        .by_ref()
        .take(TRANSITIONAL.chars().count())
        .flat_map(char::to_lowercase)
        .eq(TRANSITIONAL.chars());
    starts && !characters.next().is_some_and(char::is_alphanumeric)
}

/// One entry of a package list the service sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageEntry {
    pub info: u32,
    pub id: String,
    pub summary: String,
}

impl PackageEntry {
    /// The package name: the first field of the id `name;version;arch;data`.
    pub fn name(&self) -> &str {
        package_name(&self.id)
    }

    /// Whether the entry is an installed package: its data field starts
    /// with "installed".
    pub fn is_installed(&self) -> bool {
        self.info == packagekit::INFO_INSTALLED
            || self
                .id
                .split(';')
                .nth(3)
                .is_some_and(|data| data.starts_with("installed"))
    }
}

pub fn package_name(id: &str) -> &str {
    id.split(';').next().unwrap_or_default()
}

/// A feature package: all the entries the service sent for one name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Feature {
    pub name: String,
    pub kind: Kind,
    pub summary: String,
    /// The first installed entry, when the package is installed.
    pub installed_id: Option<String>,
    /// The first entry available to install.
    pub available_id: Option<String>,
}

impl Feature {
    pub fn is_installed(&self) -> bool {
        self.installed_id.is_some()
    }
}

/// The feature packages of `session` among `entries`, in the order the page
/// lists them: by group, then by name byte for byte. An entry with a
/// transitional summary is left out on its own; the other entries of its
/// name still count.
pub fn catalogue(entries: &[PackageEntry], session: Session) -> Vec<Feature> {
    let mut by_name: HashMap<&str, Feature> = HashMap::new();
    for entry in entries {
        let name = entry.name();
        let Some(kind) = kind_of(name, session) else {
            continue;
        };
        if is_transitional(&entry.summary) {
            continue;
        }
        let feature = by_name.entry(name).or_insert_with(|| Feature {
            name: name.to_owned(),
            kind,
            summary: entry.summary.clone(),
            installed_id: None,
            available_id: None,
        });
        if entry.is_installed() {
            if feature.installed_id.is_none() {
                feature.installed_id = Some(entry.id.clone());
                feature.summary = entry.summary.clone();
            }
        } else if feature.available_id.is_none() {
            feature.available_id = Some(entry.id.clone());
        }
    }
    let mut features: Vec<Feature> = by_name.into_values().collect();
    features.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    features
}

/// Whether a trial's entry is an installed package the change would take
/// away.
pub fn is_removal(info: u32) -> bool {
    matches!(
        info,
        packagekit::INFO_REMOVING | packagekit::INFO_OBSOLETING
    )
}

/// Package names, each once, in byte order, joined by ", ".
fn name_list(names: &[String]) -> String {
    let names: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    names.into_iter().collect::<Vec<_>>().join(", ")
}

/// The question asked before installing `name` would remove `removals`.
pub fn install_question(name: &str, removals: &[String]) -> String {
    format!(
        "Install {name} and remove {}? Enter to confirm, Escape to cancel.",
        name_list(removals)
    )
}

/// The question asked before removing `name`, and `removals` with it.
pub fn remove_question(name: &str, removals: &[String]) -> String {
    if removals.is_empty() {
        format!("Remove {name}? Enter to confirm, Escape to cancel.")
    } else {
        format!(
            "Remove {name} and also {}? Enter to confirm, Escape to cancel.",
            name_list(removals)
        )
    }
}

/// The state of a package while it is installed or removed, with the
/// progress when the service knows it.
pub fn change_state(install: bool, percentage: Option<u32>) -> String {
    let state = if install { "Installing" } else { "Removing" };
    match percentage.filter(|percentage| *percentage <= 100) {
        Some(percentage) => format!("{state} {percentage}%"),
        None => state.to_owned(),
    }
}

/// Whether a row's folded name or summary contains the folded query.
pub fn matches(query: &str, folded_name: &str, folded_summary: &str) -> bool {
    folded_name.contains(query) || folded_summary.contains(query)
}

/// The package a change is about, and the id it names.
#[derive(Clone, Debug)]
struct Target {
    name: String,
    id: String,
    install: bool,
}

/// A trial or a real change, running.
struct Change {
    serial: u32,
    target: Target,
    transaction: Option<Transaction>,
    /// The installed packages a trial says the change would take away.
    removals: Vec<String>,
    percentage: Option<u32>,
    allow_cancel: bool,
    cancel_requested: bool,
    waiting_for_auth: bool,
}

impl Change {
    fn new(serial: u32, target: Target) -> Self {
        Self {
            serial,
            target,
            transaction: None,
            removals: Vec::new(),
            percentage: None,
            allow_cancel: false,
            cancel_requested: false,
            waiting_for_auth: false,
        }
    }
}

enum Activity {
    Idle,
    /// Asking the service what the change would do.
    Trial(Change),
    /// Waiting for the user to answer the question.
    Question(Target, String),
    /// Changing the system.
    Real(Change),
}

/// How a trial or a real change ended.
enum Outcome {
    Success,
    Cancelled,
    /// With the reason the service gave, if it gave one.
    Failed(Option<String>),
}

struct State {
    listing_requested: bool,
    /// The latest listing requested; only its reply is shown.
    listing: u32,
    /// The latest listing follows a change and has not arrived yet.
    following: bool,
    /// Whether the latest listing arrived; false once it failed.
    available: bool,
    features: Vec<Feature>,
    activity: Activity,
    /// The latest trial or real change started.
    serial: u32,
    /// The package whose last change failed, and why: until the next trial.
    failure: Option<(String, String)>,
    /// The package whose change ended, and the state it showed last: until
    /// the listing that follows arrives.
    kept: Option<(String, String)>,
}

impl State {
    fn state_text(&self, feature: &Feature) -> String {
        if let Some((_, details)) = self
            .failure
            .as_ref()
            .filter(|(name, _)| *name == feature.name)
        {
            return format!("Failed: {details}");
        }
        if let Activity::Real(change) = &self.activity {
            if change.target.name == feature.name {
                return change_state(change.target.install, change.percentage);
            }
        }
        if let Some((_, state)) = self.kept.as_ref().filter(|(name, _)| *name == feature.name) {
            return state.clone();
        }
        if feature.is_installed() {
            INSTALLED.to_owned()
        } else {
            String::new()
        }
    }

    /// Busy: a trial, a question, a real change or the listing after one.
    fn is_busy(&self) -> bool {
        self.following || !matches!(self.activity, Activity::Idle)
    }
}

struct Inner {
    entry: gtk::Entry,
    session: Session,
    is_wayland: bool,
    /// Gives up (false) or takes back (true) the keyboard the launcher holds.
    hold_inputs: Rc<dyn Fn(bool)>,
    model: gtk::ListStore,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    /// The question, or why there is nothing to list.
    message: gtk::Label,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    query: RefCell<String>,
    state: RefCell<State>,
}

pub struct FeaturesPage {
    inner: Rc<Inner>,
}

impl FeaturesPage {
    pub fn new(
        entry: gtk::Entry,
        wm_name: &str,
        is_wayland: bool,
        hold_inputs: Rc<dyn Fn(bool)>,
    ) -> Self {
        let model = gtk::ListStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        let query = RefCell::new(String::new());

        let view = gtk::TreeView::with_model(&filter);
        view.set_headers_visible(false);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        for (column_index, title, expand) in [
            (KIND, "Kind", false),
            (TEXT, "Package", true),
            (STATE, "State", false),
        ] {
            let renderer = gtk::CellRendererText::new();
            let column = gtk::TreeViewColumn::with_attributes(
                title,
                &renderer,
                &[("text", column_index as i32)],
            );
            column.set_expand(expand);
            view.append_column(&column);
        }
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let message = gtk::Label::new(None);
        message.set_line_wrap(true);
        message.set_xalign(0.0);
        // Shown only while there is something to say, also when the window
        // shows all its widgets.
        message.set_no_show_all(true);

        // The tab's symbol and title, repeated on the page: accessibility
        // tools are not shown the labels of notebook tabs.
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        heading.pack_start(
            &gtk::Image::from_icon_name(Some(ICON), gtk::IconSize::Button),
            false,
            false,
            0,
        );
        let title = gtk::Label::new(None);
        title.set_markup(TITLE);
        title.style_context().add_class("help_heading");
        heading.pack_start(&title, false, false, 0);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 5);
        root.pack_start(&heading, false, false, 0);
        root.pack_start(&message, false, false, 0);
        root.pack_start(&scrolled, true, true, 0);

        let inner = Rc::new(Inner {
            entry,
            session: Session::from_wm_name(wm_name),
            is_wayland,
            hold_inputs,
            model,
            filter,
            view,
            message,
            root: root.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            query,
            state: RefCell::new(State {
                listing_requested: false,
                listing: 0,
                following: false,
                available: true,
                features: Vec::new(),
                activity: Activity::Idle,
                serial: 0,
                failure: None,
                kept: None,
            }),
        });
        let weak = Rc::downgrade(&inner);
        inner.filter.set_visible_func(move |model, iter| {
            let Some(inner) = weak.upgrade() else {
                return true;
            };
            let text = |column: u32| {
                model
                    .value(iter, column as i32)
                    .get::<String>()
                    .unwrap_or_default()
            };
            let query = inner.query.borrow();
            matches(&query, &text(FOLDED_NAME), &text(FOLDED_SUMMARY))
        });
        let weak = Rc::downgrade(&inner);
        inner.view.connect_row_activated(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.activate();
            }
        });
        Self { inner }
    }
}

impl Inner {
    /// Lists the packages, the first time the page is seen.
    fn first_show(self: &Rc<Self>) {
        let requested = std::mem::replace(&mut self.state.borrow_mut().listing_requested, true);
        if !requested {
            self.request_listing(false);
        }
    }

    fn request_listing(self: &Rc<Self>, following: bool) {
        let serial = {
            let mut state = self.state.borrow_mut();
            state.listing += 1;
            state.following |= following;
            state.listing
        };
        let entries = RefCell::new(Vec::new());
        let error = RefCell::new(None);
        let weak = Rc::downgrade(self);
        Transaction::start(
            &[packagekit::HINT_PLURAL_SIGNALS],
            "GetPackages",
            (packagekit::FILTER_ARCH | packagekit::FILTER_NEWEST,).to_variant(),
            move |event| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                match event {
                    Event::Package { info, id, summary } => {
                        entries
                            .borrow_mut()
                            .push(PackageEntry { info, id, summary });
                    }
                    Event::Error { details, .. } => {
                        error.replace(Some(details));
                    }
                    Event::Finished(packagekit::EXIT_SUCCESS) => {
                        inner.listing_arrived(serial, Ok(entries.take()));
                    }
                    Event::Finished(exit) => {
                        let reason = error
                            .take()
                            .unwrap_or_else(|| packagekit::exit_text(exit).to_owned());
                        inner.listing_arrived(serial, Err(reason));
                    }
                    Event::Failed(reason) => inner.listing_arrived(serial, Err(reason)),
                    _ => {}
                }
            },
        );
    }

    fn listing_arrived(&self, serial: u32, result: Result<Vec<PackageEntry>, String>) {
        {
            let mut state = self.state.borrow_mut();
            if serial != state.listing {
                return;
            }
            state.following = false;
            state.kept = None;
            match result {
                Ok(entries) => {
                    state.available = true;
                    state.features = catalogue(&entries, self.session);
                }
                Err(reason) => {
                    eprintln!("Unable to list packages: {reason}");
                    state.available = false;
                    state.features.clear();
                    state.failure = None;
                    if matches!(state.activity, Activity::Question(..)) {
                        state.activity = Activity::Idle;
                    }
                }
            }
        }
        self.show_rows();
        self.update_message();
    }

    /// Fills the rows from the latest listing, keeping the highlighted
    /// package highlighted while it is listed.
    fn show_rows(&self) {
        let selected = self.selected_name();
        self.model.clear();
        {
            let state = self.state.borrow();
            for feature in &state.features {
                let row = self.model.append();
                let text = format!("{}\n{}", feature.name, feature.summary);
                self.model.set(
                    &row,
                    &[
                        (KIND, &feature.kind.label()),
                        (TEXT, &text),
                        (STATE, &state.state_text(feature)),
                        (NAME, &feature.name),
                        (FOLDED_NAME, &fold_text(&feature.name)),
                        (FOLDED_SUMMARY, &fold_text(&feature.summary)),
                    ],
                );
            }
        }
        if !selected.is_some_and(|name| self.select_name(&name)) {
            *self.path.borrow_mut() = select_first_row(&self.view);
        }
    }

    /// Updates the state cell of every row.
    fn update_states(&self) {
        let state = self.state.borrow();
        let Some(iter) = self.model.iter_first() else {
            return;
        };
        loop {
            let name = self
                .model
                .value(&iter, NAME as i32)
                .get::<String>()
                .unwrap_or_default();
            if let Some(feature) = state.features.iter().find(|feature| feature.name == name) {
                self.model
                    .set_value(&iter, STATE, &state.state_text(feature).to_value());
            }
            if !self.model.iter_next(&iter) {
                break;
            }
        }
    }

    fn update_message(&self) {
        let text = {
            let state = self.state.borrow();
            match &state.activity {
                Activity::Question(_, question) => Some(question.clone()),
                _ if !state.available => Some(UNAVAILABLE.to_owned()),
                _ => None,
            }
        };
        match text {
            Some(text) => {
                self.message.set_text(&text);
                self.message.show();
            }
            None => {
                self.message.hide();
                self.message.set_text("");
            }
        }
    }

    fn selected_name(&self) -> Option<String> {
        let (model, iter) = self.view.selection().selected()?;
        model.value(&iter, NAME as i32).get::<String>().ok()
    }

    fn select_name(&self, name: &str) -> bool {
        let Some(iter) = self.filter.iter_first() else {
            return false;
        };
        loop {
            let row_name = self
                .filter
                .value(&iter, NAME as i32)
                .get::<String>()
                .unwrap_or_default();
            if row_name == name {
                if let Some(path) = self.filter.path(&iter) {
                    self.view.selection().set_mode(gtk::SelectionMode::Single);
                    self.view
                        .set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
                    *self.path.borrow_mut() = path;
                    return true;
                }
            }
            if !self.filter.iter_next(&iter) {
                return false;
            }
        }
    }

    /// Enter: start installing or removing the highlighted package, or
    /// confirm the question about it.
    fn activate(self: &Rc<Self>) {
        let Some(name) = self.selected_name() else {
            return;
        };
        let state = self.state.borrow();
        if !state.available {
            return;
        }
        match &state.activity {
            Activity::Idle if !state.following => {
                let Some(feature) = state.features.iter().find(|feature| feature.name == name)
                else {
                    return;
                };
                let install = !feature.is_installed();
                let id = if install {
                    feature.available_id.clone()
                } else {
                    feature.installed_id.clone()
                };
                let Some(id) = id else {
                    return;
                };
                let target = Target { name, id, install };
                drop(state);
                self.start_trial(target);
            }
            Activity::Question(target, _) if target.name == name => {
                let target = target.clone();
                drop(state);
                self.start_real(target);
            }
            _ => {}
        }
    }

    fn start_trial(self: &Rc<Self>, target: Target) {
        let serial = {
            let mut state = self.state.borrow_mut();
            // A new trial replaces the report of the last failure.
            state.failure = None;
            state.serial += 1;
            state.activity = Activity::Trial(Change::new(state.serial, target.clone()));
            state.serial
        };
        self.update_states();
        let transaction = self.start_change(serial, &target, true);
        if let Activity::Trial(change) = &mut self.state.borrow_mut().activity {
            if change.serial == serial {
                change.transaction = Some(transaction);
            }
        }
    }

    fn start_real(self: &Rc<Self>, target: Target) {
        let serial = {
            let mut state = self.state.borrow_mut();
            state.serial += 1;
            state.activity = Activity::Real(Change::new(state.serial, target.clone()));
            state.serial
        };
        if self.is_wayland {
            // The service's authorisation window may appear at any time
            // during the change.
            (self.hold_inputs)(false);
        }
        self.update_message();
        self.update_states();
        let transaction = self.start_change(serial, &target, false);
        if let Activity::Real(change) = &mut self.state.borrow_mut().activity {
            if change.serial == serial {
                change.transaction = Some(transaction);
            }
        }
    }

    fn start_change(self: &Rc<Self>, serial: u32, target: &Target, trial: bool) -> Transaction {
        let mut flags = packagekit::FLAG_ONLY_TRUSTED;
        if trial {
            flags |= packagekit::FLAG_SIMULATE;
        }
        let ids = vec![target.id.clone()];
        let (method, parameters) = if target.install {
            ("InstallPackages", (flags, ids).to_variant())
        } else {
            // Packages that depend on the removed one go with it, as the
            // trial tells; nothing else is removed.
            ("RemovePackages", (flags, ids, true, false).to_variant())
        };
        let weak = Rc::downgrade(self);
        Transaction::start(
            &[
                packagekit::HINT_INTERACTIVE,
                packagekit::HINT_PLURAL_SIGNALS,
            ],
            method,
            parameters,
            move |event| {
                if let Some(inner) = weak.upgrade() {
                    inner.change_event(serial, event);
                }
            },
        )
    }

    fn change_event(self: &Rc<Self>, serial: u32, event: Event) {
        let mut state = self.state.borrow_mut();
        let State {
            activity, failure, ..
        } = &mut *state;
        let (change, real) = match activity {
            Activity::Trial(change) if change.serial == serial => (change, false),
            Activity::Real(change) if change.serial == serial => (change, true),
            _ => return,
        };
        match event {
            Event::Package { info, id, .. } => {
                let name = package_name(&id);
                if !real && is_removal(info) && name != change.target.name {
                    change.removals.push(name.to_owned());
                }
            }
            Event::Error { code, details } => {
                if !packagekit::is_cancellation_error(code) {
                    *failure = Some((change.target.name.clone(), details));
                    drop(state);
                    self.update_states();
                }
            }
            Event::Percentage(percentage) => {
                change.percentage = Some(percentage);
                if real {
                    drop(state);
                    self.update_states();
                }
            }
            Event::AllowCancel(allow) => change.allow_cancel = allow,
            Event::Status(status) => {
                let waiting = status == packagekit::STATUS_WAITING_FOR_AUTH;
                if waiting != change.waiting_for_auth {
                    change.waiting_for_auth = waiting;
                    if !self.is_wayland {
                        // Under X11 the authorisation window can only take
                        // key presses while the launcher's grab is released.
                        drop(state);
                        (self.hold_inputs)(!waiting);
                    }
                }
            }
            Event::Finished(exit) => {
                drop(state);
                let outcome = if exit == packagekit::EXIT_SUCCESS {
                    Outcome::Success
                } else if packagekit::is_cancelled_exit(exit) {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed(None)
                };
                self.change_finished(outcome, exit);
            }
            Event::Failed(reason) => {
                drop(state);
                self.change_finished(Outcome::Failed(Some(reason)), 0);
            }
        }
    }

    /// A trial or a real change ended: show its failure, ask or go on with
    /// the change after a trial, and list the packages afresh.
    fn change_finished(self: &Rc<Self>, outcome: Outcome, exit: u32) {
        let (change, real) = {
            let mut state = self.state.borrow_mut();
            match std::mem::replace(&mut state.activity, Activity::Idle) {
                Activity::Trial(change) => (change, false),
                Activity::Real(change) => (change, true),
                other => {
                    state.activity = other;
                    return;
                }
            }
        };
        let target = change.target.clone();
        let succeeded = matches!(outcome, Outcome::Success);
        let mut next_real = None;
        {
            let mut state = self.state.borrow_mut();
            let failed_here = state
                .failure
                .as_ref()
                .is_some_and(|(name, _)| *name == target.name);
            match outcome {
                Outcome::Success => {}
                Outcome::Cancelled => {
                    if failed_here {
                        state.failure = None;
                    }
                }
                Outcome::Failed(reason) => {
                    if let Some(reason) = reason {
                        state.failure = Some((target.name.clone(), reason));
                    } else if !failed_here {
                        let reason = packagekit::exit_text(exit).to_owned();
                        state.failure = Some((target.name.clone(), reason));
                    }
                }
            }
            if real {
                state.kept = Some((
                    target.name.clone(),
                    change_state(target.install, change.percentage),
                ));
            } else if succeeded && !change.cancel_requested {
                if target.install && change.removals.is_empty() {
                    next_real = Some(target.clone());
                } else {
                    let question = if target.install {
                        install_question(&target.name, &change.removals)
                    } else {
                        remove_question(&target.name, &change.removals)
                    };
                    state.activity = Activity::Question(target.clone(), question);
                }
            }
        }
        let restore = if self.is_wayland {
            real
        } else {
            change.waiting_for_auth
        };
        if restore {
            (self.hold_inputs)(true);
        }
        self.request_listing(true);
        match next_real {
            Some(target) => self.start_real(target),
            None => {
                self.update_message();
                self.update_states();
            }
        }
    }

    /// Escape: true when it dismissed the question or belongs to a running
    /// change, which is cancelled if the service allows it.
    fn cancel(&self) -> bool {
        let mut state = self.state.borrow_mut();
        match &mut state.activity {
            Activity::Idle => false,
            Activity::Question(..) => {
                state.activity = Activity::Idle;
                drop(state);
                self.update_message();
                true
            }
            Activity::Trial(change) | Activity::Real(change) => {
                if change.allow_cancel && !change.cancel_requested {
                    if let Some(transaction) = &change.transaction {
                        change.cancel_requested = true;
                        transaction.cancel();
                    }
                }
                true
            }
        }
    }
}

impl DialogPage for FeaturesPage {
    fn get_root(&self) -> gtk::Widget {
        self.inner.root.clone()
    }

    fn on_entry_changed(&self) {
        let inner = &self.inner;
        inner.query.replace(fold_text(inner.entry.text().as_str()));
        inner.filter.refilter();
        inner.view.selection().set_mode(gtk::SelectionMode::Single);
        *inner.path.borrow_mut() = select_first_row(&inner.view);
    }

    fn get_id(&self) -> &'static str {
        "features"
    }

    fn get_name(&self) -> &'static str {
        TITLE
    }

    fn get_icon_name(&self) -> &'static str {
        ICON
    }

    fn get_help(&self) -> &'static str {
        "This dialog lists the optional Regolith features for the running session: looks, bar blocks, compositors and window manager extensions. Installed features are marked. The user may filter the list in the top text box. Enter installs the selected feature, or removes it if it is installed, asking first when other packages would be removed. Escape cancels a question or a running change."
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Install or Remove Package"), ("escape", "Cancel")]
            .into_iter()
            .collect()
    }

    fn lists_common_keys(&self) -> bool {
        // Escape cancels here rather than exiting.
        false
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_enter_key(&event.keyval()) {
            self.inner.activate();
            return true;
        }
        navigate(&self.inner.view, &self.inner.path, event)
    }

    fn get_keybinding(&self) -> Option<char> {
        Some('e')
    }

    fn show(&self) {
        self.inner.first_show();
        self.inner.view.grab_focus();
    }

    fn cancel(&self) -> bool {
        self.inner.cancel()
    }

    fn keeps_open(&self) -> bool {
        self.inner.state.borrow().is_busy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(info: u32, id: &str, summary: &str) -> PackageEntry {
        PackageEntry {
            info,
            id: id.to_owned(),
            summary: summary.to_owned(),
        }
    }

    fn available(name: &str, summary: &str) -> PackageEntry {
        entry(2, &format!("{name};1;amd64;apt"), summary)
    }

    fn installed(name: &str, summary: &str) -> PackageEntry {
        entry(1, &format!("{name};1;amd64;installed:apt"), summary)
    }

    fn rows(features: &[Feature]) -> Vec<(&str, &str, &str, bool)> {
        features
            .iter()
            .map(|feature| {
                (
                    feature.kind.label(),
                    feature.name.as_str(),
                    feature.summary.as_str(),
                    feature.is_installed(),
                )
            })
            .collect()
    }

    #[test]
    fn prefixes_belong_to_their_sessions() {
        for (name, x11, wayland) in [
            ("regolith-look-a", Some(Kind::Looks), Some(Kind::Looks)),
            ("i3xrocks-net", Some(Kind::BarBlocks), None),
            ("regolith-compositor-p", Some(Kind::Compositors), None),
            (
                "regolith-wm-extra",
                Some(Kind::WindowManager),
                Some(Kind::WindowManager),
            ),
            ("regolith-i3-extra", Some(Kind::WindowManager), None),
            ("regolith-sway-extra", None, Some(Kind::WindowManager)),
            ("regolith-theme-x", None, None),
            ("prefix-regolith-look-x", None, None),
            ("regolith-look-x-config", None, None),
            ("regolith-wm-x-session", None, None),
            ("i3xrocks-x-default-style", None, None),
            ("regolith-sway-x-loader", None, None),
            (
                "regolith-look-loader-addon",
                Some(Kind::Looks),
                Some(Kind::Looks),
            ),
        ] {
            assert_eq!(kind_of(name, Session::X11), x11, "{name}");
            assert_eq!(kind_of(name, Session::Wayland), wayland, "{name}");
        }
        assert_eq!(kind_of("i3xrocks-net", Session::Unknown), None);
        assert_eq!(
            kind_of("regolith-look-a", Session::Unknown),
            Some(Kind::Looks)
        );
        assert_eq!(Session::from_wm_name("i3"), Session::X11);
        assert_eq!(Session::from_wm_name("sway"), Session::Wayland);
        assert_eq!(Session::from_wm_name("Unknown"), Session::Unknown);
    }

    #[test]
    fn transitional_summaries_start_with_the_words() {
        assert!(is_transitional("transitional package"));
        assert!(is_transitional("TrAnSiTiOnAl PaCkAgE for old look"));
        assert!(is_transitional("Transitional package: use regolith-look-b"));
        assert!(!is_transitional("Useful transitional package wording"));
        assert!(!is_transitional("transitional packages"));
        assert!(!is_transitional("transitional"));
        assert!(!is_transitional(""));
    }

    #[test]
    fn catalogue_groups_orders_and_merges_entries() {
        let entries = [
            installed("regolith-wm-common", "Common"),
            available("regolith-look-z", "Available first"),
            installed("regolith-look-z", "Installed summary"),
            available("regolith-look-a2", "First available"),
            available("regolith-look-a2", "Later available"),
            available("regolith-look-a10", "Ten"),
            installed("i3xrocks-net", "Traffic"),
            available("regolith-compositor-p", "Smooth"),
            available("regolith-i3-extra", "X11"),
            available("regolith-sway-extra", "Wayland"),
            installed("regolith-look-hidden-config", "Part"),
            available("unrelated", "regolith-look- in summary"),
        ];
        assert_eq!(
            rows(&catalogue(&entries, Session::X11)),
            [
                ("Looks", "regolith-look-a10", "Ten", false),
                ("Looks", "regolith-look-a2", "First available", false),
                ("Looks", "regolith-look-z", "Installed summary", true),
                ("Bar blocks", "i3xrocks-net", "Traffic", true),
                ("Compositors", "regolith-compositor-p", "Smooth", false),
                ("Window manager", "regolith-i3-extra", "X11", false),
                ("Window manager", "regolith-wm-common", "Common", true),
            ]
        );
        assert_eq!(
            rows(&catalogue(&entries, Session::Wayland)),
            [
                ("Looks", "regolith-look-a10", "Ten", false),
                ("Looks", "regolith-look-a2", "First available", false),
                ("Looks", "regolith-look-z", "Installed summary", true),
                ("Window manager", "regolith-sway-extra", "Wayland", false),
                ("Window manager", "regolith-wm-common", "Common", true),
            ]
        );
    }

    #[test]
    fn transitional_entries_are_left_out_one_by_one() {
        let entries = [
            installed("regolith-look-a", "Transitional package, obsolete"),
            available("regolith-look-a", "First remaining"),
            available("regolith-look-a", "Later remaining"),
            available("regolith-look-b", "transitional package only"),
        ];
        let features = catalogue(&entries, Session::X11);
        assert_eq!(
            rows(&features),
            [("Looks", "regolith-look-a", "First remaining", false)]
        );
        assert_eq!(
            features[0].available_id.as_deref(),
            Some("regolith-look-a;1;amd64;apt")
        );
        assert_eq!(features[0].installed_id, None);
    }

    #[test]
    fn installed_entries_are_told_by_their_data_field() {
        assert!(entry(2, "a;1;amd64;installed:apt", "").is_installed());
        assert!(entry(1, "a;1;amd64;apt", "").is_installed());
        assert!(!entry(2, "a;1;amd64;apt", "").is_installed());
        assert!(!entry(2, "a;1;amd64", "").is_installed());
        assert_eq!(
            package_name("regolith-look-a;1;amd64;apt"),
            "regolith-look-a"
        );
    }

    #[test]
    fn questions_list_names_once_in_byte_order() {
        let removals = ["z-dep", "a10-dep", "a2-dep", "z-dep"].map(str::to_owned);
        assert_eq!(
            install_question("regolith-look-a", &removals),
            "Install regolith-look-a and remove a10-dep, a2-dep, z-dep? Enter to confirm, Escape to cancel."
        );
        assert_eq!(
            remove_question("regolith-wm-x", &[]),
            "Remove regolith-wm-x? Enter to confirm, Escape to cancel."
        );
        assert_eq!(
            remove_question("regolith-wm-x", &removals[..2]),
            "Remove regolith-wm-x and also a10-dep, z-dep? Enter to confirm, Escape to cancel."
        );
        assert!(is_removal(13) && is_removal(15) && !is_removal(12));
    }

    #[test]
    fn change_states_show_known_progress_only() {
        assert_eq!(change_state(true, None), "Installing");
        assert_eq!(change_state(true, Some(0)), "Installing 0%");
        assert_eq!(change_state(false, Some(40)), "Removing 40%");
        assert_eq!(change_state(false, Some(100)), "Removing 100%");
        assert_eq!(change_state(true, Some(101)), "Installing");
    }

    #[test]
    fn filter_matches_name_or_summary_with_literal_spaces() {
        let query = |text: &str| fold_text(text);
        let (name, summary) = (fold_text("regolith-look-a"), fold_text("Amber ocean"));
        assert!(matches(&query("LOOK-A"), &name, &summary));
        assert!(matches(&query("amber ocean"), &name, &summary));
        assert!(matches(&query(""), &name, &summary));
        assert!(!matches(&query("amber  ocean"), &name, &summary));
        assert!(!matches(&query(" amber"), &name, &summary));
        assert!(!matches(&query("OCEAN "), &name, &summary));
        assert!(matches(&query("CAFÉ"), &name, &fold_text("Café summer")));
    }
}
