use gio::prelude::SettingsExt;
use gtk::gdk;
use gtk::prelude::*;
use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use crate::args::Args;
use crate::pages::{find, DialogPage, Page, PAGES};

const ALL_PAGE_IDS: &[&str] = &[
    "apps",
    "terminal",
    "notifications",
    "keybindings",
    "windows",
    "tracker",
];

pub type SharedSeat = Rc<RefCell<Option<gdk::Seat>>>;

pub fn build(app: &gtk::Application, args: &Args, seat: SharedSeat) -> Result<gtk::Window, ()> {
    let focus_name = args.value("-p").unwrap_or("Apps");
    let all = args.has("-a");
    let focus = find(focus_name);

    let settings = gio::Settings::new("org.regolith-linux.ilia");
    let width = settings.int("window-width");
    let height = settings.int("window-height");
    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_application(Some(app));
    gdk::set_program_class("ilia");
    window.set_type_hint(gdk::WindowTypeHint::PopupMenu);
    window.set_position(gtk::WindowPosition::CenterAlways);
    window.set_default_size(width, height);

    let grid = gtk::Grid::new();
    grid.style_context().add_class("root_box");
    let entry = gtk::Entry::new();
    entry.style_context().add_class("filter_entry");
    entry.set_hexpand(true);
    entry.connect_button_press_event(|_, event| {
        if event.button() == 3 {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let notebook = gtk::Notebook::new();
    notebook.style_context().add_class("notebook");
    notebook.set_tab_pos(gtk::PositionType::Bottom);
    if focus.is_none() {
        eprintln!("Unknown page type: {focus_name}");
        if !all {
            eprintln!("No page loaded, exiting");
            return Err(());
        }
    }
    grid.attach(&entry, 0, 0, 1, 1);
    grid.attach(&notebook, 0, 1, 1, 1);
    window.add(&grid);

    let selected = focus.unwrap_or(&PAGES[0]);
    let pages: Vec<&Page> = if all {
        ALL_PAGE_IDS.iter().filter_map(|id| find(id)).collect()
    } else {
        vec![selected]
    };
    for page in &pages {
        page.initialize();
        let _ = page.get_keybinding();
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let icon = if page.id == "textlist" {
            args.value("-i").unwrap_or(page.get_icon_name())
        } else {
            page.get_icon_name()
        };
        let image = gtk::Image::from_icon_name(Some(icon), gtk::IconSize::Button);
        let label = gtk::Label::new(None);
        let name = if page.id == "textlist" {
            args.value("-l").unwrap_or(page.get_name())
        } else {
            page.get_name()
        };
        label.set_markup(name);
        tab.pack_start(&image, false, false, 0);
        tab.pack_start(&label, false, false, 5);
        notebook.append_page(&page.get_root(), Some(&tab));
    }
    if all {
        // GtkNotebook falls back to page zero when hidden pages first become
        // visible. Realize the page set before choosing the requested tab so
        // a later window.show_all() preserves the selection.
        notebook.show_all();
        if let Some(index) = pages.iter().position(|page| page.id == selected.id) {
            notebook.set_current_page(Some(index as u32));
        }
    } else {
        notebook.append_page(&help_widget(selected), Some(&gtk::Label::new(Some("Help"))));
    }

    if selected.id == "textlist" {
        std::thread::spawn(|| {
            let mut ignored = String::new();
            let _ = std::io::stdin().read_to_string(&mut ignored);
        });
    }

    let pages_for_change = pages.clone();
    let notebook_for_change = notebook.clone();
    entry.connect_changed(move |_| {
        let index = notebook_for_change.current_page().unwrap_or(0) as usize;
        if let Some(page) = pages_for_change.get(index) {
            page.on_entry_changed();
        }
    });
    let pages_for_activate = pages.clone();
    let notebook_for_activate = notebook.clone();
    entry.connect_activate(move |_| {
        let index = notebook_for_activate.current_page().unwrap_or(0) as usize;
        if let Some(page) = pages_for_activate.get(index) {
            page.on_entry_activated();
        }
    });
    let pages_for_switch = pages.clone();
    let entry_for_switch = entry.clone();
    notebook.connect_switch_page(move |_, _, index| {
        entry_for_switch.set_sensitive((index as usize) < pages_for_switch.len());
        if let Some(page) = pages_for_switch.get(index as usize) {
            page.show();
        }
    });

    let app_for_key = app.clone();
    let window_for_key = window.clone();
    let seat_for_key = seat.clone();
    let notebook_for_key = notebook.clone();
    let pages_for_key = pages.clone();
    let entry_for_key = entry.clone();
    window.connect_key_press_event(move |_, event| {
        let key = event.keyval();
        if matches!(
            key,
            gdk::keys::constants::Escape
                | gdk::keys::constants::Super_L
                | gdk::keys::constants::Super_R
        ) || matches!(
            key,
            gdk::keys::constants::MonBrightnessUp
                | gdk::keys::constants::MonBrightnessDown
                | gdk::keys::constants::AudioMicMute
                | gdk::keys::constants::AudioRaiseVolume
                | gdk::keys::constants::AudioLowerVolume
                | gdk::keys::constants::AudioMute
                | gdk::keys::constants::Print
        ) {
            quit(&window_for_key, &app_for_key, &seat_for_key);
            return glib::Propagation::Stop;
        }
        if matches!(
            key,
            gdk::keys::constants::Left | gdk::keys::constants::Right
        ) {
            notebook_for_key.grab_focus();
            return glib::Propagation::Proceed;
        }
        if matches!(
            key,
            gdk::keys::constants::Up
                | gdk::keys::constants::Down
                | gdk::keys::constants::Return
                | gdk::keys::constants::Page_Up
                | gdk::keys::constants::Page_Down
        ) {
            let index = notebook_for_key.current_page().unwrap_or(0) as usize;
            if let Some(page) = pages_for_key.get(index) {
                page.show();
            }
            return glib::Propagation::Proceed;
        }
        let index = notebook_for_key.current_page().unwrap_or(0) as usize;
        if let Some(page) = pages_for_key.get(index) {
            if page.key_event(key) {
                return glib::Propagation::Stop;
            }
        }
        entry_for_key.grab_focus_without_selecting();
        glib::Propagation::Proceed
    });
    let app_for_focus = app.clone();
    let window_for_focus = window.clone();
    let seat_for_focus = seat.clone();
    window.connect_focus_out_event(move |_, _| {
        quit(&window_for_focus, &app_for_focus, &seat_for_focus);
        glib::Propagation::Proceed
    });
    let app_for_click = app.clone();
    let window_for_click = window.clone();
    let seat_for_click = seat.clone();
    window.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
    window.connect_button_press_event(move |window, event| {
        let (x, y) = event.position();
        let (width, height) = window.size();
        let outside = x < 0.0 || y < 0.0 || x > f64::from(width) || y > f64::from(height);
        if outside {
            quit(&window_for_click, &app_for_click, &seat_for_click);
        }
        if outside {
            glib::Propagation::Proceed
        } else {
            glib::Propagation::Stop
        }
    });
    Ok(window)
}

fn quit(window: &gtk::Window, app: &gtk::Application, seat: &SharedSeat) {
    if let Some(seat) = seat.borrow_mut().take() {
        seat.ungrab();
    }
    window.hide();
    window.close();
    app.quit();
}

pub fn grab_inputs(window: &gtk::Window) -> Option<gdk::Seat> {
    let gdk_window = window.window()?;
    let display = gdk_window.display();
    let seat = display.default_seat()?;
    let capabilities = gdk::SeatCapabilities::KEYBOARD | gdk::SeatCapabilities::POINTER;
    let mut wait = Duration::from_millis(1);
    for attempt in 0..8 {
        if seat.grab(&gdk_window, capabilities, true, None, None, None) == gdk::GrabStatus::Success
        {
            return Some(seat);
        }
        if attempt != 7 {
            wait *= 2;
            std::thread::sleep(wait);
        }
    }
    None
}

fn help_widget(page: &Page) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let help = gtk::Label::new(Some(page.get_help()));
    help.set_line_wrap(true);
    root.pack_start(&help, false, false, 5);
    root.pack_start(&gtk::Label::new(Some("Keybindings")), false, false, 5);

    let model = gtk::ListStore::new(&[glib::Type::STRING, glib::Type::STRING]);
    for (key, action) in page.get_keybindings() {
        let row = model.append();
        model.set(&row, &[(0, &key), (1, &action)]);
    }
    for (key, action) in [
        ("Alt -", "Decrease Dialog Size"),
        ("Alt +", "Increase Dialog Size"),
        ("↑ ↓", "Change Selected Item"),
        ("Ctrl+p Ctrl+n", "Change Selected Item (emacs style)"),
        ("Ctrl+k Ctrl+j", "Change Selected Item (vim style)"),
        ("Esc", "Exit"),
    ] {
        let row = model.append();
        model.set(&row, &[(0, &key), (1, &action)]);
    }

    let bindings = gtk::TreeView::with_model(&model);
    bindings.set_headers_visible(false);
    bindings.set_enable_search(false);
    for (column_index, title) in [(0, "Key"), (1, "Function")] {
        let renderer = gtk::CellRendererText::new();
        let column =
            gtk::TreeViewColumn::with_attributes(title, &renderer, &[("text", column_index)]);
        bindings.append_column(&column);
    }
    bindings.connect_realize(|view| view.columns_autosize());
    root.pack_start(&bindings, false, false, 5);
    root.upcast()
}
