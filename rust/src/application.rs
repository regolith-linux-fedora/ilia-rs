use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use gio::prelude::*;
use gtk::prelude::*;

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

pub fn run(args: Args) -> i32 {
    if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        && std::env::var("GDK_BACKEND").as_deref() != Ok("x11")
    {
        eprint!("The wayland compositor does not support the layer-shell protocol, aborting.");
        return 1;
    }
    let app = gtk::Application::new(
        Some("org.regolith.launcher"),
        gio::ApplicationFlags::REPLACE | gio::ApplicationFlags::ALLOW_REPLACEMENT,
    );
    let exit_status = Rc::new(Cell::new(0));
    let seat = Rc::new(RefCell::new(None::<gtk::gdk::Seat>));
    let status_for_activate = exit_status.clone();
    let seat_for_activate = seat.clone();
    app.connect_activate(
        move |app| match dialog::build(app, &args, seat_for_activate.clone()) {
            Ok(window) => {
                if !load_style(&window, &args) {
                    status_for_activate.set(1);
                    app.quit();
                    return;
                }
                window.show_all();
                while gtk::events_pending() {
                    gtk::main_iteration();
                }
                match dialog::grab_inputs(&window) {
                    Some(grabbed) => *seat_for_activate.borrow_mut() = Some(grabbed),
                    None => {
                        eprint!("Failed to acquire access to input devices, aborting.");
                        status_for_activate.set(1);
                        app.quit();
                    }
                }
            }
            Err(()) => {
                status_for_activate.set(1);
                app.quit();
            }
        },
    );
    let seat_for_shutdown = seat.clone();
    app.connect_shutdown(move |_| {
        if let Some(seat) = seat_for_shutdown.borrow_mut().take() {
            seat.ungrab();
        }
    });
    app.run_with_args(&["ilia"]);
    exit_status.get()
}

fn load_style(window: &gtk::Window, args: &Args) -> bool {
    let provider = gtk::CssProvider::new();
    if let Some(path) = args.value("-t") {
        if !Path::new(path).exists() {
            eprintln!("File '{path}' does not exist.");
            return false;
        }
        if provider.load_from_path(path).is_err() {
            return false;
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
