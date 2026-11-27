//! The Bluetooth panel: the devices the bluetooth daemon knows, to
//! connect, disconnect or pair.
//!
//! While the panel is open the adapters discover devices around: discovery
//! starts on every powered adapter that is not discovering already, and it
//! is stopped again whichever way the launcher leaves.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use super::{
    is_activation_key, report_failure, Answers, Busy, Journal, PanelList, Properties, Row, Runs,
    Service, Signal, Soon, Subscription, OBJECT_MANAGER, PROPERTIES,
};
use crate::pages::DialogPage;

const BUS_NAME: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";
const OBJECT_MANAGER_IFACE: &str = OBJECT_MANAGER;

const NO_ADAPTER: &str = "No Bluetooth adapter";
const POWERED_OFF: &str = "Bluetooth is off";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub path: String,
    pub powered: bool,
    pub discovering: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    pub alias: String,
    pub address: String,
    pub paired: bool,
    pub connected: bool,
    pub battery: Option<u32>,
}

impl Device {
    fn group(&self) -> u8 {
        if self.connected {
            0
        } else if self.paired {
            1
        } else {
            2
        }
    }

    fn state(&self) -> &'static str {
        if self.connected {
            "Connected"
        } else if self.paired {
            "Paired"
        } else {
            ""
        }
    }
}

/// What the daemon reported in one reading, adapters and devices in object
/// path order.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub adapters: Vec<Adapter>,
    pub devices: Vec<Device>,
}

/// The daemon's objects: path → interface → properties.
pub type Objects = BTreeMap<String, HashMap<String, Properties>>;

/// The objects of `GetManagedObjects`' reply, `a{oa{sa{sv}}}`.
pub fn objects(reply: &glib::Variant) -> Objects {
    let mut objects = Objects::new();
    if reply.type_().as_str() != "a{oa{sa{sv}}}" {
        return objects;
    }
    for object in reply.iter() {
        let Some(path) = object.child_value(0).str().map(str::to_owned) else {
            continue;
        };
        let entry = objects.entry(path).or_default();
        add_interfaces(entry, &object.child_value(1));
    }
    objects
}

/// Adds the interfaces of an `a{sa{sv}}` dictionary to an object.
fn add_interfaces(object: &mut HashMap<String, Properties>, interfaces: &glib::Variant) {
    for interface in interfaces.iter() {
        if let Some(name) = interface.child_value(0).str() {
            object.insert(
                name.to_owned(),
                Properties::from_variant(&interface.child_value(1)),
            );
        }
    }
}

/// Applies what a signal of the daemon announces to its objects; tells
/// whether it was something to apply. A property the signal invalidates,
/// announcing that it changed without its value, keeps the value known
/// until the interface is read again.
pub fn apply_signal(objects: &mut Objects, signal: &Signal) -> bool {
    match (signal.interface, signal.member) {
        (OBJECT_MANAGER_IFACE, "InterfacesAdded")
            if signal.parameters.type_().as_str() == "(oa{sa{sv}})" =>
        {
            let Some(path) = signal.parameters.child_value(0).str().map(str::to_owned) else {
                return false;
            };
            add_interfaces(
                objects.entry(path).or_default(),
                &signal.parameters.child_value(1),
            );
            true
        }
        (OBJECT_MANAGER_IFACE, "InterfacesRemoved")
            if signal.parameters.type_().as_str() == "(oas)" =>
        {
            let Some(path) = signal.parameters.child_value(0).str().map(str::to_owned) else {
                return false;
            };
            if let Some(object) = objects.get_mut(&path) {
                for interface in super::strings(&signal.parameters.child_value(1)) {
                    object.remove(&interface);
                }
                if object.is_empty() {
                    objects.remove(&path);
                }
            }
            true
        }
        _ => {
            let Some((interface, changes, _)) = signal.property_changes() else {
                return false;
            };
            match objects
                .get_mut(signal.path)
                .and_then(|object| object.get_mut(&interface))
            {
                Some(properties) => {
                    properties.merge(&changes, &[]);
                    true
                }
                None => false,
            }
        }
    }
}

/// Whether `signal` invalidates properties of `interface` on the object at
/// `path`.
fn invalidates(signal: &Signal, path: &str, interface: &str) -> bool {
    signal.path == path
        && signal
            .property_changes()
            .is_some_and(|(changed, _, invalidated)| {
                changed == interface && !invalidated.is_empty()
            })
}

/// The object an `InterfacesAdded` signal announces.
fn added_path(signal: &Signal) -> Option<String> {
    (signal.parameters.type_().as_str() == "(oa{sa{sv}})")
        .then(|| signal.parameters.child_value(0).str().map(str::to_owned))
        .flatten()
}

