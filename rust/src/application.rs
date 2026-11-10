use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gio::prelude::*;
use gtk::prelude::*;
use gtk_layer_shell::LayerShell;

use crate::args::Args;
use crate::dialog;

const DEFAULT_CSS: &str = r#"                .root_box {
                    margin: 8px;
                }

                window {
                    border-style: dotted;
                    border-width: 1px;
                }

                .filter_entry {
                    border: none;
                    background: none;
                    min-height: 36px;
                    min-width: 320px;
                }

                .notebook {
                    border: none;
                }

                .keybindings {
                    font-family: monospace;
                }
            "#;

/// A D-Bus connection kept for as long as the application runs.
type SharedConnection = Rc<RefCell<Option<gio::DBusConnection>>>;

pub fn run(args: Args) -> i32 {
    let wm_name = detect_wm(
        std::env::var_os("SWAYSOCK").is_some(),
        std::env::var_os("I3SOCK").is_some(),
    );
    let app = gtk::Application::new(
        Some("org.regolith.launcher"),
        gio::ApplicationFlags::REPLACE | gio::ApplicationFlags::ALLOW_REPLACEMENT,
    );
    let exit_status = Rc::new(Cell::new(0));
    let seat = Rc::new(RefCell::new(None::<gtk::gdk::Seat>));
    let status_for_activate = exit_status.clone();
    let seat_for_activate = seat.clone();
    // The system bus connection that watches for a suspend. Its signal
    // subscription ends when the connection is freed, so the application
    // keeps it.
    let system_bus: SharedConnection = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        let is_wayland = is_wayland_display();
        match dialog::build(app, &args, seat_for_activate.clone(), wm_name, is_wayland) {
            Ok(window) => {
                if is_wayland {
                    if !gtk_layer_shell::is_supported() {
                        eprint!("The wayland compositor does not support the layer-shell protocol, aborting.");
                        status_for_activate.set(1);
                        app.quit();
                        return;
                    }
                    window.init_layer_shell();
                    window.set_layer(gtk_layer_shell::Layer::Overlay);
                    window.set_keyboard_mode(gtk_layer_shell::KeyboardMode::Exclusive);
                }
                if !load_style(&window, &args) {
                    status_for_activate.set(1);
                    app.quit();
                    return;
                }
                close_on_sleep_or_lock(app, &window, &seat_for_activate, &system_bus);
                window.show_all();
                if !is_wayland {
                    match dialog::grab_inputs(&window) {
                        Some(grabbed) => *seat_for_activate.borrow_mut() = Some(grabbed),
                        None => {
                            eprint!("Failed to acquire access to input devices, aborting.");
                            status_for_activate.set(1);
                            app.quit();
                        }
                    }
                }
            }
            Err(()) => {
                status_for_activate.set(1);
                app.quit();
            }
        }
    });
    let seat_for_shutdown = seat.clone();
    app.connect_shutdown(move |_| {
        if let Some(seat) = seat_for_shutdown.borrow_mut().take() {
            seat.ungrab();
        }
    });
    app.run_with_args(&["ilia"]);
    exit_status.get()
}

/// Closes the dialog when the system is about to suspend or the screen gets
/// locked. On X11 ilia grabs the keyboard and pointer, and while it does the
/// screen locker cannot lock.
fn close_on_sleep_or_lock(
    app: &gtk::Application,
    window: &gtk::Window,
    seat: &dialog::SharedSeat,
    system_bus: &SharedConnection,
) {
    let (app_for_signal, window, seat) = (app.clone(), window.clone(), seat.clone());
    let on_signal = move |_: &gio::DBusConnection,
                          _: &str,
                          _: &str,
                          interface: &str,
                          signal: &str,
                          parameters: &glib::Variant| {
        if is_sleep_or_lock_signal(interface, signal, parameters) {
            dialog::quit(&window, &app_for_signal, &seat);
        }
    };

    if let Some(session_bus) = app.dbus_connection() {
        session_bus.signal_subscribe(
            Some("org.gnome.ScreenSaver"),
            Some("org.gnome.ScreenSaver"),
            Some("ActiveChanged"),
            Some("/org/gnome/ScreenSaver"),
            None,
            gio::DBusSignalFlags::NONE,
            on_signal.clone(),
        );
    }

    let system_bus = system_bus.clone();
    gio::bus_get(
        gio::BusType::System,
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok(bus) => {
                bus.signal_subscribe(
                    Some("org.freedesktop.login1"),
                    Some("org.freedesktop.login1.Manager"),
                    Some("PrepareForSleep"),
                    Some("/org/freedesktop/login1"),
                    None,
                    gio::DBusSignalFlags::NONE,
                    on_signal,
                );
                system_bus.replace(Some(bus));
            }
            Err(error) => eprintln!("Cannot watch for suspend: {}", error.message()),
        },
    );
}

