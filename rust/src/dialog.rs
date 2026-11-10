use gio::prelude::SettingsExt;
use gtk::gdk;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glib::translate::IntoGlib;
use glib::Unichar;

use crate::args::Args;
use crate::input_sources::InputSourceSwitcher;
use crate::pages::{create, find, is_enter_key, DialogPage, Page, PAGES};

const MIN_WINDOW_WIDTH: i32 = 160;
const MIN_WINDOW_HEIGHT: i32 = 100;

const ALL_PAGE_IDS: &[&str] = &[
    "apps",
    "terminal",
    "notifications",
    "keybindings",
    "windows",
    "tracker",
];

pub type SharedSeat = Rc<RefCell<Option<gdk::Seat>>>;

pub fn build(
    app: &gtk::Application,
    args: &Args,
    seat: SharedSeat,
    wm_name: &str,
    is_wayland: bool,
) -> Result<gtk::Window, ()> {
    let focus_name = args.value("-p").unwrap_or("Apps");
    let all = args.has("-a");
    // With -a only the general pages are loaded: textlist cannot be chosen.
    let focus = find(focus_name).filter(|page| !all || ALL_PAGE_IDS.contains(&page.id));

    let settings = gio::Settings::new("org.regolith-linux.ilia");
    let width = settings.int("window-width");
    let height = settings.int("window-height");
    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_application(Some(app));
    gdk::set_program_class("ilia");
    window.set_type_hint(gdk::WindowTypeHint::PopupMenu);
    window.set_position(gtk::WindowPosition::CenterAlways);
    if is_wayland {
        window.set_size_request(width, height);
    } else {
        window.set_default_size(width, height);
    }

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
    let page_specs: Vec<&Page> = if all {
        ALL_PAGE_IDS.iter().filter_map(|id| find(id)).collect()
    } else {
        vec![selected]
    };
    let app_for_pages = app.clone();
    let window_for_pages = window.clone();
    let seat_for_pages = seat.clone();
    let page_quit: Rc<dyn Fn()> =
        Rc::new(move || quit(&window_for_pages, &app_for_pages, &seat_for_pages));
    let pages: Vec<Rc<dyn DialogPage>> = page_specs
        .iter()
        .map(|page| {
            create(
                page,
                args,
                &settings,
                &entry,
                page_quit.clone(),
                wm_name,
                is_wayland,
            )
        })
        .collect();
    for page in &pages {
        page.initialize();
        let _ = page.get_keybinding();
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let icon = if page.get_id() == "textlist" {
            args.value("-i").unwrap_or(page.get_icon_name())
        } else {
            page.get_icon_name()
        };
        // The icon is a button that shows its page, as the label does; it
        // never takes the keyboard focus.
        let button = gtk::Button::new();
        button.set_can_focus(false);
        button.set_relief(gtk::ReliefStyle::None);
        button.add(&gtk::Image::from_icon_name(
            Some(icon),
            gtk::IconSize::Button,
        ));
        let notebook_for_button = notebook.clone();
        let page_index = notebook.n_pages();
        button.connect_clicked(move |_| notebook_for_button.set_current_page(Some(page_index)));
        let label = gtk::Label::new(None);
        let name = if page.get_id() == "textlist" {
            args.value("-l").unwrap_or(page.get_name())
        } else {
            page.get_name()
        };
        label.set_markup(name);
        tab.pack_start(&button, false, false, 0);
        tab.pack_start(&label, false, false, 5);
        // Notebook tab widgets are not reached by the window's show_all().
        tab.show_all();
        notebook.append_page(&page.get_root(), Some(&tab));
    }
    let active_page = Rc::new(Cell::new(0usize));
    if all {
        // GtkNotebook falls back to page zero when hidden pages first become
        // visible. Realize the page set before choosing the requested tab so
        // a later window.show_all() preserves the selection.
        notebook.show_all();
        if let Some(index) = pages.iter().position(|page| page.get_id() == selected.id) {
            notebook.set_current_page(Some(index as u32));
            active_page.set(index);
        }
    } else {
        notebook.append_page(
            &help_widget(pages[0].as_ref()),
            Some(&gtk::Label::new(Some("Help"))),
        );
    }

    let pages_for_change = pages.clone();
    let active_for_change = active_page.clone();
    entry.connect_changed(move |_| {
        pages_for_change[active_for_change.get()].on_entry_changed();
    });
    let pages_for_activate = pages.clone();
    let active_for_activate = active_page.clone();
    entry.connect_activate(move |_| {
        pages_for_activate[active_for_activate.get()].on_entry_activated();
    });
    let pages_for_switch = pages.clone();
    let entry_for_switch = entry.clone();
    let active_for_switch = active_page.clone();
    // Like the reference, watch page switches only once the window is shown,
    // so building and showing the notebook is not treated as a switch.
    let notebook_for_switch = notebook.clone();
    let switch_connected = Cell::new(false);
    window.connect_show(move |_| {
        if switch_connected.replace(true) {
            return;
        }
        notebook_for_switch.set_current_page(Some(active_for_switch.get() as u32));
        let pages_for_switch = pages_for_switch.clone();
        let entry_for_switch = entry_for_switch.clone();
        let active_for_switch = active_for_switch.clone();
        notebook_for_switch.connect_switch_page(move |_, _, index| {
            let index = index as usize;
            if index == pages_for_switch.len() {
                // The Help page: the filter has nothing to act on.
                entry_for_switch.set_sensitive(false);
            } else if let Some(page) = pages_for_switch.get(index) {
                active_for_switch.set(index);
                entry_for_switch.set_sensitive(true);
                // Filter the new page by the text already typed.
                page.on_entry_changed();
            }
            pages_for_switch[active_for_switch.get()].show();
        });
    });

    // The desktop's layout-switch shortcuts. On X11 ilia's keyboard grab
    // keeps them from the desktop, so ilia switches the layout itself.
    let layout_switcher = InputSourceSwitcher::from_settings(
        gdk::Keymap::for_display(&WidgetExt::display(&window)).as_ref(),
    );
    // Super was pressed as part of a layout-switch shortcut: exit if it is
    // released on its own.
    let exit_on_super_release = Rc::new(Cell::new(false));

    let app_for_key = app.clone();
    let window_for_key = window.clone();
    let seat_for_key = seat.clone();
    let notebook_for_key = notebook.clone();
    let pages_for_key = pages.clone();
    let entry_for_key = entry.clone();
    let active_for_key = active_page.clone();
    let settings_for_key = settings.clone();
    let super_release_for_key = exit_on_super_release.clone();
    window.connect_key_press_event(move |window, event| {
        let key = event.keyval();
        let keyval = *key;
        let state = event.state();
        if let Some(switcher) = layout_switcher.as_ref().filter(|_| !is_wayland) {
            let base_keyval = keyval_in_first_layout(window, event);
            if let Some(direction) = switcher.match_key(keyval, base_keyval, state) {
                super_release_for_key.set(false);
                InputSourceSwitcher::switch_source(direction);
                return glib::Propagation::Stop;
            }
            if super_release_for_key.get() && !event.is_modifier() {
                // Super did not start a layout switch after all.
                quit(&window_for_key, &app_for_key, &seat_for_key);
                return glib::Propagation::Stop;
            }
        }
        if state.contains(gdk::ModifierType::MOD1_MASK) {
            let shortcuts: Vec<char> = pages_for_key
                .iter()
                .map(|page| page.get_keybinding())
                .collect();
            if let Some(index) = alt_switch_target(&shortcuts, keyval) {
                notebook_for_key.set_current_page(Some(index as u32));
                return glib::Propagation::Stop;
            }
            if keyval == u32::from('+') {
                change_size(window, &settings_for_key, 128, is_wayland);
                return glib::Propagation::Stop;
            }
            if keyval == u32::from('-') {
                change_size(window, &settings_for_key, -128, is_wayland);
                return glib::Propagation::Stop;
            }
        } else if state.contains(gdk::ModifierType::CONTROL_MASK) {
            if let Some(handled) = entry_editing(
                &entry_for_key,
                event,
                pages_for_key[active_for_key.get()].as_ref(),
            ) {
                return if handled {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                };
            }
        }
        if is_super_key(&key)
            && layout_switcher
                .as_ref()
                .is_some_and(|switcher| switcher.super_starts_shortcut(state))
        {
            // Super completes the modifiers of a layout-switch shortcut: do
            // not exit yet. On Wayland the compositor takes the rest of the
            // shortcut and ilia never sees it.
            super_release_for_key.set(!is_wayland);
            return glib::Propagation::Stop;
        }
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
                | gdk::keys::constants::Page_Up
                | gdk::keys::constants::Page_Down
                | gdk::keys::constants::Home
                | gdk::keys::constants::End
                | gdk::keys::constants::KP_Home
                | gdk::keys::constants::KP_End
        ) {
            pages_for_key[active_for_key.get()].show();
            return glib::Propagation::Proceed;
        }
        if is_enter_key(&key) {
            // The page may accept typed text that matches no row; otherwise
            // the list activates its highlighted row, for Return and the
            // keypad Enter alike.
            let page = &pages_for_key[active_for_key.get()];
            if page.key_event(event) {
                return glib::Propagation::Stop;
            }
            page.show();
            return glib::Propagation::Proceed;
        }
        if pages_for_key[active_for_key.get()].key_event(event) {
            return glib::Propagation::Stop;
        }
        entry_for_key.grab_focus_without_selecting();
        glib::Propagation::Proceed
    });
    let app_for_release = app.clone();
    let window_for_release = window.clone();
    let seat_for_release = seat.clone();
    window.connect_key_release_event(move |_, event| {
        if exit_on_super_release.get() && is_super_key(&event.keyval()) {
            quit(&window_for_release, &app_for_release, &seat_for_release);
        }
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
    // The list has the focus at first, as after every page switch.
    pages[active_page.get()].show();
    Ok(window)
}

fn is_super_key(key: &gdk::keys::Key) -> bool {
    matches!(
        *key,
        gdk::keys::constants::Super_L | gdk::keys::constants::Super_R
    )
}

/// The key of a key event as it is in the first layout.
fn keyval_in_first_layout(window: &gtk::Window, event: &gdk::EventKey) -> u32 {
    gdk::Keymap::for_display(&WidgetExt::display(window))
        .and_then(|keymap| {
            keymap.translate_keyboard_state(u32::from(event.hardware_keycode()), event.state(), 0)
        })
        .map_or(*event.keyval(), |(keyval, ..)| keyval)
}

pub fn quit(window: &gtk::Window, app: &gtk::Application, seat: &SharedSeat) {
    if let Some(seat) = seat.borrow_mut().take() {
        seat.ungrab();
    }
    window.hide();
    window.close();
    app.quit();
}

pub fn grab_inputs(window: &gtk::Window) -> Option<gdk::Seat> {
    let gdk_window = window.window()?;
    // The nullable lookup: a missing display is reported, not assumed away.
    let Some(display) = gdk::Display::default() else {
        eprintln!("Failed to get Display");
        return None;
    };
    let Some(seat) = display.default_seat() else {
        println!("Failed to get Seat from Display");
        return None;
    };
    let capabilities = gdk::SeatCapabilities::KEYBOARD | gdk::SeatCapabilities::POINTER;
    // Microseconds, slept with g_usleep like the reference.
    let mut wait: u64 = 1000;
    let mut status = gdk::GrabStatus::Failed;
    for _ in 0..8 {
        status = seat.grab(&gdk_window, capabilities, true, None, None, None);
        if status == gdk::GrabStatus::Success {
            return Some(seat);
        }
        // The reference doubles before every sleep, failed final attempt
        // included: 2 ms, 4 ms, ..., 256 ms.
        wait *= 2;
        glib::usleep(wait);
    }
    eprintln!("Aborting, failed to grab input: {}", status.into_glib());
    None
}

/// Grows or shrinks the dialog by `delta` in both dimensions, ignoring the
/// change below the minimum size or at/above the first monitor's size, and
/// persists the accepted size.
fn change_size(window: &gtk::Window, settings: &gio::Settings, delta: i32, is_wayland: bool) {
    let Some((width, height)) = resized(window.size(), delta, first_monitor_size(window)) else {
        return;
    };
    if is_wayland {
        // The Wayland window is sized by its size request, which is also
        // its minimum: without updating it the window cannot shrink.
        window.set_size_request(width, height);
    }
    window.resize(width, height);
    let _ = settings.set_int("window-width", width);
    let _ = settings.set_int("window-height", height);
}

/// The size to apply and persist after a resize request, or `None` when the
/// request is ignored.
fn resized(current: (i32, i32), delta: i32, monitor: Option<(i32, i32)>) -> Option<(i32, i32)> {
    let (width, height) = (current.0 + delta, current.1 + delta);
    size_change_allowed(width, height, monitor).then_some((width, height))
}

/// The first page whose shortcut matches the pressed key, compared like the
/// reference: the shortcut itself or its ASCII uppercase form.
fn alt_switch_target(shortcuts: &[char], keyval: u32) -> Option<usize> {
    shortcuts.iter().position(|shortcut| {
        let shortcut = *shortcut as u32;
        shortcut == keyval || shortcut.wrapping_sub(32) == keyval
    })
}

fn first_monitor_size(window: &gtk::Window) -> Option<(i32, i32)> {
    let monitor = WidgetExt::display(window).monitor(0)?;
    let geometry = monitor.geometry();
    Some((geometry.width(), geometry.height()))
}

fn size_change_allowed(width: i32, height: i32, monitor: Option<(i32, i32)>) -> bool {
    if width < MIN_WINDOW_WIDTH || height < MIN_WINDOW_HEIGHT {
        return false;
    }
    match monitor {
        Some((monitor_width, monitor_height)) => width < monitor_width && height < monitor_height,
        None => true,
    }
}

/// Ctrl+key entry editing in the reference's branch order. Returns `None`
/// when the key is not an editing key, so the plain-key handling runs.
fn entry_editing(entry: &gtk::Entry, event: &gdk::EventKey, page: &dyn DialogPage) -> Option<bool> {
    let keyval = *event.keyval();
    let text = entry.text();
    // Entry positions count characters.
    let length = text.chars().count() as i32;
    let key = char::from_u32(keyval)?;
    match key {
        'h' => {
            let position = entry.position();
            if position > 0 {
                entry.set_position(position - 1);
            }
        }
        'l' => {
            let position = entry.position();
            if position < length {
                entry.set_position(position + 1);
            }
        }
        'j' | 'n' | 'k' | 'p' => return Some(page.key_event(event)),
        '0' => entry.set_position(0),
        // Ctrl+$, whichever keys type the dollar sign.
        '$' => entry.set_position(-1),
        'a' => entry.select_region(0, -1),
        'w' => entry.set_position(forward_word(text.as_str(), entry.position())),
        'b' => entry.set_position(backward_word(text.as_str(), entry.position())),
        'c' => clipboard_copy(entry),
        'v' => clipboard_paste(entry),
        'x' => clipboard_cut(entry),
        _ => return None,
    }
    Some(true)
}

/// Words end at white space, punctuation and symbols, Unicode ones
/// included: GLib's `g_unichar_ispunct`, as the reference uses.
fn is_word_break(character: char) -> bool {
    character.is_whitespace() || character.is_punctuation()
}

fn forward_word(text: &str, position: i32) -> i32 {
    let characters: Vec<char> = text.chars().collect();
    let mut found_break = false;
    for (index, character) in characters.iter().enumerate().skip(position.max(0) as usize) {
        if is_word_break(*character) {
            found_break = true;
        } else if found_break {
            return index as i32;
        }
    }
    characters.len() as i32
}

fn backward_word(text: &str, position: i32) -> i32 {
    let characters: Vec<char> = text.chars().collect();
    let mut start = position.min(characters.len() as i32);
    for index in (0..start).rev() {
        if is_word_break(characters[index as usize]) {
            break;
        }
        start = index;
    }
    let mut found_word = false;
    for index in (0..start).rev() {
        if !is_word_break(characters[index as usize]) {
            found_word = true;
        } else if found_word {
            return index + 1;
        }
    }
    0
}

fn clipboard(entry: &gtk::Entry) -> gtk::Clipboard {
    gtk::Clipboard::for_display(&WidgetExt::display(entry), &gdk::SELECTION_CLIPBOARD)
}

fn clipboard_copy(entry: &gtk::Entry) {
    let clipboard = clipboard(entry);
    let text = entry.text();
    match entry.selection_bounds() {
        Some((start, end)) => clipboard.set_text(&slice_chars(text.as_str(), start, end)),
        None => clipboard.set_text(text.as_str()),
    }
    clipboard.store();
}

fn clipboard_cut(entry: &gtk::Entry) {
    let clipboard = clipboard(entry);
    if let Some((start, end)) = entry.selection_bounds() {
        clipboard.set_text(&slice_chars(entry.text().as_str(), start, end));
        clipboard.store();
        entry
            .buffer()
            .delete_text(start as u16, Some((end - start) as u16));
    }
}

fn clipboard_paste(entry: &gtk::Entry) {
    // Don't wait for the text: wait_for_text() runs a nested main loop until
    // the clipboard owner answers, and an owner that hangs keeps ilia running
    // for up to two minutes, even after its window closed.
    let entry = entry.clone();
    clipboard(&entry).request_text(move |_, text| {
        let Some(text) = text else {
            return;
        };
        if let Some((start, end)) = entry.selection_bounds() {
            entry
                .buffer()
                .delete_text(start as u16, Some((end - start) as u16));
        }
        entry.emit_insert_at_cursor(text);
    });
}

fn slice_chars(text: &str, start: i32, end: i32) -> String {
    text.chars()
        .skip(start.max(0) as usize)
        .take((end - start).max(0) as usize)
        .collect()
}

fn help_widget(page: &dyn DialogPage) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let help = gtk::Label::new(Some(page.get_help()));
    help.set_line_wrap(true);
    root.pack_start(&help, false, false, 5);
    let heading = gtk::Label::new(Some("Keybindings"));
    heading.style_context().add_class("help_heading");
    root.pack_start(&heading, false, false, 5);

    let model = gtk::ListStore::new(&[glib::Type::STRING, glib::Type::STRING]);
    for (key, action) in page.get_keybindings() {
        let row = model.append();
        model.set(&row, &[(0, &key), (1, &action)]);
    }
    for (key, action) in [
        ("Alt -", "Decrease Dialog Size"),
        ("Alt +", "Increase Dialog Size"),
        ("↑ ↓", "Change Selected Item"),
        ("Home End", "Select First or Last Item"),
        ("Ctrl+p Ctrl+n", "Change Selected Item (emacs style)"),
        ("Ctrl+k Ctrl+j", "Change Selected Item (vim style)"),
        ("Esc", "Exit"),
    ] {
        let row = model.append();
        model.set(&row, &[(0, &key), (1, &action)]);
    }

    let bindings = gtk::TreeView::with_model(&model);
    bindings.set_headers_visible(false);
    bindings.set_fixed_height_mode(true);
    bindings.set_enable_search(false);
    for (column_index, title) in [(0, "Key"), (1, "Function")] {
        let renderer = gtk::CellRendererText::new();
        let column =
            gtk::TreeViewColumn::with_attributes(title, &renderer, &[("text", column_index)]);
        // A fixed-height list needs fixed-size columns.
        column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
        bindings.append_column(&column);
    }
    bindings.connect_realize(|view| view.columns_autosize());
    root.pack_start(&bindings, false, false, 5);
    root.upcast()
}