/// What the objects say, adapters and devices in object path order.
pub fn snapshot(objects: &Objects) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for (path, interfaces) in objects {
        if let Some(adapter) = interfaces.get(ADAPTER_IFACE) {
            snapshot.adapters.push(Adapter {
                path: path.clone(),
                powered: adapter.boolean("Powered"),
                discovering: adapter.boolean("Discovering"),
            });
        }
        if let Some(device) = interfaces.get(DEVICE_IFACE) {
            let paired = device.boolean("Paired");
            let connected = device.boolean("Connected");
            // Unnamed devices that are neither paired nor connected are
            // only noise from the discovery.
            if !(paired || connected || device.has("Name")) {
                continue;
            }
            let battery = interfaces
                .get(BATTERY_IFACE)
                .and_then(|battery| battery.get("Percentage"))
                .and_then(|percentage| super::number(&percentage));
            snapshot.devices.push(Device {
                path: path.clone(),
                alias: device.string("Alias"),
                address: device.string("Address"),
                paired,
                connected,
                battery,
            });
        }
    }
    snapshot
}

/// A name's sort key: Unicode full case folding, compared code point by
/// code point. GLib's casefold performs it, except that it lowercases
/// Cherokee capitals, which the case folding table keeps and folds the
/// small letters to; those are put back.
pub fn sort_key(name: &str) -> String {
    glib::casefold(name)
        .chars()
        .map(|c| {
            let folded = match u32::from(c) {
                small @ 0xAB70..=0xABBF => small - 0xAB70 + 0x13A0,
                small @ 0x13F8..=0x13FD => small - 8,
                other => other,
            };
            char::from_u32(folded).unwrap_or(c)
        })
        .collect()
}

/// The devices in listing order: connected, paired, the others; by
/// case-folded name, then address.
pub fn sorted_devices(devices: &[Device]) -> Vec<Device> {
    let mut devices = devices.to_vec();
    devices.sort_by_cached_key(|device| {
        (
            device.group(),
            sort_key(&device.alias),
            device.address.clone(),
        )
    });
    devices
}

