//! The power panel: the profiles of the power-profiles daemon, to switch
//! between.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use super::{
    is_activation_key, report_failure, Answers, Busy, Journal, PanelList, Properties, Row, Runs,
    Service, Signal, Soon, Subscription, PROPERTIES,
};
use crate::pages::DialogPage;

/// The daemon's names, current first, each with its object path and
/// interface (named like the bus name).
const DAEMONS: &[(&str, &str)] = &[
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];

const UNAVAILABLE: &str = "Power profiles are not available";

/// What the daemon reported in one reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The raw profile names, in the daemon's order.
    pub profiles: Vec<String>,
    pub active: String,
    pub degraded: String,
}

impl Snapshot {
    fn from_properties(properties: &Properties) -> Self {
        let profiles = properties
            .get("Profiles")
            .filter(|profiles| profiles.type_().as_str() == "aa{sv}")
            .map(|profiles| {
                profiles
                    .iter()
                    .filter_map(|profile| super::lookup(&profile, "Profile"))
                    .filter_map(|name| name.str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            profiles,
            active: properties.string("ActiveProfile"),
            degraded: properties.string("PerformanceDegraded"),
        }
    }
}

/// How a profile reads: the known ones by their titles, any other as named.
pub fn title(profile: &str) -> &str {
    match profile {
        "power-saver" => "Power Saver",
        "balanced" => "Balanced",
        "performance" => "Performance",
        other => other,
    }
}

/// One row per profile, in the daemon's order.
pub fn rows(snapshot: &Snapshot) -> Vec<Row> {
    snapshot
        .profiles
        .iter()
        .map(|profile| {
            let detail = if profile == "performance" && !snapshot.degraded.is_empty() {
                format!("degraded: {}", snapshot.degraded)
            } else {
                String::new()
            };
            let state = if *profile == snapshot.active {
                "Active"
            } else {
                ""
            };
            Row::new(title(profile), detail, state, profile.clone())
        })
        .collect()
}

/// Applies a property change the daemon at `path` announced for
/// `interface`; tells whether the signal was one.
fn apply_change(properties: &mut Properties, signal: &Signal, path: &str, interface: &str) -> bool {
    let Some((changed, changes, invalidated)) = signal.property_changes() else {
        return false;
    };
    if signal.path != path || changed != interface {
        return false;
    }
    properties.merge(&changes, &invalidated);
    true
}

/// The panel's state, shared with the service's answers and signals.
struct Panel {
    list: Rc<PanelList>,
    runs: Runs,
    answers: Answers,
    journal: Journal,
    soon: Soon,
    /// What each daemon name's owner is, in [`DAEMONS`] order: `None`
    /// until known.
    owners: RefCell<[Option<Option<Service>>; 2]>,
    /// The daemon talked to, with its object path.
    current: RefCell<Option<(Service, &'static str)>>,
    subscription: RefCell<Option<Subscription>>,
    /// The daemon's properties; `None` before its first answer or
    /// announcement.
    properties: RefCell<Option<Properties>>,
    busy: Busy,
    quit: Rc<dyn Fn()>,
}

pub struct PowerPage {
    panel: Rc<Panel>,
}

impl PowerPage {
    pub fn new(entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
        let panel = Rc::new(Panel {
            list: PanelList::new(entry),
            runs: Runs::default(),
            answers: Answers::default(),
            journal: Journal::default(),
            soon: Soon::default(),
            owners: RefCell::default(),
            current: RefCell::default(),
            subscription: RefCell::default(),
            properties: RefCell::default(),
            busy: Busy::default(),
            quit,
        });
        let for_click = Rc::downgrade(&panel);
        panel.list.connect_activated(move || {
            if let Some(panel) = for_click.upgrade() {
                panel.activate_selected();
            }
        });
        for (index, &(name, _)) in DAEMONS.iter().enumerate() {
            let panel = panel.clone();
            super::watch(name, move |service| panel.owner_changed(index, service));
        }
        Rc::new(Self { panel })
    }
}

impl Panel {
    fn owner_changed(self: &Rc<Self>, index: usize, service: Option<Service>) {
        self.owners.borrow_mut()[index] = Some(service);
        // The older name only stands in while the current one has no
        // owner; nothing is decided before that is known.
        let chosen = {
            let owners = self.owners.borrow();
            match &owners[0] {
                None => return,
                Some(Some(service)) => Some((service.clone(), DAEMONS[0].1)),
                Some(None) => match &owners[1] {
                    None => return,
                    Some(Some(service)) => Some((service.clone(), DAEMONS[1].1)),
                    Some(None) => None,
                },
            }
        };
        let unchanged = match (&*self.current.borrow(), &chosen) {
            (Some((current, _)), Some((service, _))) => current.same_as(service),
            (None, None) => !self.runs.is_current(0),
            _ => false,
        };
        if unchanged {
            return;
        }
        let run = self.runs.next();
        self.subscription.replace(None);
        self.properties.replace(None);
        self.answers.clear();
        self.journal.clear();
        self.current.replace(chosen.clone());
        let Some((service, _)) = chosen else {
            self.list.end_opening();
            self.list.set_rows(&[Row::status(UNAVAILABLE, "")]);
            return;
        };
        self.list.start_opening();
        // What the previous daemon said no longer holds.
        self.list.set_rows(&[]);
        let panel = self.clone();
        let subscription = service.follow(&[PROPERTIES, service.name], move |signal| {
            panel.on_signal(run, signal)
        });
        self.subscription.replace(Some(subscription));
        self.read(run, true);
    }

    /// Reads the daemon's properties; `opening` for the read made on
    /// (re)opening, whose answer, whatever it is, ends the opening.
    fn read(self: &Rc<Self>, run: u64, opening: bool) {
        if !self.runs.is_current(run) {
            return;
        }
        let Some((service, path)) = self.current.borrow().clone() else {
            return;
        };
        let number = self.answers.ask();
        let mark = self.journal.mark();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service.properties(path, service.name).await;
            if !panel.runs.is_current(run) {
                return;
            }
            if opening {
                panel.list.end_opening();
            }
            let Ok(mut properties) = reply else {
                return;
            };
            // The answer may predate signals that came while it was on its
            // way; what they announced is newer.
            let current = panel.journal.replay_since(mark, |signal| {
                apply_change(&mut properties, signal, path, service.name);
            });
            if current && panel.answers.take("", number) {
                panel.properties.replace(Some(properties));
                panel.render();
            }
        });
    }

    /// Applies what a signal announces right away, even before the first
    /// answer, and reads the daemon again soon in case it announced less
    /// than changed.
    fn on_signal(self: &Rc<Self>, run: u64, signal: &Signal) {
        if !self.runs.is_current(run) {
            return;
        }
        let Some((service, path)) = self.current.borrow().clone() else {
            return;
        };
        self.journal.record(signal);
        let mut properties = self.properties.borrow().clone().unwrap_or_default();
        if apply_change(&mut properties, signal, path, service.name) {
            self.properties.replace(Some(properties));
            self.render();
        }
        let panel = self.clone();
        // Due for whoever owns the name by then.
        self.soon
            .request(move || panel.read(panel.runs.current(), false));
    }

    fn render(&self) {
        let Some(snapshot) = self
            .properties
            .borrow()
            .as_ref()
            .map(Snapshot::from_properties)
        else {
            return;
        };
        self.list.set_rows(&rows(&snapshot));
    }

    fn activate_selected(self: &Rc<Self>) {
        let Some((profile, name)) = self.list.selected() else {
            return;
        };
        let Some((service, path)) = self.current.borrow().clone() else {
            return;
        };
        let Some(snapshot) = self
            .properties
            .borrow()
            .as_ref()
            .map(Snapshot::from_properties)
        else {
            return;
        };
        if !snapshot.profiles.contains(&profile) {
            return;
        }
        if profile == snapshot.active {
            (self.quit)();
            return;
        }
        if !self.busy.start() {
            return;
        }
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service
                .set_property(path, service.name, "ActiveProfile", profile.to_variant())
                .await;
            panel.busy.end();
            match reply {
                Ok(()) => (panel.quit)(),
                Err(message) => report_failure(&name, &message),
            }
        });
    }
}

