//! The window manager's and the desktop's keys while ilia is open.
//!
//! On X11 ilia holds a keyboard grab, so a key press reaches neither i3's
//! bindings nor the programs that grab keys on the root window (the
//! desktop's Print, volume, brightness, calculator and playback keys).
//! [`X11KeyForwarder`] takes the presses ilia does not use itself:
//!
//! - when i3 would run a binding for the press in its current binding mode,
//!   ilia lets go of the keyboard, has i3 run that binding's command over IPC
//!   exactly as the configuration holds it, and closes. A `--release` binding
//!   runs when the key is released;
//! - otherwise one of the desktop's keys closes ilia at once, held or not:
//!   ilia lets go of the keyboard and presses the key again through XTEST,
//!   with the modifiers of the original press, so the program that grabs it
//!   gets it.
//!
//! sway runs its bindings while ilia has the keyboard focus:
//! [`watch_sway_bindings`] closes ilia when one runs, unless all it does is
//! switch the keyboard layout.
//!
//! A window-manager socket that cannot be used (missing, refusing, or never
//! answering) is reported once, and then left alone for the rest of the run.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk;

use crate::ipc::{self, Config, Connection};
use crate::wm_bindings::{self, Binding, Press, EVENT_STATE_MASK};
use crate::x11::X11Display;

/// How long a binding lookup waits for i3: a key press is handled while it
/// waits.
const LOOKUP_TIMEOUT: Duration = Duration::from_millis(750);
/// How long the subscription to sway's bindings waits for sway to accept it.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The window-manager socket the session names. Once it fails it is not
/// used again, and the failure is reported once for the whole run.
pub struct WmSocket {
    path: OsString,
    failed: Cell<bool>,
}

impl WmSocket {
    /// The socket for `wm_name`, or `None` when the session names none.
    pub fn from_session(wm_name: &str) -> Option<Rc<Self>> {
        let path = ipc::wm_socket(wm_name).ok()?;
        Some(Rc::new(Self {
            path,
            failed: Cell::new(false),
        }))
    }

    fn usable(&self) -> bool {
        !self.failed.get()
    }

    fn fail(&self, error: &str) {
        if !self.failed.replace(true) {
            eprintln!(
                "Unable to use the window manager socket {}: {error}",
                std::path::Path::new(&self.path).display()
            );
        }
    }

    /// Finds out in the background whether the socket answers, so a socket
    /// that accepts connections but never answers is known before the first
    /// key press has to wait for it.
    fn probe(self: &Rc<Self>) {
        let path = self.path.clone();
        #[allow(deprecated)]
        let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let spawned = std::thread::Builder::new()
            .name("ilia-wm-probe".into())
            .spawn(move || {
                let result = Connection::open(&path, LOOKUP_TIMEOUT)
                    .and_then(|mut connection| connection.request(ipc::GET_BINDING_STATE, ""))
                    .and_then(|reply| ipc::binding_state_from_reply(&reply));
                let _ = sender.send(result.err());
            });
        if spawned.is_err() {
            return;
        }
        let socket = self.clone();
        receiver.attach(None, move |error: Option<String>| {
            if let Some(error) = error {
                socket.fail(&error);
            }
            glib::ControlFlow::Break
        });
    }
}

/// i3's bindings as they are now: its binding mode, and its configuration
/// read again whenever it changed.
struct I3Bindings {
    socket: Rc<WmSocket>,
    cache: RefCell<Option<ParsedConfig>>,
}

/// The bindings read from i3's configuration files, kept until the files
/// change.
struct ParsedConfig {
    files: Vec<String>,
    bindings: Rc<Vec<Binding>>,
}

impl I3Bindings {
    /// The current binding mode and the bindings, or `None` when i3 cannot
    /// be asked.
    fn current(&self) -> Option<(String, Rc<Vec<Binding>>)> {
        if !self.socket.usable() {
            return None;
        }
        match self.read() {
            Ok(current) => Some(current),
            Err(error) => {
                self.socket.fail(&error);
                None
            }
        }
    }