/// True for the D-Bus signals that announce a suspend or a screen lock:
/// logind's `PrepareForSleep(true)` and the screensaver's
/// `ActiveChanged(true)`.
fn is_sleep_or_lock_signal(interface: &str, signal: &str, parameters: &glib::Variant) -> bool {
    let is_sleep = interface == "org.freedesktop.login1.Manager" && signal == "PrepareForSleep";
    let is_lock = interface == "org.gnome.ScreenSaver" && signal == "ActiveChanged";
    (is_sleep || is_lock)
        && parameters
            .get::<(bool,)>()
            .is_some_and(|(starting,)| starting)
}

/// Whether GDK connected to a Wayland compositor. Asking GDK rather than the
/// environment also covers sessions without `XDG_SESSION_TYPE` (sway started
/// from a TTY) and `GDK_BACKEND=x11`.
fn is_wayland_display() -> bool {
    gtk::gdk::Display::default()
        .is_some_and(|display| display.type_().name() == "GdkWaylandDisplay")
}

fn detect_wm(has_sway_socket: bool, has_i3_socket: bool) -> &'static str {
    if has_sway_socket {
        "sway"
    } else if has_i3_socket {
        "i3"
    } else {
        "Unknown"
    }
}

fn load_style(window: &gtk::Window, args: &Args) -> bool {
    let mut provider = gtk::CssProvider::new();
    if let Some(path) = args.value("-t") {
        // Resolve through GFile like the reference: a relative path is
        // reported (and loaded) as its absolute form.
        let file = gio::File::for_path(path);
        let shown = file
            .path()
            .map(|resolved| resolved.display().to_string())
            .unwrap_or_else(|| path.to_owned());
        if !file.query_exists(None::<&gio::Cancellable>) {
            eprintln!("File '{shown}' does not exist.");
            return false;
        }
        if let Err(error) = provider.load_from_file(&file) {
            // A bad rule must not keep the launcher from opening: say what
            // is wrong and fall back to the built-in style.
            eprintln!(
                "Failed to load stylesheet '{shown}', using the default style: {}",
                error.message()
            );
            provider = gtk::CssProvider::new();
            if provider.load_from_data(DEFAULT_CSS.as_bytes()).is_err() {
                return false;
            }
        }
    } else if args.has("-n") {
        return true;
    } else if provider.load_from_data(DEFAULT_CSS.as_bytes()).is_err() {
        return false;
    }
    if let Some(screen) = gtk::prelude::WidgetExt::screen(window) {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{detect_wm, is_sleep_or_lock_signal};
    use glib::prelude::ToVariant;

    #[test]
    fn suspend_and_lock_signals_close_the_dialog() {
        let starting = (true,).to_variant();
        let ending = (false,).to_variant();
        let sleep = "org.freedesktop.login1.Manager";
        let lock = "org.gnome.ScreenSaver";
        assert!(is_sleep_or_lock_signal(sleep, "PrepareForSleep", &starting));
        assert!(is_sleep_or_lock_signal(lock, "ActiveChanged", &starting));
        // Resume and unlock.
        assert!(!is_sleep_or_lock_signal(sleep, "PrepareForSleep", &ending));
        assert!(!is_sleep_or_lock_signal(lock, "ActiveChanged", &ending));
        // Other signals, and unexpected arguments.
        assert!(!is_sleep_or_lock_signal(
            sleep,
            "PrepareForShutdown",
            &starting
        ));
        assert!(!is_sleep_or_lock_signal(
            lock,
            "WakeUpScreen",
            &().to_variant()
        ));
        assert!(!is_sleep_or_lock_signal(
            sleep,
            "PrepareForSleep",
            &("true",).to_variant()
        ));
        assert!(!is_sleep_or_lock_signal(
            sleep,
            "PrepareForSleep",
            &().to_variant()
        ));
        assert!(!is_sleep_or_lock_signal(
            sleep,
            "PrepareForSleep",
            &true.to_variant()
        ));
    }

    #[test]
    fn wm_detection_prefers_sway_then_i3() {
        assert_eq!(detect_wm(true, true), "sway");
        assert_eq!(detect_wm(false, true), "i3");
        assert_eq!(detect_wm(false, false), "Unknown");
    }
}