impl DialogPage for PowerPage {
    fn get_root(&self) -> gtk::Widget {
        self.panel.list.root()
    }
    fn on_entry_changed(&self) {
        self.panel.list.filter_text_changed();
    }
    fn get_id(&self) -> &'static str {
        "power"
    }
    fn get_name(&self) -> &'static str {
        "<u>P</u>ower"
    }
    fn get_icon_name(&self) -> &'static str {
        "power-profile-balanced-symbolic"
    }
    fn get_help(&self) -> &'static str {
        HELP
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Switch to Profile")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_activation_key(event) {
            self.panel.activate_selected();
            return true;
        }
        self.panel.list.navigate(event)
    }
    fn get_keybinding(&self) -> Option<char> {
        Some('p')
    }
    fn show(&self) {
        self.panel.list.view().grab_focus();
    }
    /// A dialog an action brings up (a polkit agent's, a pairing agent's)
    /// takes the focus without ending the action.
    fn is_launching(&self) -> bool {
        self.panel.busy.is_set()
    }
}

pub const HELP: &str =
    "This dialog lists the power profiles. Enter switches to the highlighted profile.";

#[cfg(test)]
mod tests {
    use super::{rows, title, Snapshot};

    fn cells(snapshot: &Snapshot) -> Vec<(String, String, String)> {
        rows(snapshot)
            .into_iter()
            .map(|row| (row.name, row.detail, row.state))
            .collect()
    }

    #[test]
    fn known_profiles_get_titles() {
        assert_eq!(title("power-saver"), "Power Saver");
        assert_eq!(title("balanced"), "Balanced");
        assert_eq!(title("performance"), "Performance");
        assert_eq!(title("Balanced"), "Balanced");
        assert_eq!(title("whisper"), "whisper");
    }

    #[test]
    fn rows_keep_the_daemon_order_and_mark_the_active_profile() {
        let snapshot = Snapshot {
            profiles: ["performance", "whisper", "power-saver", "Balanced"]
                .map(String::from)
                .to_vec(),
            active: "whisper".to_owned(),
            degraded: "battery-low".to_owned(),
        };
        let cell = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
        assert_eq!(
            cells(&snapshot),
            [
                cell("Performance", "degraded: battery-low", ""),
                cell("whisper", "", "Active"),
                cell("Power Saver", "", ""),
                cell("Balanced", "", ""),
            ]
        );
        let keys: Vec<String> = rows(&snapshot).into_iter().map(|row| row.key).collect();
        assert_eq!(keys, ["performance", "whisper", "power-saver", "Balanced"]);
    }
}