/// What the panel lists for a reading, and the devices listed. Only a
/// `complete` reading, every object at once, tells the adapter status:
/// objects learned one by one list their devices.
pub fn listing(snapshot: &Snapshot, complete: bool) -> (Vec<Row>, Vec<Device>) {
    if complete && snapshot.adapters.is_empty() {
        return (vec![Row::status(NO_ADAPTER, "")], Vec::new());
    }
    if complete && snapshot.adapters.iter().all(|adapter| !adapter.powered) {
        return (vec![Row::status(POWERED_OFF, "Off")], Vec::new());
    }
    let devices = sorted_devices(&snapshot.devices);
    let rows = devices
        .iter()
        .map(|device| {
            let detail = device
                .battery
                .map(|percentage| format!("{percentage} %"))
                .unwrap_or_default();
            Row::new(
                device.alias.clone(),
                detail,
                device.state(),
                device.address.clone(),
            )
        })
        .collect();
    (rows, devices)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Discovery {
    /// Asked for, not answered yet.
    Pending,
    Accepted,
}

/// The discoveries of one owner of the daemon's name.
#[derive(Default)]
struct Status {
    adapters: Vec<Adapter>,
    devices: Vec<Device>,
    /// The adapters as the previous reading had them; `None` before the
    /// first.
    previous: Option<HashMap<String, bool>>,
    /// Adapters asked to discover during this run, at most once each.
    discovery_asked: HashSet<String>,
    /// The discoveries to stop when leaving.
    discoveries: HashMap<String, Discovery>,
}

impl Status {
    /// The adapters to ask for discovery after a new reading: on opening,
    /// the powered ones not discovering; then those that become powered,
    /// and those that appear powered and not discovering.
    fn adapters_to_discover(&mut self) -> Vec<String> {
        let previous = self.previous.take();
        let mut start = Vec::new();
        for adapter in &self.adapters {
            let trigger = match previous
                .as_ref()
                .map(|previous| previous.get(&adapter.path))
            {
                None | Some(None) => adapter.powered && !adapter.discovering,
                Some(Some(was_powered)) => adapter.powered && !was_powered,
            };
            if trigger && self.discovery_asked.insert(adapter.path.clone()) {
                start.push(adapter.path.clone());
            }
        }
        // Adapters that went away have nothing left to stop.
        let present: HashSet<&str> = self
            .adapters
            .iter()
            .map(|adapter| adapter.path.as_str())
            .collect();
        self.discoveries
            .retain(|path, _| present.contains(path.as_str()));
        self.previous = Some(
            self.adapters
                .iter()
                .map(|adapter| (adapter.path.clone(), adapter.powered))
                .collect(),
        );
        start
    }
}

/// The panel's state, shared with the daemon's answers and signals.
struct Panel {
    list: Rc<PanelList>,
    runs: Runs,
    answers: Answers,
    journal: Journal,
    soon: Soon,
    service: RefCell<Option<Service>>,
    subscription: RefCell<Option<Subscription>>,
    /// What the daemon answered and announced since the owner appeared.
    objects: RefCell<Objects>,
    /// The objects have been read once since the owner appeared.
    loaded: Cell<bool>,
    /// Readings of every object asked for and not answered yet.
    reading_all: Cell<usize>,
    /// The (object, interface) pairs being read on their own.
    reading: RefCell<HashSet<(String, &'static str)>>,
    /// The (object, interface) pairs read on their own, each with the
    /// journal mark of its question: newer than the answer to a reading of
    /// every object asked before.
    read_alone: RefCell<HashMap<(String, &'static str), u64>>,
    status: RefCell<Status>,
    busy: Busy,
    quit: Rc<dyn Fn()>,
}

pub struct BluetoothPage {
    panel: Rc<Panel>,
}

impl BluetoothPage {
    pub fn new(entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
        let panel = Rc::new(Panel {
            list: PanelList::new(entry),
            runs: Runs::default(),
            answers: Answers::default(),
            journal: Journal::default(),
            soon: Soon::default(),
            service: RefCell::default(),
            subscription: RefCell::default(),
            objects: RefCell::default(),
            loaded: Cell::new(false),
            reading_all: Cell::new(0),
            reading: RefCell::default(),
            read_alone: RefCell::default(),
            status: RefCell::default(),
            busy: Busy::default(),
            quit,
        });
        let for_exit = panel.clone();
        super::at_exit(move || for_exit.stop_discoveries());
        let for_click = Rc::downgrade(&panel);
        panel.list.connect_activated(move || {
            if let Some(panel) = for_click.upgrade() {
                panel.activate_selected();
            }
        });
        let for_owner = panel.clone();
        super::watch(BUS_NAME, move |service| for_owner.owner_changed(service));
        Rc::new(Self { panel })
    }
}

impl Panel {
    /// A new owner is like a fresh opening of the panel; with none, the
    /// devices and discoveries are gone with it.
    fn owner_changed(self: &Rc<Self>, service: Option<Service>) {
        let run = self.runs.next();
        self.subscription.replace(None);
        self.objects.borrow_mut().clear();
        self.loaded.set(false);
        self.reading_all.set(0);
        self.reading.borrow_mut().clear();
        self.read_alone.borrow_mut().clear();
        self.answers.clear();
        self.journal.clear();
        self.status.replace(Status::default());
        self.service.replace(service.clone());
        let Some(service) = service else {
            self.list.end_opening();
            self.list.set_rows(&[Row::status(NO_ADAPTER, "")]);
            return;
        };
        self.list.start_opening();
        self.list.set_rows(&[]);
        let panel = self.clone();
        let subscription = service.follow(&[PROPERTIES, OBJECT_MANAGER_IFACE], move |signal| {
            panel.on_signal(run, signal)
        });
        self.subscription.replace(Some(subscription));
        self.read(run, true);
    }

    /// Reads every object; `opening` for the read made on (re)opening,
    /// whose answer, whatever it is, ends the opening.
    fn read(self: &Rc<Self>, run: u64, opening: bool) {
        if !self.runs.is_current(run) {
            return;
        }
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        let number = self.answers.ask();
        let mark = self.journal.mark();
        self.reading_all.set(self.reading_all.get() + 1);
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service
                .read(
                    "/",
                    OBJECT_MANAGER_IFACE,
                    "GetManagedObjects",
                    None,
                    "(a{oa{sa{sv}}})",
                )
                .await;
            if !panel.runs.is_current(run) {
                return;
            }
            panel.reading_all.set(panel.reading_all.get() - 1);
            let Ok(reply) = reply else {
                if opening {
                    panel.list.end_opening();
                }
                return;
            };
            // The answer may predate signals that came while it was on its
            // way; what they announced is newer.
            let mut answered = objects(&reply.child_value(0));
            // Interfaces with properties invalidated meanwhile: the answer
            // may hold their older values.
            let mut stale = HashSet::new();
            let current = panel.journal.replay_since(mark, |signal| {
                apply_signal(&mut answered, signal);
                if let Some(interface) = [ADAPTER_IFACE, DEVICE_IFACE, BATTERY_IFACE]
                    .into_iter()
                    .find(|interface| invalidates(signal, signal.path, interface))
                {
                    stale.insert((signal.path.to_owned(), interface));
                }
            });
            if !current {
                // Too much came meanwhile to bring it up to date.
                panel.read(run, opening);
                return;
            }
            if opening {
                panel.list.end_opening();
            }
            if !panel.answers.take("", number) {
                return;
            }
            {
                // An interface read on its own after this question was asked
                // is newer than this answer; signals since have reached both.
                let objects = panel.objects.borrow();
                let mut read_alone = panel.read_alone.borrow_mut();
                read_alone.retain(|(path, interface), asked| {
                    if *asked <= mark {
                        return false;
                    }
                    if let Some(properties) =
                        objects.get(path).and_then(|object| object.get(*interface))
                    {
                        answered
                            .entry(path.clone())
                            .or_default()
                            .insert((*interface).to_owned(), properties.clone());
                    }
                    true
                });
            }
            panel.objects.replace(answered);
            panel.loaded.set(true);
            for (path, interface) in stale {
                panel.refresh(run, path, interface);
            }
            panel.update();
        });
    }

    /// Whether the objects known so far can be listed: once read, and
    /// before that from the moment an adapter or a device is among them.
    fn listable(&self) -> bool {
        self.loaded.get()
            || self.status.borrow().previous.is_some()
            || self.objects.borrow().values().any(|interfaces| {
                interfaces.contains_key(ADAPTER_IFACE) || interfaces.contains_key(DEVICE_IFACE)
            })
    }

    /// Applies what a signal announces right away. An object announced, or
    /// whose property change names an interface not known yet, has what
    /// its row needs read on its own, so that it shows without waiting for
    /// the reading of every object. A known interface whose properties a
    /// change invalidates is read again. Until the reading of every object
    /// has answered, a signal while none is under way (the last one failed)
    /// asks again.
    fn on_signal(self: &Rc<Self>, run: u64, signal: &Signal) {
        if !self.runs.is_current(run) {
            return;
        }
        self.journal.record(signal);
        let applied = apply_signal(&mut self.objects.borrow_mut(), signal);
        let changed = signal
            .property_changes()
            .and_then(|(interface, _, invalidated)| {
                [ADAPTER_IFACE, DEVICE_IFACE, BATTERY_IFACE]
                    .into_iter()
                    .find(|known| *known == interface)
                    .map(|interface| (interface, invalidated))
            });
        if applied {
            if signal.interface == OBJECT_MANAGER_IFACE && signal.member == "InterfacesAdded" {
                self.read_missing(run, added_path(signal));
            }
            if let Some((interface, invalidated)) = changed {
                if !invalidated.is_empty() {
                    self.refresh(run, signal.path.to_owned(), interface);
                }
            }
            if self.listable() {
                self.update();
            }
        } else if let Some((interface, _)) = changed {
            if interface == BATTERY_IFACE {
                self.read_object(run, signal.path.to_owned(), DEVICE_IFACE);
            }
            self.read_object(run, signal.path.to_owned(), interface);
        }
        if !self.loaded.get() && self.reading_all.get() == 0 {
            let panel = self.clone();
            // Due for whoever owns the name by then.
            self.soon.request(move || {
                if !panel.loaded.get() && panel.reading_all.get() == 0 {
                    panel.read(panel.runs.current(), false);
                }
            });
        }
    }

    /// Reads what the object at `path` needs to be listed and is not known
    /// yet: a battery's device, a device's adapter and, before the reading
    /// of every object has answered, its battery.
    fn read_missing(self: &Rc<Self>, run: u64, path: Option<String>) {
        let Some(path) = path else {
            return;
        };
        let (has_device, has_battery, adapter) = {
            let objects = self.objects.borrow();
            let Some(object) = objects.get(&path) else {
                return;
            };
            (
                object.contains_key(DEVICE_IFACE),
                object.contains_key(BATTERY_IFACE),
                object
                    .get(DEVICE_IFACE)
                    .map(|device| device.string("Adapter")),
            )
        };
        if has_battery && !has_device {
            self.read_object(run, path.clone(), DEVICE_IFACE);
        }
        if has_device && !has_battery && !self.loaded.get() {
            self.read_object(run, path, BATTERY_IFACE);
        }
        if let Some(adapter) = adapter {
            self.read_object(run, adapter, ADAPTER_IFACE);
        }
    }

    /// Reads `interface` of the object at `path` on its own, unless it is
    /// known or being read already. A device brings its battery and its
    /// adapter along, so that it can be listed.
    fn read_object(self: &Rc<Self>, run: u64, path: String, interface: &'static str) {
        if !self.runs.is_current(run) || path.is_empty() {
            return;
        }
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        let known = self
            .objects
            .borrow()
            .get(&path)
            .is_some_and(|object| object.contains_key(interface));
        if known || !self.reading.borrow_mut().insert((path.clone(), interface)) {
            return;
        }
        let mark = self.journal.mark();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service.properties(&path, interface).await;
            if !panel.runs.is_current(run) {
                return;
            }
            panel
                .reading
                .borrow_mut()
                .remove(&(path.clone(), interface));
            let Ok(properties) = reply else {
                return;
            };
            // Brought up to date like the reading of every object; gone
            // when what came meanwhile removed it.
            let mut answered = Objects::new();
            answered
                .entry(path.clone())
                .or_default()
                .insert(interface.to_owned(), properties);
            // A change that invalidated properties meanwhile found the
            // interface unknown: the answer may hold their older values.
            let mut stale = false;
            let current = panel.journal.replay_since(mark, |signal| {
                apply_signal(&mut answered, signal);
                stale |= invalidates(signal, &path, interface);
            });
            if !current {
                // Too much came meanwhile to bring it up to date.
                panel.read_object(run, path, interface);
                return;
            }
            let Some(properties) = answered
                .get_mut(&path)
                .and_then(|object| object.remove(interface))
            else {
                return;
            };
            {
                let mut objects = panel.objects.borrow_mut();
                let object = objects.entry(path.clone()).or_default();
                // Something as recent arrived meanwhile.
                if object.contains_key(interface) {
                    return;
                }
                object.insert(interface.to_owned(), properties);
            }
            panel.note_read_alone(path.clone(), interface, mark);
            if stale {
                panel.refresh(run, path.clone(), interface);
            }
            panel.read_missing(run, Some(path));
            if panel.listable() {
                panel.update();
            }
        });
    }

    /// Reads `interface` of the object at `path` again, since a change
    /// invalidated some of its properties; until the answer, they keep the
    /// values known. Only the answer to the latest such question counts,
    /// brought up to date like any other, and only while the interface is
    /// still there.
    fn refresh(self: &Rc<Self>, run: u64, path: String, interface: &'static str) {
        if !self.runs.is_current(run) {
            return;
        }
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        let number = self.answers.ask();
        let mark = self.journal.mark();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service.properties(&path, interface).await;
            if !panel.runs.is_current(run) {
                return;
            }
            let Ok(properties) = reply else {
                return;
            };
            let mut answered = Objects::new();
            answered
                .entry(path.clone())
                .or_default()
                .insert(interface.to_owned(), properties);
            let current = panel.journal.replay_since(mark, |signal| {
                apply_signal(&mut answered, signal);
            });
            if !current {
                // Too much came meanwhile to bring it up to date.
                panel.refresh(run, path, interface);
                return;
            }
            let Some(properties) = answered
                .get_mut(&path)
                .and_then(|object| object.remove(interface))
            else {
                return;
            };
            if !panel.answers.take(&format!("{path} {interface}"), number) {
                return;
            }
            {
                let mut objects = panel.objects.borrow_mut();
                let Some(known) = objects
                    .get_mut(&path)
                    .and_then(|object| object.get_mut(interface))
                else {
                    return;
                };
                *known = properties;
            }
            panel.note_read_alone(path, interface, mark);
            if panel.listable() {
                panel.update();
            }
        });
    }

    /// Records that `interface` of the object at `path` was read on its own
    /// by a question asked at `mark`.
    fn note_read_alone(&self, path: String, interface: &'static str, mark: u64) {
        let mut read_alone = self.read_alone.borrow_mut();
        let asked = read_alone.entry((path, interface)).or_insert(mark);
        *asked = (*asked).max(mark);
    }

    /// Lists what the objects say now, and starts the discoveries that
    /// are due.
    fn update(self: &Rc<Self>) {
        let snapshot = snapshot(&self.objects.borrow());
        let (rows, devices) = listing(&snapshot, self.loaded.get());
        let start = {
            let mut status = self.status.borrow_mut();
            status.adapters = snapshot.adapters;
            status.devices = devices;
            status.adapters_to_discover()
        };
        for path in start {
            self.start_discovery(path);
        }
        self.list.set_rows(&rows);
    }

    fn start_discovery(self: &Rc<Self>, path: String) {
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        self.status
            .borrow_mut()
            .discoveries
            .insert(path.clone(), Discovery::Pending);
        let run = self.runs.current();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service
                .read(&path, ADAPTER_IFACE, "StartDiscovery", None, "()")
                .await;
            if !panel.runs.is_current(run) {
                return;
            }
            let mut status = panel.status.borrow_mut();
            // An adapter that went away meanwhile is not stopped.
            if let Some(discovery) = status.discoveries.get_mut(&path) {
                match reply {
                    Ok(_) => *discovery = Discovery::Accepted,
                    Err(_) => {
                        status.discoveries.remove(&path);
                    }
                }
            }
        });
    }

    /// Stops the discoveries the panel started, the ones still unanswered
    /// included, and waits until the requests have left for the bus.
    fn stop_discoveries(&self) {
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        let status = self.status.borrow();
        let mut paths: Vec<&String> = status.discoveries.keys().collect();
        if paths.is_empty() {
            return;
        }
        paths.sort();
        for path in paths {
            let message = gio::DBusMessage::new_method_call(
                Some(BUS_NAME),
                path,
                Some(ADAPTER_IFACE),
                "StopDiscovery",
            );
            message.set_flags(
                gio::DBusMessageFlags::NO_REPLY_EXPECTED | gio::DBusMessageFlags::NO_AUTO_START,
            );
            let _ = service
                .connection
                .send_message(&message, gio::DBusSendMessageFlags::NONE);
        }
        let _ = service.connection.flush_sync(gio::Cancellable::NONE);
    }

    fn activate_selected(self: &Rc<Self>) {
        let Some((key, name)) = self.list.selected() else {
            return;
        };
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        let (adapters, device) = {
            let status = self.status.borrow();
            let device = status
                .devices
                .iter()
                .find(|device| device.address == key)
                .cloned();
            (status.adapters.clone(), device)
        };
        if key == Row::status(POWERED_OFF, "Off").key {
            if !self.busy.start() {
                return;
            }
            // Every adapter is asked at once, so that together they take no
            // longer than one action may.
            let calls: Vec<_> = adapters
                .iter()
                .filter(|adapter| !adapter.powered)
                .map(|adapter| {
                    let (service, path) = (service.clone(), adapter.path.clone());
                    glib::MainContext::default().spawn_local(async move {
                        service
                            .set_property(&path, ADAPTER_IFACE, "Powered", true.to_variant())
                            .await
                    })
                })
                .collect();
            let panel = self.clone();
            glib::MainContext::default().spawn_local(async move {
                // Stays open: the devices show up as the adapters power on.
                let mut failure = None;
                for call in calls {
                    if let Ok(Err(message)) = call.await {
                        failure.get_or_insert(message);
                    }
                }
                panel.busy.end();
                if let Some(message) = failure {
                    report_failure(&name, &message);
                }
            });
            return;
        }
        let Some(device) = device else {
            return;
        };
        if !self.busy.start() {
            return;
        }
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = connect_or_disconnect(&service, &device).await;
            panel.busy.end();
            match reply {
                Ok(()) => (panel.quit)(),
                Err(message) => report_failure(&name, &message),
            }
        });
    }
}

