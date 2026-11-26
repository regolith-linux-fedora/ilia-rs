use std::cell::{Cell, RefCell};
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use gio::prelude::*;
use gtk::prelude::*;
use gtk_layer_shell::LayerShell;

use crate::apps;
use crate::args::Args;
use crate::dialog;
use crate::menu;
use crate::textlist;

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
    if opens_apps_page(&args) {
        // Start reading the desktop files now, on worker threads, so that it
        // overlaps with GTK's start-up. GTK sets the locale only while it
        // starts, so set it first: GLib translates the desktop files' Name
        // and Keywords as it reads them.
        // SAFETY: no other thread is running yet, and the argument is a valid
        // C string.
        unsafe {
            libc::setlocale(libc::LC_ALL, c"".as_ptr());
        }
        apps::prefetch_apps();
    }
    // A variable that is set but empty counts as unset.
    let has_socket = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let wm_name = detect_wm(has_socket("SWAYSOCK"), has_socket("I3SOCK"));
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
    // A terminating signal closes the launcher like any other way out, so
    // the commands its pages started end with it.
    let signalled = Rc::new(Cell::new(None::<i32>));
    if let Some(signals) = catch_terminating_signals() {
        let (app, signalled) = (app.clone(), signalled.clone());
        glib::source::unix_fd_add_local(signals, glib::IOCondition::IN, move |fd, _| {
            if let Some(signal) = read_signal(fd) {
                signalled.set(signalled.get().or(Some(signal)));
                app.quit();
            }
            glib::ControlFlow::Continue
        });
    }
    app.run_with_args(&["ilia"]);
    // Page commands still running end with the launcher; a command a
    // choice started gets a moment to finish. A signal that came while the
    // main loop was ending, or comes while the launcher waits here, counts
    // too.
    textlist::stop_commands();
    menu::stop_commands();
    if let Some(signal) = signalled.get().or_else(pending_signal) {
        end_by_signal(signal);
    }
    textlist::wait_for_launched();
    if let Some(signal) = pending_signal() {
        end_by_signal(signal);
    }
    exit_status.get()
}

/// The terminating signals the launcher catches to close cleanly. Others,
/// SIGKILL among them, end it at once; the commands its menu pages started
/// still end with it (their guardian stops them, see `menu`). SIGTRAP is
/// left alone: it is how GLib stops on a fatal error.
const TERMINATING_SIGNALS: [libc::c_int; 12] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGABRT,
    libc::SIGXCPU,
    libc::SIGXFSZ,
    libc::SIGPROF,
    libc::SIGVTALRM,
];

/// The end of the pipe the signal handler writes caught signals to, or -1.
static SIGNAL_WRITER: AtomicI32 = AtomicI32::new(-1);
/// The end of that pipe the main loop reads them from, or -1.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_terminating_signal(signal: libc::c_int) {
    let writer = SIGNAL_WRITER.load(Ordering::Relaxed);
    if writer < 0 {
        return;
    }
    // SAFETY: only async-signal-safe calls, on a live one-byte buffer;
    // errno is restored for the code the signal interrupted.
    unsafe {
        let errno = *libc::__errno_location();
        let byte = signal as u8;
        libc::write(writer, std::ptr::addr_of!(byte).cast(), 1);
        *libc::__errno_location() = errno;
    }
}

/// Catches the terminating signals that still have their default action at
/// start-up, so that each becomes a byte to read on the returned
/// descriptor; `None` when they cannot be caught. A signal ignored, or
/// handled by something loaded before the launcher (a profiler's SIGPROF,
/// say), is left as it is.
fn catch_terminating_signals() -> Option<RawFd> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a live array of the two descriptors pipe2 fills in.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return None;
    }
    SIGNAL_PIPE.store(fds[0], Ordering::Relaxed);
    SIGNAL_WRITER.store(fds[1], Ordering::Relaxed);
    for signal in TERMINATING_SIGNALS {
        // SAFETY: all-zero sigaction structures are valid; sigaction reads
        // and writes only the live structures passed, and the handler is
        // async-signal-safe.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut current) != 0
                || current.sa_sigaction != libc::SIG_DFL
            {
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_terminating_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    Some(fds[0])
}

/// The first caught signal not yet read, if any.
fn pending_signal() -> Option<i32> {
    let signals = SIGNAL_PIPE.load(Ordering::Relaxed);
    if signals < 0 {
        return None;
    }
    read_signal(signals)
}

/// The first signal waiting on the descriptor `fd`, reading all of them.
fn read_signal(fd: RawFd) -> Option<i32> {
    let mut first = None;
    let mut buffer = [0u8; 16];
    loop {
        // SAFETY: `buffer` is a live, writable buffer of the length given.
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        match usize::try_from(read) {
            Ok(read) if read > 0 => first = first.or(Some(i32::from(buffer[0]))),
            _ if read < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return first,
        }
    }
}

/// Ends the launcher as `signal` would have ended it, without a core dump.
fn end_by_signal(signal: i32) {
    // SAFETY: setrlimit reads the live structure passed; restoring the
    // default action and raising a signal touch no memory of this process.
    unsafe {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Whether the dialog shows the Applications page: always with `-a`, and
/// with `-p apps` or no `-p` at all.
fn opens_apps_page(args: &Args) -> bool {
    args.has("-a")
        || args
            .value("-p")
            .unwrap_or("apps")
            .eq_ignore_ascii_case("apps")
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
    use super::{detect_wm, is_sleep_or_lock_signal, opens_apps_page};
    use crate::args::Args;
    use glib::prelude::ToVariant;

    #[test]
    fn desktop_files_are_read_early_only_for_the_applications_page() {
        let opens = |args: &[&str]| opens_apps_page(&Args::parse(args.iter().copied()));
        assert!(opens(&["ilia"]));
        assert!(opens(&["ilia", "-p", "apps"]));
        assert!(opens(&["ilia", "-p", "Apps"]));
        assert!(opens(&["ilia", "-a", "-p", "windows"]));
        assert!(!opens(&["ilia", "-p", "textlist"]));
        assert!(!opens(&["ilia", "-p", "keybindings"]));
        assert!(!opens(&["ilia", "-p", "windows"]));
    }

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