#[cfg(test)]
mod tests {
    use super::{alt_switch_target, backward_word, forward_word, resized, size_change_allowed};

    #[test]
    fn resize_respects_minimum_and_monitor_cap() {
        assert!(size_change_allowed(618, 588, Some((1024, 768))));
        assert!(!size_change_allowed(159, 400, Some((1024, 768))));
        assert!(!size_change_allowed(400, 99, Some((1024, 768))));
        assert!(!size_change_allowed(1024, 400, Some((1024, 768))));
        assert!(!size_change_allowed(400, 768, Some((1024, 768))));
        assert!(size_change_allowed(5000, 5000, None));
    }

    #[test]
    fn resize_returns_the_size_to_persist() {
        assert_eq!(
            resized((490, 460), 128, Some((1024, 768))),
            Some((618, 588))
        );
        assert_eq!(
            resized((490, 460), -128, Some((1024, 768))),
            Some((362, 332))
        );
        assert_eq!(resized((200, 150), -128, Some((1024, 768))), None);
        assert_eq!(resized((900, 460), 128, Some((1024, 768))), None);
    }

    #[test]
    fn alt_switch_matches_shortcut_or_its_uppercase() {
        let all_mode = ['a', 'c', 'n', 'k', 'w', 'f'];
        assert_eq!(alt_switch_target(&all_mode, u32::from('w')), Some(4));
        assert_eq!(alt_switch_target(&all_mode, u32::from('W')), Some(4));
        assert_eq!(alt_switch_target(&all_mode, u32::from('t')), None);
        assert_eq!(alt_switch_target(&['t'], u32::from('T')), Some(0));
        assert_eq!(alt_switch_target(&all_mode, u32::from('+')), None);
    }