/// Disconnects a connected device and connects a paired one. Any other
/// is paired, trusted and connected, each step waiting for the one before.
async fn connect_or_disconnect(service: &Service, device: &Device) -> Result<(), String> {
    let path = device.path.as_str();
    if device.connected {
        return service
            .act(path, DEVICE_IFACE, "Disconnect", None)
            .await
            .map(|_| ());
    }
    if !device.paired {
        service.act(path, DEVICE_IFACE, "Pair", None).await?;
        service
            .set_property(path, DEVICE_IFACE, "Trusted", true.to_variant())
            .await?;
    }
    service
        .act(path, DEVICE_IFACE, "Connect", None)
        .await
        .map(|_| ())
}

impl DialogPage for BluetoothPage {
    fn get_root(&self) -> gtk::Widget {
        self.panel.list.root()
    }
    fn on_entry_changed(&self) {
        self.panel.list.filter_text_changed();
    }
    fn get_id(&self) -> &'static str {
        "bluetooth"
    }
    fn get_name(&self) -> &'static str {
        "<u>B</u>luetooth"
    }
    fn get_icon_name(&self) -> &'static str {
        "bluetooth-symbolic"
    }
    fn get_help(&self) -> &'static str {
        HELP
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Connect, Disconnect or Pair Device")]
            .into_iter()
            .collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_activation_key(event) {
            self.panel.activate_selected();
            return true;
        }
        self.panel.list.navigate(event)
    }
    fn get_keybinding(&self) -> Option<char> {
        Some('b')
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

pub const HELP: &str = "This dialog lists the Bluetooth devices nearby and known. Enter disconnects the highlighted device when it is connected, connects it when it is paired, and otherwise pairs, trusts and connects it. When Bluetooth is off, Enter switches it on.";

#[cfg(test)]
mod tests {
    use super::*;

    fn device(alias: &str, address: &str, paired: bool, connected: bool) -> Device {
        Device {
            path: format!("/org/bluez/hci0/dev_{}", address.replace(':', "_")),
            alias: alias.to_owned(),
            address: address.to_owned(),
            paired,
            connected,
            battery: None,
        }
    }

    fn adapter(path: &str, powered: bool, discovering: bool) -> Adapter {
        Adapter {
            path: path.to_owned(),
            powered,
            discovering,
        }
    }

    #[test]
    fn devices_sort_by_group_folded_name_then_address() {
        let devices = [
            device("Nomad Phone", "00:04", false, false),
            device("drift keys", "00:03", true, false),
            device("Drift Keyboard", "00:02", true, false),
            device("Aurora", "00:01", true, true),
            device("Straße", "00:06", true, false),
            device("STRASSE", "00:05", true, false),
        ];
        let names: Vec<String> = sorted_devices(&devices)
            .into_iter()
            .map(|device| device.alias)
            .collect();
        assert_eq!(
            names,
            [
                "Aurora",
                "Drift Keyboard",
                "drift keys",
                "STRASSE",
                "Straße",
                "Nomad Phone"
            ]
        );
    }

    #[test]
    fn sort_key_is_full_case_folding() {
        assert_eq!(sort_key("Straße"), "strasse");
        assert_eq!(sort_key("\u{FB00} device"), "ff device");
        assert_eq!(sort_key("\u{FF26}"), "\u{FF46}");
        assert_eq!(sort_key("Café"), "café");
        // Cherokee folds to the capitals, both cases alike.
        assert_eq!(sort_key("\u{13A0}\u{AB70}"), "\u{13A0}\u{13A0}");
        assert_eq!(sort_key("\u{13F0}\u{13F8}"), "\u{13F0}\u{13F0}");
    }

    #[test]
    fn status_rows_take_precedence() {
        let mut snapshot = Snapshot {
            devices: vec![device("Aurora", "00:01", true, true)],
            ..Snapshot::default()
        };
        assert_eq!(listing(&snapshot, true).0, [Row::status(NO_ADAPTER, "")]);
        snapshot.adapters = vec![
            adapter("/org/bluez/hci0", false, false),
            adapter("/org/bluez/hci1", false, false),
        ];
        assert_eq!(
            listing(&snapshot, true).0,
            [Row::status(POWERED_OFF, "Off")]
        );
        // Objects learned one by one never tell the adapter status.
        let partial = listing(&snapshot, false).0;
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].name, "Aurora");
        let without_adapters = Snapshot {
            adapters: Vec::new(),
            ..snapshot.clone()
        };
        assert_eq!(listing(&without_adapters, false).0, partial);
        snapshot.adapters[1].powered = true;
        let rows = listing(&snapshot, true).0;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].name.as_str(), rows[0].state.as_str()),
            ("Aurora", "Connected")
        );
    }

    #[test]
    fn managed_objects_admit_paired_connected_or_named_devices() {
        let mut objects: HashMap<
            glib::variant::ObjectPath,
            HashMap<String, HashMap<String, glib::Variant>>,
        > = HashMap::new();
        let mut add = |path: &str, interface: &str, properties: Vec<(&str, glib::Variant)>| {
            objects
                .entry(glib::variant::ObjectPath::try_from(path).unwrap())
                .or_default()
                .insert(
                    interface.to_owned(),
                    properties
                        .into_iter()
                        .map(|(name, value)| (name.to_owned(), value))
                        .collect(),
                );
        };
        add(
            "/org/bluez/hci0",
            ADAPTER_IFACE,
            vec![
                ("Powered", true.to_variant()),
                ("Discovering", false.to_variant()),
            ],
        );
        add(
            "/org/bluez/hci0/dev_1",
            DEVICE_IFACE,
            vec![
                ("Alias", "Named".to_variant()),
                ("Address", "00:01".to_variant()),
                ("Name", "".to_variant()),
            ],
        );
        add(
            "/org/bluez/hci0/dev_1",
            BATTERY_IFACE,
            vec![("Percentage", 0u8.to_variant())],
        );
        add(
            "/org/bluez/hci0/dev_2",
            DEVICE_IFACE,
            vec![
                ("Alias", "00:02".to_variant()),
                ("Address", "00:02".to_variant()),
            ],
        );
        add(
            "/org/bluez/hci0/dev_3",
            DEVICE_IFACE,
            vec![
                ("Alias", "".to_variant()),
                ("Address", "00:03".to_variant()),
                ("Paired", true.to_variant()),
            ],
        );
        let snapshot = snapshot(&super::objects(&objects.to_variant()));
        assert_eq!(snapshot.adapters, [adapter("/org/bluez/hci0", true, false)]);
        let cells: Vec<(String, String, String)> = listing(&snapshot, true)
            .0
            .into_iter()
            .map(|row| (row.name, row.detail, row.state))
            .collect();
        assert_eq!(
            cells,
            [
                (String::new(), String::new(), "Paired".to_owned()),
                ("Named".to_owned(), "0 %".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn discovery_starts_once_per_adapter_on_the_triggers() {
        let mut status = Status {
            adapters: vec![
                adapter("/hci0", true, false),
                adapter("/hci1", true, true),
                adapter("/hci2", false, false),
            ],
            ..Status::default()
        };
        assert_eq!(status.adapters_to_discover(), ["/hci0"]);
        // Already discovering: not started when it stops on its own.
        status.adapters[1].discovering = false;
        assert!(status.adapters_to_discover().is_empty());
        // Powered later, and an adapter appearing powered and idle.
        status.adapters[2].powered = true;
        status.adapters.push(adapter("/hci3", true, false));
        assert_eq!(status.adapters_to_discover(), ["/hci2", "/hci3"]);
        // A power cycle asks no second time.
        status.adapters[0].powered = false;
        assert!(status.adapters_to_discover().is_empty());
        status.adapters[0].powered = true;
        assert!(status.adapters_to_discover().is_empty());
    }

    #[test]
    fn vanished_adapters_are_not_stopped() {
        let mut status = Status {
            adapters: vec![adapter("/hci0", true, false), adapter("/hci4", true, false)],
            ..Status::default()
        };
        for path in status.adapters_to_discover() {
            status.discoveries.insert(path, Discovery::Accepted);
        }
        status.adapters.truncate(1);
        status.adapters_to_discover();
        assert_eq!(status.discoveries.keys().collect::<Vec<_>>(), ["/hci0"]);
    }

    #[test]
    fn signals_apply_to_the_objects() {
        let mut objects = Objects::new();
        let signal = |path: &'static str, interface: &'static str, member, parameters| {
            (path, interface, member, parameters)
        };
        let apply =
            |objects: &mut Objects,
             (path, interface, member, parameters): (&str, &str, &str, glib::Variant)| {
                apply_signal(
                    objects,
                    &Signal {
                        path,
                        interface,
                        member,
                        parameters: &parameters,
                    },
                )
            };
        let device: HashMap<String, HashMap<String, glib::Variant>> = HashMap::from([(
            DEVICE_IFACE.to_owned(),
            HashMap::from([
                ("Alias".to_owned(), "Aurora".to_variant()),
                ("Address".to_owned(), "00:01".to_variant()),
                ("Paired".to_owned(), true.to_variant()),
            ]),
        )]);
        let path = glib::variant::ObjectPath::try_from("/org/bluez/hci0/dev_1").unwrap();
        assert!(apply(
            &mut objects,
            signal(
                "/",
                OBJECT_MANAGER_IFACE,
                "InterfacesAdded",
                (path.clone(), device).to_variant()
            )
        ));
        let changes: HashMap<String, glib::Variant> =
            HashMap::from([("Connected".to_owned(), true.to_variant())]);
        assert!(apply(
            &mut objects,
            signal(
                "/org/bluez/hci0/dev_1",
                super::super::PROPERTIES,
                "PropertiesChanged",
                (DEVICE_IFACE, changes.clone(), Vec::<String>::new()).to_variant()
            )
        ));
        assert_eq!(snapshot(&objects).devices[0].state(), "Connected");
        // An invalidated property keeps its value until it is read again.
        assert!(apply(
            &mut objects,
            signal(
                "/org/bluez/hci0/dev_1",
                super::super::PROPERTIES,
                "PropertiesChanged",
                (
                    DEVICE_IFACE,
                    HashMap::<String, glib::Variant>::new(),
                    vec!["Alias".to_owned()]
                )
                    .to_variant()
            )
        ));
        assert_eq!(snapshot(&objects).devices[0].alias, "Aurora");
        let invalidation = (
            DEVICE_IFACE,
            HashMap::<String, glib::Variant>::new(),
            vec!["Alias".to_owned()],
        )
            .to_variant();
        let invalidating = Signal {
            path: "/org/bluez/hci0/dev_1",
            interface: super::super::PROPERTIES,
            member: "PropertiesChanged",
            parameters: &invalidation,
        };
        assert!(invalidates(
            &invalidating,
            "/org/bluez/hci0/dev_1",
            DEVICE_IFACE
        ));
        assert!(!invalidates(
            &invalidating,
            "/org/bluez/hci0/dev_1",
            BATTERY_IFACE
        ));
        assert!(!invalidates(
            &invalidating,
            "/org/bluez/hci0/dev_2",
            DEVICE_IFACE
        ));
        // A change for an interface not known is left to reading it alone.
        assert!(!apply(
            &mut objects,
            signal(
                "/org/bluez/hci0/dev_1",
                super::super::PROPERTIES,
                "PropertiesChanged",
                (BATTERY_IFACE, changes, Vec::<String>::new()).to_variant()
            )
        ));
        assert!(apply(
            &mut objects,
            signal(
                "/",
                OBJECT_MANAGER_IFACE,
                "InterfacesRemoved",
                (path, vec![DEVICE_IFACE.to_owned()]).to_variant()
            )
        ));
        assert!(objects.is_empty());
    }
}