    fn read(&self) -> Result<(String, Rc<Vec<Binding>>), String> {
        let mut connection = Connection::open(&self.socket.path, LOOKUP_TIMEOUT)?;
        let mode = ipc::binding_state_from_reply(&connection.request(ipc::GET_BINDING_STATE, "")?)?;
        let Config::I3 { files, .. } =
            ipc::config_from_reply(connection.request(ipc::GET_CONFIG, "")?, "i3", None)?
        else {
            return Err("unexpected configuration reply".to_owned());
        };
        let mut cache = self.cache.borrow_mut();
        if let Some(parsed) = cache.as_ref().filter(|parsed| parsed.files == files) {
            return Ok((mode, parsed.bindings.clone()));
        }
        let bindings = Rc::new(wm_bindings::parse_i3_config(&files));
        *cache = Some(ParsedConfig {
            files,
            bindings: bindings.clone(),
        });
        Ok((mode, bindings))
    }
}

/// A key bound by an i3 `--release` binding, waiting for its release.
struct PendingKey {
    keycode: u16,
    /// The release binding to run.
    binding: Binding,
}

/// Hands the X11 key presses ilia does not use to i3 or to the desktop.
pub struct X11KeyForwarder {
    display: X11Display,
    keymap: Option<gdk::Keymap>,
    numlock_mask: u32,
    wm_name: String,
    bindings: Option<I3Bindings>,
    pending: RefCell<Option<PendingKey>>,
    /// Gives up the keyboard and pointer and hides the window, synchronously.
    release_input: Rc<dyn Fn()>,
    /// Closes ilia.
    finished: Rc<dyn Fn()>,
}

impl X11KeyForwarder {
    /// The forwarder for an X11 display; `None` on any other. i3's bindings
    /// are followed when `wm_name` is i3 and the session names its socket;
    /// the desktop's keys are passed on in any case.
    pub fn new(
        display: &gdk::Display,
        wm_name: &str,
        release_input: Rc<dyn Fn()>,
        finished: Rc<dyn Fn()>,
    ) -> Option<Self> {
        let x11 = X11Display::new(display)?;
        let bindings = (wm_name == "i3")
            .then(|| WmSocket::from_session(wm_name))
            .flatten()
            .map(|socket| {
                socket.probe();
                I3Bindings {
                    socket,
                    cache: RefCell::new(None),
                }
            });
        Some(Self {
            numlock_mask: x11.numlock_mask(),
            display: x11,
            keymap: gdk::Keymap::for_display(display),
            wm_name: wm_name.to_owned(),
            bindings,
            pending: RefCell::new(None),
            release_input,
            finished,
        })
    }

    /// The real modifier that carries Num Lock.
    pub fn numlock_mask(&self) -> u32 {
        self.numlock_mask
    }

    /// Handles a press of a key ilia does not use itself. True when the key
    /// is taken: ilia closes now, or, for an i3 `--release` binding, when the
    /// key is released.
    pub fn key_pressed(&self, event: &gdk::EventKey) -> bool {
        let keycode = event.hardware_keycode();
        {
            let mut pending = self.pending.borrow_mut();
            if let Some(held) = pending.as_ref() {
                if held.keycode == keycode {
                    // Repeated while held.
                    return true;
                }
                // Another key: the one held no longer counts.
                *pending = None;
            }
        }
        let press = Press {
            keycode: u32::from(keycode),
            state: event.state().bits() & EVENT_STATE_MASK,
            layout: u32::from(event.group()),
        };
        let found = match (&self.bindings, &self.keymap) {
            (Some(bindings), Some(keymap)) => bindings.current().and_then(|(mode, list)| {
                wm_bindings::lookup(&list, &mode, &press, self.numlock_mask, keymap).cloned()
            }),
            _ => None,
        };
        match found {
            Some(binding) if !binding.release => {
                self.run_binding(&binding);
                true
            }
            Some(binding) => {
                *self.pending.borrow_mut() = Some(PendingKey { keycode, binding });
                true
            }
            None if is_desktop_key(*event.keyval()) || is_desktop_key(self.base_keyval(event)) => {
                // Handed on now, not on release: the key may stay held.
                self.press_again(keycode, press.state);
                true
            }
            None => false,
        }
    }