    #[test]
    fn word_motion_uses_space_and_punctuation_breaks() {
        assert_eq!(forward_word("alpha beta", 0), 6);
        assert_eq!(forward_word("alpha", 2), 5);
        assert_eq!(forward_word("a.b", 0), 2);
        // Ctrl+b jumps to the start of the previous word, not the current one.
        assert_eq!(backward_word("alpha beta", 10), 0);
        assert_eq!(backward_word("one two three", 13), 4);
        assert_eq!(backward_word("one two three", 8), 4);
        assert_eq!(backward_word("alpha", 3), 0);
    }

    #[test]
    fn word_motion_counts_characters_and_breaks_at_unicode_punctuation() {
        // Inverted question mark and guillemets are punctuation, the euro
        // sign a symbol; GLib breaks words at both.
        assert_eq!(forward_word("café¿uno dos", 0), 5);
        assert_eq!(forward_word("«hola» adiós", 1), 7);
        assert_eq!(forward_word("1€ más", 0), 3);
        assert_eq!(backward_word("café¿uno dos", 8), 0);
        assert_eq!(backward_word("café¿uno dos", 12), 5);
        // The end of the text is a character position, not a byte offset.
        assert_eq!(forward_word("añejo", 1), 5);
        assert_eq!(forward_word("über", 0), 4);
    }
}
