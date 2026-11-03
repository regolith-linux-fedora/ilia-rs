use std::cell::{Cell, RefCell};
use std::path::Path;
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

pub fn run(args: Args) -> i32 {
    let is_wayland = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        && std::env::var("GDK_BACKEND").as_deref() != Ok("x11");
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
    app.connect_activate(
        move |app| match dialog::build(
            app,
            &args,
            seat_for_activate.clone(),
            wm_name,
            is_wayland,
        ) {
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

#[cfg(test)]
mod tests {
    use super::detect_wm;

    #[test]
    fn wm_detection_prefers_sway_then_i3() {
        assert_eq!(detect_wm(true, true), "sway");
        assert_eq!(detect_wm(false, true), "i3");
        assert_eq!(detect_wm(false, false), "Unknown");
    }
}