    /// Handles a key release. True when it runs a pending release binding.
    pub fn key_released(&self, event: &gdk::EventKey) -> bool {
        let keycode = event.hardware_keycode();
        let taken = {
            let mut pending = self.pending.borrow_mut();
            match pending.as_ref() {
                Some(held) if held.keycode == keycode => pending.take(),
                _ => None,
            }
        };
        let Some(taken) = taken else {
            return false;
        };
        self.run_binding(&taken.binding);
        true
    }

    /// The key's symbol in the first layout, without modifiers.
    fn base_keyval(&self, event: &gdk::EventKey) -> u32 {
        self.keymap
            .as_ref()
            .and_then(|keymap| {
                keymap.translate_keyboard_state(
                    u32::from(event.hardware_keycode()),
                    gdk::ModifierType::empty(),
                    0,
                )
            })
            .map_or(*event.keyval(), |(keyval, ..)| keyval)
    }

    /// Lets go of the keyboard first, so a program the binding starts can
    /// take it, then has i3 run the binding, then closes.
    fn run_binding(&self, binding: &Binding) {
        (self.release_input)();
        if let Err(error) = ipc::run_command(&self.wm_name, &binding.command) {
            eprintln!("Unable to run the binding of {}: {error}", self.wm_name);
        }
        (self.finished)();
    }

    /// Lets go of the keyboard, presses the key again for the program that
    /// grabs it, with the modifiers of the original press, then closes. The
    /// key may still be held: the X server then sees it go up and down again,
    /// a fresh press that the program's grab takes.
    fn press_again(&self, keycode: u16, mods: u32) {
        (self.release_input)();
        if let Err(error) = self.display.press_key(keycode, mods) {
            eprintln!("Unable to pass the key on: {error}");
        }
        (self.finished)();
    }
}

/// The desktop's keys, which ilia hands back when the window manager does
/// not bind them.
pub fn is_desktop_key(keyval: u32) -> bool {
    use gdk::keys::constants as keys;
    matches!(
        gdk::keys::Key::from(keyval),
        keys::Print
            | keys::AudioRaiseVolume
            | keys::AudioLowerVolume
            | keys::AudioMute
            | keys::AudioMicMute
            | keys::MonBrightnessUp
            | keys::MonBrightnessDown
            | keys::Calculator
            | keys::AudioPlay
            | keys::AudioPause
            | keys::AudioNext
            | keys::AudioPrev
    )
}

/// Calls `binding_run` each time sway runs a binding, except one that only
/// switches the keyboard layout. sway has run it already; ilia just gets out
/// of the way. Without a usable sway socket ilia cannot know, and stays open.
pub fn watch_sway_bindings(binding_run: Rc<dyn Fn()>) {
    let Some(socket) = WmSocket::from_session("sway") else {
        return;
    };
    let path = socket.path.clone();
    #[allow(deprecated)]
    let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let spawned = std::thread::Builder::new()
        .name("ilia-sway-bindings".into())
        .spawn(move || {
            let subscribed = Connection::open(&path, SUBSCRIBE_TIMEOUT)
                .and_then(|connection| connection.subscribe(&["binding"]));
            let mut connection = match subscribed {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            // Events until sway goes away.
            while let Ok((event_type, event)) = connection.next_event() {
                if event_type != ipc::BINDING_EVENT {
                    continue;
                }
                if let Ok(Some(command)) = ipc::command_from_binding_event(&event) {
                    if sender.send(Ok(command)).is_err() {
                        return;
                    }
                }
            }
        });
    if let Err(error) = spawned {
        socket.fail(&error.to_string());
        return;
    }
    receiver.attach(None, move |result: Result<String, String>| {
        match result {
            Ok(command) => {
                if !wm_bindings::is_layout_switch_only(&command) {
                    binding_run();
                }
            }
            Err(error) => socket.fail(&error),
        }
        glib::ControlFlow::Continue
    });
}
