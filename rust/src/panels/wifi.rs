//! The Wi-Fi panel: the networks the network manager's Wi-Fi devices see,
//! one row per SSID, to connect to or disconnect from.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use super::{
    is_activation_key, report_failure, report_failure_hiding, Answers, Busy, Journal, PanelList,
    Properties, Row, Runs, Service, Signal, Soon, Subscription, OBJECT_MANAGER, PROPERTIES,
};
use crate::pages::DialogPage;

const BUS_NAME: &str = "org.freedesktop.NetworkManager";
const MANAGER: &str = "/org/freedesktop/NetworkManager";
const MANAGER_IFACE: &str = "org.freedesktop.NetworkManager";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";

/// The interfaces whose signals the panel follows.
const FOLLOWED: &[&str] = &[
    PROPERTIES,
    OBJECT_MANAGER,
    MANAGER_IFACE,
    SETTINGS_IFACE,
    CONNECTION_IFACE,
    DEVICE_IFACE,
    WIRELESS_IFACE,
    ACCESS_POINT_IFACE,
    ACTIVE_IFACE,
];

const DEVICE_TYPE_WIFI: u32 = 2;
const ACTIVE_ACTIVATING: u32 = 1;
const ACTIVE_ACTIVATED: u32 = 2;
const AP_FLAGS_PRIVACY: u32 = 0x1;
const KEY_MGMT_PSK: u32 = 0x100;
const KEY_MGMT_802_1X: u32 = 0x200;
const KEY_MGMT_SAE: u32 = 0x400;
/// Opportunistic wireless encryption: open to join, encrypted on the air.
const KEY_MGMT_OWE: u32 = 0x800;
/// The open network of an OWE transition pair, which NetworkManager joins
/// with OWE as it does an OWE network.
const KEY_MGMT_OWE_TM: u32 = 0x1000;
/// WPA3-Enterprise 192-bit: 802.1X with the Suite B ciphers.
const KEY_MGMT_EAP_SUITE_B_192: u32 = 0x2000;

const UNAVAILABLE: &str = "Wi-Fi is not available";
const NO_ADAPTER: &str = "No Wi-Fi adapter";
const RADIO_OFF: &str = "Wi-Fi is off";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub flags: u32,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    pub path: String,
    pub is_wifi: bool,
    /// The access points in the order the device reports them.
    pub access_points: Vec<String>,
    /// The active connection, `/` for none.
    pub active_connection: String,
}

/// What the network manager reported in one reading.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// False only when the manager says Wi-Fi is off and `GetDevices` has
    /// answered; until then the networks known are listed.
    pub wireless_enabled: bool,
    /// In `GetDevices` order.
    pub devices: Vec<Device>,
    pub access_points: HashMap<String, AccessPoint>,
    /// Active connection → (`SpecificObject`, `State`).
    pub active: HashMap<String, (String, u32)>,
    /// Saved connections in `ListConnections` order, with their SSIDs.
    pub saved: Vec<(String, Vec<u8>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Through the active connection.
    Connected(String),
    Connecting(String),
    /// The first saved connection for the SSID.
    Saved(String),
    Open,
    Enterprise,
    Secured,
}

impl State {
    fn label(&self) -> &'static str {
        match self {
            Self::Connected(_) => "Connected",
            Self::Connecting(_) => "Connecting",
            Self::Saved(_) => "Saved",
            Self::Open => "Open",
            Self::Enterprise => "Enterprise",
            Self::Secured => "Secured",
        }
    }

    fn is_active(&self) -> bool {
        matches!(self, Self::Connected(_) | Self::Connecting(_))
    }
}

/// One listed network: an SSID, represented by its strongest access point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: Vec<u8>,
    pub name: String,
    pub strength: u8,
    pub device: String,
    pub access_point: String,
    pub flags: u32,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
    pub state: State,
}

impl Network {
    fn key(&self) -> String {
        ssid_key(&self.ssid)
    }

    /// An OWE network, or the open one of an OWE transition pair, joined
    /// without a passphrase.
    pub fn is_owe(&self) -> bool {
        self.rsn_flags & (KEY_MGMT_OWE | KEY_MGMT_OWE_TM) != 0
    }

    /// A WEP network: privacy without any WPA or RSN key management.
    pub fn is_wep(&self) -> bool {
        self.flags & AP_FLAGS_PRIVACY != 0 && self.wpa_flags == 0 && self.rsn_flags == 0
    }

    /// The key management a passphrase connects with: SAE for an
    /// SAE-only network, WPA-PSK otherwise.
    pub fn key_management(&self) -> &'static str {
        let any = self.wpa_flags | self.rsn_flags;
        if self.rsn_flags & KEY_MGMT_SAE != 0 && any & KEY_MGMT_PSK == 0 {
            "sae"
        } else {
            "wpa-psk"
        }
    }
}

/// A row key for the SSID: its bytes, so that SSIDs that decode alike stay
/// apart.
fn ssid_key(ssid: &[u8]) -> String {
    ssid.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn security(access_point: &AccessPoint) -> State {
    let any = access_point.wpa_flags | access_point.rsn_flags;
    if (access_point.flags & AP_FLAGS_PRIVACY == 0 && any == 0)
        || access_point.rsn_flags & (KEY_MGMT_OWE | KEY_MGMT_OWE_TM) != 0
    {
        State::Open
    } else if any & (KEY_MGMT_802_1X | KEY_MGMT_EAP_SUITE_B_192) != 0
        && any & (KEY_MGMT_PSK | KEY_MGMT_SAE) == 0
    {
        State::Enterprise
    } else {
        State::Secured
    }
}

/// The access points sharing one SSID while [`networks`] gathers them.
struct Found<'a> {
    /// The device of the strongest access point, and its path.
    device: &'a str,
    path: &'a str,
    best: &'a AccessPoint,
    /// Every access point of the SSID, on any device.
    all: HashSet<&'a str>,
}

/// The listed networks, in listing order.
pub fn networks(snapshot: &Snapshot) -> Vec<Network> {
    let wifi_devices: Vec<&Device> = snapshot
        .devices
        .iter()
        .filter(|device| device.is_wifi)
        .collect();
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut found: HashMap<Vec<u8>, Found> = HashMap::new();
    for device in &wifi_devices {
        for path in &device.access_points {
            let Some(access_point) = snapshot.access_points.get(path) else {
                continue;
            };
            if access_point.ssid.is_empty() {
                continue;
            }
            match found.get_mut(&access_point.ssid) {
                Some(network) => {
                    network.all.insert(path);
                    // Ties keep the earlier access point.
                    if access_point.strength > network.best.strength {
                        network.device = &device.path;
                        network.path = path;
                        network.best = access_point;
                    }
                }
                None => {
                    order.push(access_point.ssid.clone());
                    found.insert(
                        access_point.ssid.clone(),
                        Found {
                            device: &device.path,
                            path,
                            best: access_point,
                            all: HashSet::from([path.as_str()]),
                        },
                    );
                }
            }
        }
    }
    let mut networks: Vec<Network> = order
        .into_iter()
        .map(|ssid| {
            let Found {
                device,
                path,
                best: access_point,
                all,
            } = &found[&ssid];
            let mut active = None;
            for wifi in &wifi_devices {
                let Some((specific, state)) = snapshot.active.get(&wifi.active_connection) else {
                    continue;
                };
                if !all.contains(specific.as_str()) {
                    continue;
                }
                let connection = wifi.active_connection.clone();
                match *state {
                    ACTIVE_ACTIVATED => {
                        active = Some(State::Connected(connection));
                        break;
                    }
                    ACTIVE_ACTIVATING if active.is_none() => {
                        active = Some(State::Connecting(connection))
                    }
                    _ => {}
                }
            }
            let saved = snapshot
                .saved
                .iter()
                .find(|(_, saved_ssid)| *saved_ssid == ssid)
                .map(|(connection, _)| State::Saved(connection.clone()));
            Network {
                name: String::from_utf8_lossy(&ssid).into_owned(),
                strength: access_point.strength,
                device: (*device).to_owned(),
                access_point: (*path).to_owned(),
                flags: access_point.flags,
                wpa_flags: access_point.wpa_flags,
                rsn_flags: access_point.rsn_flags,
                state: active.or(saved).unwrap_or_else(|| security(access_point)),
                ssid,
            }
        })
        .collect();
    networks.sort_by(|a, b| {
        b.state
            .is_active()
            .cmp(&a.state.is_active())
            .then_with(|| b.strength.cmp(&a.strength))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    networks
}

/// What the panel lists for a reading.
pub fn listing(snapshot: &Snapshot) -> (Vec<Row>, Vec<Network>) {
    if !snapshot.devices.iter().any(|device| device.is_wifi) {
        return (vec![Row::status(NO_ADAPTER, "")], Vec::new());
    }
    if !snapshot.wireless_enabled {
        return (vec![Row::status(RADIO_OFF, "Off")], Vec::new());
    }
    let networks = networks(snapshot);
    let rows = networks
        .iter()
        .map(|network| {
            Row::new(
                network.name.clone(),
                format!("{} %", network.strength),
                network.state.label(),
                network.key(),
            )
        })
        .collect();
    (rows, networks)
}

/// A passphrase the network manager takes: 8 to 63 characters, or 64
/// hexadecimal digits.
pub fn is_valid_passphrase(passphrase: &str) -> bool {
    let length = passphrase.chars().count();
    (8..=63).contains(&length)
        || (length == 64 && passphrase.chars().all(|c| c.is_ascii_hexdigit()))
}

fn access_point(properties: &Properties) -> AccessPoint {
    AccessPoint {
        ssid: properties.bytes("Ssid"),
        strength: properties.number("Strength").min(255) as u8,
        flags: properties.number("Flags"),
        wpa_flags: properties.number("WpaFlags"),
        rsn_flags: properties.number("RsnFlags"),
    }
}

/// The settings for a new connection to `network`, with the passphrase
/// when one is given; an OWE network joined without one asks for OWE.
fn new_connection(network: &Network, passphrase: Option<&str>) -> glib::Variant {
    let mut settings: BTreeMap<String, HashMap<String, glib::Variant>> = BTreeMap::new();
    settings.insert(
        "802-11-wireless".to_owned(),
        HashMap::from([(
            "ssid".to_owned(),
            glib::Variant::array_from_fixed_array(&network.ssid),
        )]),
    );
    match passphrase {
        Some(passphrase) => {
            settings.insert(
                "802-11-wireless-security".to_owned(),
                HashMap::from([
                    ("key-mgmt".to_owned(), network.key_management().to_variant()),
                    ("psk".to_owned(), passphrase.to_variant()),
                ]),
            );
        }
        None if network.is_owe() => {
            settings.insert(
                "802-11-wireless-security".to_owned(),
                HashMap::from([("key-mgmt".to_owned(), "owe".to_variant())]),
            );
        }
        None => {}
    }
    let settings: HashMap<String, HashMap<String, glib::Variant>> = settings.into_iter().collect();
    settings.to_variant()
}

/// What the network manager has answered so far, object by object.
#[derive(Default)]
struct Cache {
    manager: Option<Properties>,
    /// The devices in `GetDevices` order.
    devices: Option<Vec<String>>,
    /// `GetDevices` has answered. Until then `devices` holds only what was
    /// announced (even a `Devices` change, which may predate the answer),
    /// and no adapter status is inferred from it.
    devices_complete: bool,
    /// (object path, interface) → properties.
    objects: HashMap<(String, String), Properties>,
    /// What was announced for interfaces not read yet, until it holds
    /// everything the panel takes from them; then they count as read.
    partial: HashMap<(String, String), Properties>,
    /// The saved connections in `ListConnections` order, with those
    /// announced since appended; while `ListConnections` is unanswered,
    /// only those announced.
    saved: Option<Vec<String>>,
    /// Saved connection → its SSID, `None` for one without.
    settings: HashMap<String, Option<Vec<u8>>>,
}

impl Cache {
    fn object(&self, path: &str, interface: &str) -> Option<&Properties> {
        self.objects.get(&(path.to_owned(), interface.to_owned()))
    }

    /// Takes the answer to reading `interface` of the object at `path`.
    fn read(&mut self, path: String, interface: &str, properties: Properties) {
        let key = (path, interface.to_owned());
        self.partial.remove(&key);
        self.objects.insert(key, properties);
    }

    /// Applies a property change announced for `interface` of the object
    /// at `path`. One not read yet collects what is announced for it, and
    /// counts as read as soon as that is everything the panel takes from
    /// it, whether its reading has answered or not.
    fn apply_change(
        &mut self,
        path: &str,
        interface: &str,
        changes: &Properties,
        invalidated: &[String],
    ) -> Applied {
        let key = (path.to_owned(), interface.to_owned());
        if let Some(properties) = self.objects.get_mut(&key) {
            properties.merge(changes, invalidated);
            return Applied::Merged;
        }
        if !PIECEMEAL.contains(&interface) {
            return Applied::Nothing;
        }
        let partial = self.partial.entry(key.clone()).or_default();
        partial.merge(changes, invalidated);
        if !is_complete(interface, partial) {
            return Applied::Nothing;
        }
        let properties = self.partial.remove(&key).unwrap_or_default();
        self.objects.insert(key, properties.clone());
        Applied::Completed(properties)
    }

    /// What can be listed from the answers so far; `None` while that is
    /// nothing: the devices are not known yet, none of those known is a
    /// Wi-Fi device while others are still unknown (or the list is only
    /// what was announced), or `GetDevices` has answered with a Wi-Fi
    /// device but whether Wi-Fi is on is not known. While the list
    /// is only what was announced, neither status row is inferred: the
    /// networks of the Wi-Fi devices known so far are listed.
    fn snapshot(&self) -> Option<Snapshot> {
        let paths = self.devices.as_ref()?;
        let mut snapshot = Snapshot::default();
        let mut unknown = false;
        for path in paths {
            let Some(properties) = self.object(path, DEVICE_IFACE) else {
                unknown = true;
                continue;
            };
            let is_wifi = properties.number("DeviceType") == DEVICE_TYPE_WIFI;
            let mut device = Device {
                path: path.clone(),
                is_wifi,
                active_connection: properties.string("ActiveConnection"),
                ..Device::default()
            };
            if is_wifi {
                device.access_points = self
                    .object(path, WIRELESS_IFACE)
                    .map(|wireless| wireless.paths("AccessPoints"))
                    .unwrap_or_default();
                for access_point_path in &device.access_points {
                    if let Some(properties) = self.object(access_point_path, ACCESS_POINT_IFACE) {
                        snapshot
                            .access_points
                            .insert(access_point_path.clone(), access_point(properties));
                    }
                }
                if let Some(active) = self.object(&device.active_connection, ACTIVE_IFACE) {
                    snapshot.active.insert(
                        device.active_connection.clone(),
                        (active.string("SpecificObject"), active.number("State")),
                    );
                }
            }
            snapshot.devices.push(device);
        }
        if snapshot.devices.iter().any(|device| device.is_wifi) {
            snapshot.wireless_enabled =
                !self.devices_complete || self.manager.as_ref()?.boolean("WirelessEnabled");
        } else if unknown || !self.devices_complete {
            return None;
        }
        snapshot.saved = self
            .saved
            .iter()
            .flatten()
            .filter_map(|path| {
                let ssid = self.settings.get(path)?.clone()?;
                Some((path.clone(), ssid))
            })
            .collect();
        Some(snapshot)
    }
}

/// The interfaces whose announced properties can stand in for reading
/// them.
const PIECEMEAL: &[&str] = &[
    DEVICE_IFACE,
    WIRELESS_IFACE,
    ACCESS_POINT_IFACE,
    ACTIVE_IFACE,
];

/// Whether `properties` hold everything the panel takes from `interface`.
fn is_complete(interface: &str, properties: &Properties) -> bool {
    let needed: &[&str] = match interface {
        DEVICE_IFACE if properties.number("DeviceType") != DEVICE_TYPE_WIFI => &["DeviceType"],
        DEVICE_IFACE => &["DeviceType", "ActiveConnection"],
        WIRELESS_IFACE => &["AccessPoints"],
        ACCESS_POINT_IFACE => &["Ssid", "Strength", "Flags", "WpaFlags", "RsnFlags"],
        ACTIVE_IFACE => &["SpecificObject", "State"],
        _ => return false,
    };
    needed.iter().all(|name| properties.has(name))
}

/// What a property change did to the cache.
enum Applied {
    Nothing,
    /// Merged into what was known.
    Merged,
    /// Completed an interface not read yet: these are its properties.
    Completed(Properties),
}

/// Applies a property change announced for `interface` of the object at
/// `path`; tells whether the signal was one.
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

/// What the manager announces about its devices.
#[derive(Debug, PartialEq, Eq)]
enum DevicesChange {
    /// `Devices` changed: the whole list.
    All(Vec<String>),
    Added(String),
    Removed(String),
}

impl DevicesChange {
    fn of(signal: &Signal) -> Option<Self> {
        if signal.path != MANAGER {
            return None;
        }
        if let Some((interface, changes, _)) = signal.property_changes() {
            return (interface == MANAGER_IFACE && changes.has("Devices"))
                .then(|| Self::All(changes.paths("Devices")));
        }
        if signal.interface != MANAGER_IFACE || signal.parameters.type_().as_str() != "(o)" {
            return None;
        }
        let device = signal.parameters.child_value(0).str()?.to_owned();
        match signal.member {
            "DeviceAdded" => Some(Self::Added(device)),
            "DeviceRemoved" => Some(Self::Removed(device)),
            _ => None,
        }
    }

    fn apply(&self, devices: &mut Vec<String>) {
        match self {
            Self::All(all) => devices.clone_from(all),
            Self::Added(device) => {
                if !devices.contains(device) {
                    devices.push(device.clone());
                }
            }
            Self::Removed(device) => devices.retain(|path| path != device),
        }
    }
}

/// What the settings service announces about the saved connections.
#[derive(Debug, PartialEq, Eq)]
enum SavedChange {
    Added(String),
    Removed(String),
}

impl SavedChange {
    fn of(signal: &Signal) -> Option<Self> {
        match (signal.interface, signal.member) {
            (SETTINGS_IFACE, "NewConnection" | "ConnectionRemoved")
                if signal.path == SETTINGS && signal.parameters.type_().as_str() == "(o)" =>
            {
                let connection = signal.parameters.child_value(0).str()?.to_owned();
                Some(if signal.member == "NewConnection" {
                    Self::Added(connection)
                } else {
                    Self::Removed(connection)
                })
            }
            // A connection also announces its own removal.
            (CONNECTION_IFACE, "Removed") => Some(Self::Removed(signal.path.to_owned())),
            _ => None,
        }
    }

    fn apply(&self, saved: &mut Vec<String>) {
        match self {
            Self::Added(connection) => {
                if !saved.contains(connection) {
                    saved.push(connection.clone());
                }
            }
            Self::Removed(connection) => saved.retain(|path| path != connection),
        }
    }
}

/// What reading a saved connection's settings told.
struct SavedSettings {
    /// The connection is still there.
    exists: bool,
    /// Its SSID, `None` for one without.
    ssid: Option<Vec<u8>>,
}

/// The panel's state, shared with the service's answers and signals.
struct Panel {
    list: Rc<PanelList>,
    runs: Runs,
    answers: Answers,
    journal: Journal,
    soon: Soon,
    service: RefCell<Option<Service>>,
    subscription: RefCell<Option<Subscription>>,
    cache: RefCell<Cache>,
    /// The devices asked for a scan since the owner appeared.
    scanned: RefCell<HashSet<String>>,
    /// The opening reads still unanswered.
    opening: Cell<usize>,
    /// Set while the panel asks on opening or takes an opening answer: what
    /// it asks then is an opening read too.
    asking_opening: Cell<bool>,
    networks: RefCell<Vec<Network>>,
    /// Shown is a status row, with Enter switching the radio on.
    radio_off: Cell<bool>,
    busy: Busy,
    quit: Rc<dyn Fn()>,
}

pub struct WifiPage {
    panel: Rc<Panel>,
}

impl WifiPage {
    pub fn new(entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
        let panel = Rc::new(Panel {
            list: PanelList::new(entry),
            runs: Runs::default(),
            answers: Answers::default(),
            journal: Journal::default(),
            soon: Soon::default(),
            service: RefCell::default(),
            subscription: RefCell::default(),
            cache: RefCell::default(),
            scanned: RefCell::default(),
            opening: Cell::new(0),
            asking_opening: Cell::new(false),
            networks: RefCell::default(),
            radio_off: Cell::new(false),
            busy: Busy::default(),
            quit,
        });
        let for_click = Rc::downgrade(&panel);
        panel.list.connect_activated(move || {
            if let Some(panel) = for_click.upgrade() {
                panel.activate();
            }
        });
        let for_owner = panel.clone();
        super::watch(BUS_NAME, move |service| for_owner.owner_changed(service));
        Rc::new(Self { panel })
    }
}

impl Panel {
    /// A new owner is like a fresh opening of the panel; with none, the
    /// networks are gone with it.
    fn owner_changed(self: &Rc<Self>, service: Option<Service>) {
        let run = self.runs.next();
        self.subscription.replace(None);
        self.cache.replace(Cache::default());
        self.scanned.borrow_mut().clear();
        self.opening.set(0);
        self.answers.clear();
        self.journal.clear();
        self.networks.borrow_mut().clear();
        self.radio_off.set(false);
        self.service.replace(service.clone());
        let Some(service) = service else {
            self.list.end_opening();
            self.list.set_rows(&[Row::status(UNAVAILABLE, "")]);
            return;
        };
        self.list.start_opening();
        self.list.set_rows(&[]);
        let panel = self.clone();
        let subscription = service.follow(FOLLOWED, move |signal| panel.on_signal(run, signal));
        self.subscription.replace(Some(subscription));
        self.asking_opening.set(true);
        self.read_all(run);
        self.asking_opening.set(false);
    }

    /// Asks `call` and, once it answers, lets `on_answer` take the answer
    /// unless a later question about `what` was answered already. Each
    /// question waits on its own. The answer may predate signals that came
    /// while it was on its way: `catch_up` applies each of them to it
    /// first, since what they announced is newer.
    fn ask<T: 'static>(
        self: &Rc<Self>,
        run: u64,
        what: String,
        call: impl Future<Output = Result<T, glib::Error>> + 'static,
        catch_up: impl Fn(&mut T, &Signal) + 'static,
        on_answer: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let number = self.answers.ask();
        let mark = self.journal.mark();
        let opening = self.asking_opening.get();
        if opening {
            self.opening.set(self.opening.get() + 1);
        }
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = call.await;
            if !panel.runs.is_current(run) {
                return;
            }
            if let Ok(mut answer) = reply {
                let current = panel
                    .journal
                    .replay_since(mark, |signal| catch_up(&mut answer, signal));
                if current && panel.answers.take(&what, number) {
                    // What an opening answer leads to reading is part of
                    // the opening.
                    let outer = panel.asking_opening.replace(opening);
                    on_answer(&panel, answer);
                    panel.asking_opening.set(outer);
                    panel.render();
                }
            }
            if opening {
                panel.opening.set(panel.opening.get() - 1);
                // Every opening read has answered: from now on the
                // highlight follows its row.
                if panel.opening.get() == 0 {
                    panel.list.end_opening();
                }
            }
        });
    }

    fn current_service(&self, run: u64) -> Option<Service> {
        if !self.runs.is_current(run) {
            return None;
        }
        self.service.borrow().clone()
    }

    /// Reads everything the panel lists, every object on its own.
    fn read_all(self: &Rc<Self>, run: u64) {
        let Some(service) = self.current_service(run) else {
            return;
        };
        let reader = service.clone();
        self.ask(
            run,
            "manager".to_owned(),
            async move { reader.properties(MANAGER, MANAGER_IFACE).await },
            |properties, signal| {
                apply_change(properties, signal, MANAGER, MANAGER_IFACE);
            },
            |panel, properties| panel.cache.borrow_mut().manager = Some(properties),
        );
        let reader = service.clone();
        self.ask(
            run,
            "devices".to_owned(),
            async move {
                reader
                    .read(MANAGER, MANAGER_IFACE, "GetDevices", None, "(ao)")
                    .await
                    .map(|reply| super::strings(&reply.child_value(0)))
            },
            |devices, signal| {
                if let Some(change) = DevicesChange::of(signal) {
                    change.apply(devices);
                }
            },
            move |panel, devices| {
                {
                    let mut cache = panel.cache.borrow_mut();
                    cache.devices = Some(devices.clone());
                    cache.devices_complete = true;
                }
                for path in devices {
                    panel.read_device(run, path);
                }
            },
        );
        self.ask(
            run,
            "saved".to_owned(),
            async move {
                service
                    .read(SETTINGS, SETTINGS_IFACE, "ListConnections", None, "(ao)")
                    .await
                    .map(|reply| super::strings(&reply.child_value(0)))
            },
            |saved, signal| {
                if let Some(change) = SavedChange::of(signal) {
                    change.apply(saved);
                }
            },
            move |panel, mut saved| {
                {
                    let mut cache = panel.cache.borrow_mut();
                    // A connection known here that the answer leaves out
                    // stays until it is announced gone or its settings can
                    // no longer be read: the answer may be older than what
                    // made it known.
                    for known in cache.saved.iter().flatten() {
                        if !saved.contains(known) {
                            saved.push(known.clone());
                        }
                    }
                    cache.saved = Some(saved.clone());
                }
                for path in saved {
                    panel.read_settings(run, path);
                }
            },
        );
    }

    fn read_object(
        self: &Rc<Self>,
        run: u64,
        path: String,
        interface: &'static str,
        then: impl FnOnce(&Rc<Self>, &Properties) + 'static,
    ) {
        let Some(service) = self.current_service(run) else {
            return;
        };
        let what = format!("{path} {interface}");
        let (object, announced) = (path.clone(), path.clone());
        self.ask(
            run,
            what,
            async move { service.properties(&object, interface).await },
            move |properties, signal| {
                apply_change(properties, signal, &announced, interface);
            },
            move |panel, properties| {
                panel
                    .cache
                    .borrow_mut()
                    .read(path, interface, properties.clone());
                then(panel, &properties);
            },
        );
    }

    /// A device, and once it shows to be a Wi-Fi device: the scan, its
    /// access points and its active connection.
    fn read_device(self: &Rc<Self>, run: u64, path: String) {
        let device = path.clone();
        self.read_object(run, path, DEVICE_IFACE, move |panel, properties| {
            panel.device_known(run, device, properties);
        });
    }

    /// What a device's properties call for: for a Wi-Fi device, the scan,
    /// its access points and its active connection.
    fn device_known(self: &Rc<Self>, run: u64, path: String, properties: &Properties) {
        if properties.number("DeviceType") != DEVICE_TYPE_WIFI {
            return;
        }
        self.scan(run, &path);
        self.read_wireless(run, path);
        self.read_active(run, properties.string("ActiveConnection"));
    }

    /// Reads the access points named that are not known yet.
    fn read_access_points(self: &Rc<Self>, run: u64, access_points: Vec<String>) {
        for access_point in access_points {
            let known = self
                .cache
                .borrow()
                .object(&access_point, ACCESS_POINT_IFACE)
                .is_some();
            if !known {
                self.read_object(run, access_point, ACCESS_POINT_IFACE, |_, _| {});
            }
        }
    }

    fn read_wireless(self: &Rc<Self>, run: u64, path: String) {
        self.read_object(run, path, WIRELESS_IFACE, move |panel, properties| {
            for access_point in properties.paths("AccessPoints") {
                panel.read_object(run, access_point, ACCESS_POINT_IFACE, |_, _| {});
            }
        });
    }

    fn read_active(self: &Rc<Self>, run: u64, path: String) {
        if path.is_empty() || path == "/" {
            return;
        }
        self.read_object(run, path, ACTIVE_IFACE, |_, _| {});
    }

    /// A saved connection's SSID. A connection whose settings cannot be
    /// read, or that is announced gone meanwhile, is no longer saved.
    fn read_settings(self: &Rc<Self>, run: u64, path: String) {
        let Some(service) = self.current_service(run) else {
            return;
        };
        let (connection, announced) = (path.clone(), path.clone());
        self.ask(
            run,
            format!("{path} settings"),
            async move {
                let reply = service
                    .read(
                        &connection,
                        CONNECTION_IFACE,
                        "GetSettings",
                        None,
                        "(a{sa{sv}})",
                    )
                    .await;
                Ok::<_, glib::Error>(match reply {
                    Ok(reply) => SavedSettings {
                        exists: true,
                        ssid: super::lookup(&reply.child_value(0), "802-11-wireless")
                            .and_then(|wireless| super::lookup(&wireless, "ssid"))
                            .and_then(|ssid| ssid.fixed_array::<u8>().ok().map(<[u8]>::to_vec)),
                    },
                    Err(_) => SavedSettings {
                        exists: false,
                        ssid: None,
                    },
                })
            },
            move |answer, signal| {
                if SavedChange::of(signal) == Some(SavedChange::Removed(announced.clone())) {
                    answer.exists = false;
                }
            },
            move |panel, answer| {
                let mut cache = panel.cache.borrow_mut();
                if answer.exists {
                    cache.settings.insert(path, answer.ssid);
                } else {
                    if let Some(saved) = cache.saved.as_mut() {
                        SavedChange::Removed(path.clone()).apply(saved);
                    }
                    cache.settings.remove(&path);
                }
            },
        );
    }

    /// Opening the panel asks every Wi-Fi device for a scan, once, as soon
    /// as it is known to be one.
    fn scan(self: &Rc<Self>, run: u64, path: &str) {
        let Some(service) = self.current_service(run) else {
            return;
        };
        if !self.scanned.borrow_mut().insert(path.to_owned()) {
            return;
        }
        let options: HashMap<String, glib::Variant> = HashMap::new();
        let path = path.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let _ = service
                .read(
                    &path,
                    WIRELESS_IFACE,
                    "RequestScan",
                    Some(&(options,).to_variant()),
                    "()",
                )
                .await;
        });
    }

    /// Applies what a signal announces right away, reads what it newly
    /// refers to, and reads everything again soon when the signal left
    /// values out.
    fn on_signal(self: &Rc<Self>, run: u64, signal: &Signal) {
        if !self.runs.is_current(run) {
            return;
        }
        self.journal.record(signal);
        if let Some(change) = DevicesChange::of(signal) {
            self.devices_changed(run, &change);
        }
        if let Some(change) = SavedChange::of(signal) {
            self.saved_changed(run, change);
        }
        if let Some((interface, changes, invalidated)) = signal.property_changes() {
            let applied = {
                let mut cache = self.cache.borrow_mut();
                if signal.path == MANAGER && interface == MANAGER_IFACE {
                    match cache.manager.as_mut() {
                        Some(manager) => manager.merge(&changes, &invalidated),
                        None if changes.has("WirelessEnabled") => {
                            cache.manager = Some(changes.clone())
                        }
                        None => {}
                    }
                    Applied::Merged
                } else {
                    cache.apply_change(signal.path, &interface, &changes, &invalidated)
                }
            };
            match applied {
                Applied::Nothing => {}
                Applied::Merged => {
                    if interface == DEVICE_IFACE && changes.has("ActiveConnection") {
                        self.read_active(run, changes.string("ActiveConnection"));
                    }
                    if interface == WIRELESS_IFACE {
                        self.read_access_points(run, changes.paths("AccessPoints"));
                    }
                    self.render();
                }
                Applied::Completed(properties) => {
                    // What reading the interface would have led to, without
                    // waiting for that reading.
                    let listed = self
                        .cache
                        .borrow()
                        .devices
                        .iter()
                        .flatten()
                        .any(|device| device == signal.path);
                    match interface.as_str() {
                        DEVICE_IFACE if listed => {
                            self.device_known(run, signal.path.to_owned(), &properties)
                        }
                        WIRELESS_IFACE => {
                            self.read_access_points(run, properties.paths("AccessPoints"))
                        }
                        _ => {}
                    }
                    self.render();
                }
            }
        }
        // Signals carry the new values, applied above. Everything is read
        // again only after one that leaves values out: an access point
        // arriving or leaving, a saved connection whose settings changed, or
        // properties named without their values. NetworkManager sends a
        // stream of signals on objects the panel never lists (IP
        // configurations, other devices) and on every scan.
        let leaves_out = (signal.interface == WIRELESS_IFACE
            && matches!(signal.member, "AccessPointAdded" | "AccessPointRemoved"))
            || (signal.interface == CONNECTION_IFACE && signal.member == "Updated")
            || signal
                .property_changes()
                .is_some_and(|(interface, _, invalidated)| {
                    !invalidated.is_empty() && FOLLOWED.contains(&interface.as_str())
                });
        if leaves_out {
            let panel = self.clone();
            // Due for whoever owns the name by then.
            self.soon
                .request(move || panel.read_all(panel.runs.current()));
        }
    }

    /// Takes the devices the manager announces right away, so that they
    /// are read even while `GetDevices` is unanswered.
    fn devices_changed(self: &Rc<Self>, run: u64, change: &DevicesChange) {
        let unread: Vec<String> = {
            let mut cache = self.cache.borrow_mut();
            let mut devices = cache.devices.take().unwrap_or_default();
            let before = devices.clone();
            change.apply(&mut devices);
            let unread = devices
                .iter()
                .filter(|path| !before.contains(path) || cache.object(path, DEVICE_IFACE).is_none())
                .cloned()
                .collect();
            cache.devices = Some(devices);
            unread
        };
        for path in unread {
            self.read_device(run, path);
        }
        self.render();
    }

    /// Takes a saved connection's arrival or removal right away, even while
    /// `ListConnections` is unanswered: a new one has its settings read on
    /// their own, a removed one is forgotten.
    fn saved_changed(self: &Rc<Self>, run: u64, change: SavedChange) {
        {
            let mut cache = self.cache.borrow_mut();
            change.apply(cache.saved.get_or_insert_with(Vec::new));
            if let SavedChange::Removed(connection) = &change {
                cache.settings.remove(connection);
            }
        }
        if let SavedChange::Added(connection) = change {
            self.read_settings(run, connection);
        }
        self.render();
    }

    /// Lists what the answers so far say.
    fn render(&self) {
        let Some(snapshot) = self.cache.borrow().snapshot() else {
            return;
        };
        let (rows, networks) = listing(&snapshot);
        self.radio_off.set(
            rows.first()
                .is_some_and(|row| row.status && row.name == RADIO_OFF),
        );
        self.networks.replace(networks);
        self.list.set_rows(&rows);
    }

    /// Enter, or a click on a row: accepts the passphrase while the prompt
    /// shows, and otherwise acts on the highlighted row.
    fn activate(self: &Rc<Self>) {
        match self.list.prompt_key() {
            Some(key) => self.accept_passphrase(&key),
            None => self.activate_selected(),
        }
    }

    fn activate_selected(self: &Rc<Self>) {
        let Some((key, name)) = self.list.selected() else {
            return;
        };
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        if self.radio_off.get() {
            if !self.busy.start() {
                return;
            }
            let panel = self.clone();
            glib::MainContext::default().spawn_local(async move {
                // Stays open: the networks show up as the service lists them.
                let reply = service
                    .set_property(MANAGER, MANAGER_IFACE, "WirelessEnabled", true.to_variant())
                    .await;
                panel.busy.end();
                if let Err(message) = reply {
                    report_failure(&name, &message);
                }
            });
            return;
        }
        let Some(network) = self.network(&key) else {
            return;
        };
        let call = match &network.state {
            State::Connected(active) | State::Connecting(active) => {
                object_paths(&[active]).map(|paths| ("DeactivateConnection", paths))
            }
            State::Saved(connection) => {
                object_paths(&[connection, &network.device, &network.access_point])
                    .map(|paths| ("ActivateConnection", paths))
            }
            State::Open => add_and_activate(&network, None),
            State::Enterprise => None,
            State::Secured if network.is_wep() => {
                report_failure(&name, "WEP is not supported");
                None
            }
            State::Secured => {
                self.list.show_prompt(&key);
                None
            }
        };
        if let Some((method, parameters)) = call {
            self.run(name, method, parameters, None);
        }
    }

    fn network(&self, key: &str) -> Option<Network> {
        self.networks
            .borrow()
            .iter()
            .find(|network| network.key() == key)
            .cloned()
    }

    /// Calls `method` for the network `name` and exits once it succeeds.
    /// A failure report never repeats `secret`.
    fn run(
        self: &Rc<Self>,
        name: String,
        method: &'static str,
        parameters: glib::Variant,
        secret: Option<String>,
    ) {
        let Some(service) = self.service.borrow().clone() else {
            return;
        };
        if !self.busy.start() {
            return;
        }
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let reply = service
                .act(MANAGER, MANAGER_IFACE, method, Some(parameters))
                .await;
            panel.busy.end();
            match (reply, secret) {
                (Ok(_), _) => (panel.quit)(),
                (Err(message), Some(secret)) => report_failure_hiding(&name, &message, &secret),
                (Err(message), None) => report_failure(&name, &message),
            }
        });
    }

    /// Enter in the passphrase prompt: connects with an acceptable
    /// passphrase, and otherwise keeps the prompt.
    fn accept_passphrase(self: &Rc<Self>, key: &str) {
        let passphrase = self.list.entry().text();
        if !is_valid_passphrase(passphrase.as_str()) {
            return;
        }
        let Some(network) = self.network(key) else {
            return;
        };
        if let Some((method, parameters)) = add_and_activate(&network, Some(passphrase.as_str())) {
            self.run(
                network.name.clone(),
                method,
                parameters,
                Some(passphrase.to_string()),
            );
        }
    }
}

fn add_and_activate(
    network: &Network,
    passphrase: Option<&str>,
) -> Option<(&'static str, glib::Variant)> {
    let device = object_path(&network.device)?;
    let access_point = object_path(&network.access_point)?;
    Some((
        "AddAndActivateConnection",
        glib::Variant::tuple_from_iter([new_connection(network, passphrase), device, access_point]),
    ))
}

fn object_path(path: &str) -> Option<glib::Variant> {
    glib::variant::ObjectPath::try_from(path)
        .ok()
        .map(|path| path.to_variant())
}

/// The call parameters `(o…)` for object paths.
fn object_paths(paths: &[&String]) -> Option<glib::Variant> {
    let paths: Vec<glib::Variant> = paths
        .iter()
        .map(|path| object_path(path))
        .collect::<Option<_>>()?;
    Some(glib::Variant::tuple_from_iter(paths))
}

impl DialogPage for WifiPage {
    fn get_root(&self) -> gtk::Widget {
        self.panel.list.root()
    }
    fn on_entry_changed(&self) {
        self.panel.list.filter_text_changed();
    }
    fn get_id(&self) -> &'static str {
        "wifi"
    }
    fn get_name(&self) -> &'static str {
        "<u>W</u>i-Fi"
    }
    fn get_icon_name(&self) -> &'static str {
        "network-wireless-symbolic"
    }
    fn get_help(&self) -> &'static str {
        HELP
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Connect, Disconnect or Switch Wi-Fi On")]
            .into_iter()
            .collect()
    }
    fn capture_key_event(&self, event: &gdk::EventKey) -> bool {
        // Escape leaves the passphrase prompt, not the launcher.
        let list = &self.panel.list;
        if event.keyval() == gdk::keys::constants::Escape && list.prompt_key().is_some() {
            list.close_prompt();
            return true;
        }
        false
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_activation_key(event) {
            self.panel.activate();
            return true;
        }
        self.panel.list.navigate(event)
    }
    fn get_keybinding(&self) -> Option<char> {
        Some('w')
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

pub const HELP: &str = "This dialog lists the wireless networks in range. Enter connects to the highlighted network, asking for its passphrase when it needs one, or disconnects from it when it is connected. When Wi-Fi is off, Enter switches it on.";

#[cfg(test)]
mod tests {
    use super::super::lookup;
    use super::*;

    fn ap(ssid: &[u8], strength: u8, flags: u32, wpa: u32, rsn: u32) -> AccessPoint {
        AccessPoint {
            ssid: ssid.to_vec(),
            strength,
            flags,
            wpa_flags: wpa,
            rsn_flags: rsn,
        }
    }

    fn snapshot(devices: &[(&str, &[(&str, AccessPoint)])]) -> Snapshot {
        let mut snapshot = Snapshot {
            wireless_enabled: true,
            ..Snapshot::default()
        };
        for (device, access_points) in devices {
            snapshot.devices.push(Device {
                path: device.to_string(),
                is_wifi: true,
                access_points: access_points
                    .iter()
                    .map(|(path, _)| path.to_string())
                    .collect(),
                active_connection: "/".to_owned(),
            });
            for (path, access_point) in access_points.iter() {
                snapshot
                    .access_points
                    .insert(path.to_string(), access_point.clone());
            }
        }
        snapshot
    }

    fn cells(snapshot: &Snapshot) -> Vec<(String, String, String)> {
        listing(snapshot)
            .0
            .into_iter()
            .map(|row| (row.name, row.detail, row.state))
            .collect()
    }

    fn cell(name: &str, detail: &str, state: &str) -> (String, String, String) {
        (name.to_owned(), detail.to_owned(), state.to_owned())
    }

    #[test]
    fn security_follows_the_flag_words() {
        assert_eq!(security(&ap(b"x", 1, 0, 0, 0)), State::Open);
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0)), State::Secured);
        assert_eq!(security(&ap(b"x", 1, 0, 0x100, 0)), State::Secured);
        assert_eq!(security(&ap(b"x", 1, 0, 0, 0x200)), State::Enterprise);
        assert_eq!(security(&ap(b"x", 1, 1, 0x200, 0x400)), State::Secured);
        assert_eq!(security(&ap(b"x", 1, 1, 0x300, 0)), State::Secured);
        // OWE, also in transition mode, is open to join.
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0x800)), State::Open);
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0x1800)), State::Open);
        assert_eq!(security(&ap(b"x", 1, 1, 0x200, 0xa00)), State::Open);
        // The open network of a transition pair carries only the
        // transition bit.
        assert_eq!(security(&ap(b"x", 1, 0, 0, 0x1000)), State::Open);
        // WPA3-Enterprise 192-bit.
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0x2000)), State::Enterprise);
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0x2200)), State::Enterprise);
        assert_eq!(security(&ap(b"x", 1, 1, 0, 0x2400)), State::Secured);
    }

    #[test]
    fn wep_and_owe_networks_are_told_apart() {
        let listed = |flags, wpa, rsn| {
            networks(&snapshot(&[(
                "/d/a",
                &[("/ap/1", ap(b"x", 1, flags, wpa, rsn))],
            )]))
            .remove(0)
        };
        assert!(listed(1, 0, 0).is_wep());
        assert!(!listed(0, 0, 0).is_wep());
        assert!(!listed(1, 0x100, 0).is_wep());
        assert!(!listed(1, 0, 0x800).is_wep());
        let owe = listed(1, 0, 0x800);
        assert!(owe.is_owe());
        let settings = new_connection(&owe, None);
        let security = lookup(&settings, "802-11-wireless-security").unwrap();
        assert_eq!(lookup(&security, "key-mgmt").unwrap().str(), Some("owe"));
        assert!(lookup(&security, "psk").is_none());
        let transition = listed(0, 0, 0x1000);
        assert!(transition.is_owe() && !transition.is_wep());
        let settings = new_connection(&transition, None);
        let security = lookup(&settings, "802-11-wireless-security").unwrap();
        assert_eq!(lookup(&security, "key-mgmt").unwrap().str(), Some("owe"));
    }

    #[test]
    fn one_row_per_ssid_from_its_strongest_access_point() {
        let snapshot = snapshot(&[
            (
                "/d/a",
                &[
                    ("/ap/1", ap(b"Couple", 80, 0, 0, 0)),
                    ("/ap/2", ap(b"Couple", 80, 0, 0x200, 0)),
                    ("/ap/3", ap(b"", 99, 0, 0, 0)),
                    ("/ap/4", ap(b"\xff\x41", 10, 0, 0, 0)),
                    ("/ap/5", ap(b"delta", 80, 0, 0, 0)),
                ],
            ),
            (
                "/d/b",
                &[
                    ("/ap/6", ap(b"Couple", 80, 0, 0x100, 0)),
                    ("/ap/7", ap(b"\xfe\x41", 20, 0, 0, 0)),
                    ("/ap/8", ap(b"Echo", 80, 0, 0, 0)),
                ],
            ),
        ]);
        assert_eq!(
            cells(&snapshot),
            [
                cell("Couple", "80 %", "Open"),
                cell("Echo", "80 %", "Open"),
                cell("delta", "80 %", "Open"),
                cell("\u{fffd}A", "20 %", "Open"),
                cell("\u{fffd}A", "10 %", "Open"),
            ]
        );
        let couple = &networks(&snapshot)[0];
        assert_eq!(
            (couple.device.as_str(), couple.access_point.as_str()),
            ("/d/a", "/ap/1")
        );
    }

    #[test]
    fn active_and_saved_states_win_over_security_by_rule() {
        let mut snapshot = snapshot(&[(
            "/d/a",
            &[
                ("/ap/n1", ap(b"North", 84, 0, 0x100, 0)),
                ("/ap/n2", ap(b"North", 47, 0, 0x100, 0)),
                ("/ap/q", ap(b"Quarry", 90, 0, 0x100, 0)),
            ],
        )]);
        snapshot.saved.push(("/s/0".to_owned(), b"North".to_vec()));
        snapshot.saved.push(("/s/1".to_owned(), b"Quarry".to_vec()));
        snapshot.saved.push(("/s/2".to_owned(), b"Quarry".to_vec()));
        assert_eq!(
            cells(&snapshot),
            [
                cell("Quarry", "90 %", "Saved"),
                cell("North", "84 %", "Saved")
            ]
        );
        snapshot.devices[0].active_connection = "/active/0".to_owned();
        snapshot
            .active
            .insert("/active/0".to_owned(), ("/ap/n2".to_owned(), 1));
        assert_eq!(
            cells(&snapshot),
            [
                cell("North", "84 %", "Connecting"),
                cell("Quarry", "90 %", "Saved")
            ]
        );
        snapshot
            .active
            .insert("/active/0".to_owned(), ("/ap/n2".to_owned(), 2));
        let listed = networks(&snapshot);
        assert_eq!(listed[0].state, State::Connected("/active/0".to_owned()));
        assert_eq!(listed[1].state, State::Saved("/s/1".to_owned()));
        snapshot
            .active
            .insert("/active/0".to_owned(), ("/ap/n2".to_owned(), 3));
        assert_eq!(
            networks(&snapshot)[1].state,
            State::Saved("/s/0".to_owned())
        );
    }

    #[test]
    fn status_rows_take_precedence() {
        let mut snapshot = snapshot(&[("/d/a", &[("/ap/1", ap(b"x", 1, 0, 0, 0))])]);
        snapshot.wireless_enabled = false;
        assert_eq!(cells(&snapshot), [cell(RADIO_OFF, "", "Off")]);
        snapshot.devices[0].is_wifi = false;
        assert_eq!(cells(&snapshot), [cell(NO_ADAPTER, "", "")]);
    }

    #[test]
    fn passphrase_lengths_count_characters() {
        assert!(!is_valid_passphrase("1234567"));
        assert!(is_valid_passphrase("12345678"));
        assert!(is_valid_passphrase(&"z".repeat(63)));
        assert!(!is_valid_passphrase(&"z".repeat(64)));
        assert!(is_valid_passphrase(&"aF09".repeat(16)));
        assert!(!is_valid_passphrase(&"k".repeat(65)));
        assert!(is_valid_passphrase(&"é".repeat(8)));
        assert!(is_valid_passphrase(&"é".repeat(63)));
        assert!(!is_valid_passphrase(&"é".repeat(7)));
        assert!(!is_valid_passphrase(&"é".repeat(64)));
    }

    #[test]
    fn key_management_is_sae_only_without_psk() {
        let network = |wpa, rsn| Network {
            ssid: b"x".to_vec(),
            name: "x".to_owned(),
            strength: 1,
            device: "/d".to_owned(),
            access_point: "/ap".to_owned(),
            flags: 1,
            wpa_flags: wpa,
            rsn_flags: rsn,
            state: State::Secured,
        };
        assert_eq!(network(0, 0x400).key_management(), "sae");
        assert_eq!(network(0x400, 0).key_management(), "wpa-psk");
        assert_eq!(network(0x100, 0x400).key_management(), "wpa-psk");
        assert_eq!(network(0, 0x500).key_management(), "wpa-psk");
    }

    #[test]
    fn new_connections_carry_the_ssid_bytes() {
        let network = networks(&snapshot(&[(
            "/d/a",
            &[("/ap/1", ap(b"\xfeA", 1, 0, 0, 0x400))],
        )]))
        .remove(0);
        let open = new_connection(&network, None);
        assert_eq!(open.type_().as_str(), "a{sa{sv}}");
        let ssid = lookup(&lookup(&open, "802-11-wireless").unwrap(), "ssid").unwrap();
        assert_eq!(ssid.fixed_array::<u8>().unwrap(), b"\xfeA");
        assert!(lookup(&open, "802-11-wireless-security").is_none());
        let secured = new_connection(&network, Some(" pass phrase "));
        let security = lookup(&secured, "802-11-wireless-security").unwrap();
        assert_eq!(lookup(&security, "key-mgmt").unwrap().str(), Some("sae"));
        assert_eq!(
            lookup(&security, "psk").unwrap().str(),
            Some(" pass phrase ")
        );
    }

    #[test]
    fn announced_devices_update_the_list() {
        let mut devices = vec!["/d/a".to_owned()];
        DevicesChange::Added("/d/b".to_owned()).apply(&mut devices);
        DevicesChange::Added("/d/a".to_owned()).apply(&mut devices);
        assert_eq!(devices, ["/d/a", "/d/b"]);
        DevicesChange::Removed("/d/a".to_owned()).apply(&mut devices);
        assert_eq!(devices, ["/d/b"]);
        DevicesChange::All(vec!["/d/c".to_owned(), "/d/b".to_owned()]).apply(&mut devices);
        assert_eq!(devices, ["/d/c", "/d/b"]);
    }

    #[test]
    fn saved_connection_announcements_are_recognised_and_applied() {
        let added = (glib::variant::ObjectPath::try_from("/s/2").unwrap(),).to_variant();
        let signal = |path, interface, member, parameters| Signal {
            path,
            interface,
            member,
            parameters,
        };
        let new = SavedChange::of(&signal(SETTINGS, SETTINGS_IFACE, "NewConnection", &added));
        assert_eq!(new, Some(SavedChange::Added("/s/2".to_owned())));
        let gone = SavedChange::of(&signal(
            SETTINGS,
            SETTINGS_IFACE,
            "ConnectionRemoved",
            &added,
        ));
        assert_eq!(gone, Some(SavedChange::Removed("/s/2".to_owned())));
        let nothing = ().to_variant();
        let own = SavedChange::of(&signal("/s/1", CONNECTION_IFACE, "Removed", &nothing));
        assert_eq!(own, Some(SavedChange::Removed("/s/1".to_owned())));
        assert_eq!(
            SavedChange::of(&signal(MANAGER, MANAGER_IFACE, "NewConnection", &added)),
            None
        );
        let mut saved = vec!["/s/0".to_owned(), "/s/1".to_owned()];
        new.unwrap().apply(&mut saved);
        SavedChange::Added("/s/0".to_owned()).apply(&mut saved);
        assert_eq!(saved, ["/s/0", "/s/1", "/s/2"]);
        own.unwrap().apply(&mut saved);
        gone.unwrap().apply(&mut saved);
        assert_eq!(saved, ["/s/0"]);
    }

    #[test]
    fn announced_devices_alone_never_claim_no_adapter() {
        let mut cache = Cache {
            devices: Some(vec!["/d/e".to_owned()]),
            ..Cache::default()
        };
        let mut wired = Properties::default();
        wired.0.insert("DeviceType".to_owned(), 1u32.to_variant());
        cache
            .objects
            .insert(("/d/e".to_owned(), DEVICE_IFACE.to_owned()), wired);
        assert!(cache.snapshot().is_none());
        cache.devices_complete = true;
        assert!(cache.snapshot().is_some());
    }

    #[test]
    fn announced_devices_alone_never_claim_radio_off() {
        let mut cache = Cache {
            devices: Some(vec!["/d/w".to_owned()]),
            ..Cache::default()
        };
        let mut device = Properties::default();
        device
            .0
            .insert("DeviceType".to_owned(), DEVICE_TYPE_WIFI.to_variant());
        cache
            .objects
            .insert(("/d/w".to_owned(), DEVICE_IFACE.to_owned()), device);
        let mut wireless = Properties::default();
        wireless
            .0
            .insert("AccessPoints".to_owned(), vec!["/a/1"].to_variant());
        cache
            .objects
            .insert(("/d/w".to_owned(), WIRELESS_IFACE.to_owned()), wireless);
        let mut ap = Properties::default();
        ap.0.insert(
            "Ssid".to_owned(),
            glib::Variant::array_from_fixed_array(b"Early"),
        );
        ap.0.insert("Strength".to_owned(), 79u8.to_variant());
        cache
            .objects
            .insert(("/a/1".to_owned(), ACCESS_POINT_IFACE.to_owned()), ap);
        // The manager has not answered yet: the network is listed anyway.
        let early = cache.snapshot().unwrap();
        assert_eq!(cells(&early), [cell("Early", "79 %", "Open")]);
        let mut manager = Properties::default();
        manager
            .0
            .insert("WirelessEnabled".to_owned(), false.to_variant());
        cache.manager = Some(manager);
        let partial = cache.snapshot().unwrap();
        assert_eq!(cells(&partial), [cell("Early", "79 %", "Open")]);
        cache.devices_complete = true;
        let whole = cache.snapshot().unwrap();
        assert_eq!(cells(&whole), [cell(RADIO_OFF, "", "Off")]);
    }

    #[test]
    fn announced_properties_stand_in_for_an_unanswered_read() {
        let mut cache = Cache {
            devices: Some(vec!["/d/w".to_owned()]),
            ..Cache::default()
        };
        let properties = |entries: &[(&str, glib::Variant)]| {
            Properties(
                entries
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
            )
        };
        // Some of a Wi-Fi device's properties: not enough to list it.
        let some = properties(&[("DeviceType", DEVICE_TYPE_WIFI.to_variant())]);
        assert!(matches!(
            cache.apply_change("/d/w", DEVICE_IFACE, &some, &[]),
            Applied::Nothing
        ));
        assert!(cache.object("/d/w", DEVICE_IFACE).is_none());
        assert!(cache.snapshot().is_none());
        let rest = properties(&[("ActiveConnection", "/".to_variant())]);
        let Applied::Completed(device) = cache.apply_change("/d/w", DEVICE_IFACE, &rest, &[])
        else {
            panic!("the device is complete");
        };
        assert_eq!(device.number("DeviceType"), DEVICE_TYPE_WIFI);
        let wireless = properties(&[("AccessPoints", vec!["/a/1"].to_variant())]);
        assert!(matches!(
            cache.apply_change("/d/w", WIRELESS_IFACE, &wireless, &[]),
            Applied::Completed(_)
        ));
        // An invalidated property leaves it incomplete again.
        let ap = properties(&[
            ("Ssid", glib::Variant::array_from_fixed_array(b"Payload")),
            ("Strength", 79u8.to_variant()),
            ("Flags", 0u32.to_variant()),
            ("WpaFlags", 0u32.to_variant()),
            ("RsnFlags", 0u32.to_variant()),
        ]);
        let none = Properties::default();
        cache.apply_change("/a/1", ACCESS_POINT_IFACE, &ap, &["Flags".to_owned()]);
        assert!(cache.object("/a/1", ACCESS_POINT_IFACE).is_none());
        assert!(matches!(
            cache.apply_change("/a/1", ACCESS_POINT_IFACE, &ap, &[]),
            Applied::Completed(_)
        ));
        assert_eq!(
            cells(&cache.snapshot().unwrap()),
            [cell("Payload", "79 %", "Open")]
        );
        // Once known, changes merge; an answer replaces what was announced.
        assert!(matches!(
            cache.apply_change("/a/1", ACCESS_POINT_IFACE, &none, &[]),
            Applied::Merged
        ));
        cache.apply_change("/a/2", ACCESS_POINT_IFACE, &none, &[]);
        cache.read("/a/2".to_owned(), ACCESS_POINT_IFACE, ap.clone());
        assert!(cache.partial.is_empty());
        // Interfaces the panel takes nothing from are never collected.
        assert!(matches!(
            cache.apply_change("/d/w", "org.example.Other", &ap, &[]),
            Applied::Nothing
        ));
        assert!(cache.partial.is_empty());
    }
}
