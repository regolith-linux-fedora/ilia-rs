//! Text copied in ilia that stays pasteable after the window closes.
//!
//! The text goes on the clipboard through GTK. When the window closes while
//! ilia still owns the clipboard, the text is handed to the clipboard
//! manager on an X11 display that has one; everywhere else (Wayland, or X11
//! without a manager) ilia keeps running without a window, serving the text,
//! until another program puts something else on the clipboard or the display
//! goes away.

use std::cell::RefCell;

use glib::translate::ToGlibPtr;
use gtk::gdk;
use gtk::prelude::*;

#[derive(Default)]
struct State {
    // Numbers each copy, so that losing an earlier one is told apart.
    generation: u64,
    // The copy ilia owns the clipboard with, if it still does.
    owned: Option<u64>,
    display: Option<gdk::Display>,
    // Set while ilia runs on without a window: ends the application.
    release: Option<Box<dyn FnOnce()>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// The text GTK serves for one copy. GTK drops it when the clipboard no
/// longer holds the copy: replaced by ilia itself or by another program.
struct Copied {
    text: String,
    generation: u64,
}

impl Drop for Copied {
    fn drop(&mut self) {
        let release = STATE.with(|state| {
            let mut state = state.borrow_mut();
            if state.owned != Some(self.generation) {
                return None;
            }
            state.owned = None;
            state.release.take()
        });
        if let Some(release) = release {
            release();
        }
    }
}

/// Puts `text` on the clipboard of `display`.
pub fn copy(display: &gdk::Display, text: &str) {
    let clipboard = gtk::Clipboard::for_display(display, &gdk::SELECTION_CLIPBOARD);
    let generation = STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.generation += 1;
        state.generation
    });
    let copied = Copied {
        text: text.to_owned(),
        generation,
    };
    let served = clipboard.set_with_data(&text_targets(), move |_, selection, _| {
        selection.set_text(&copied.text);
    });
    if !served {
        return;
    }
    // Every target may be handed to a clipboard manager. gtk-rs has no
    // binding for this call.
    // SAFETY: the clipboard pointer is valid for the call; no targets
    // (NULL, 0) means all of them.
    unsafe {
        gtk::ffi::gtk_clipboard_set_can_store(clipboard.to_glib_none().0, std::ptr::null(), 0);
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.owned = Some(generation);
        state.display = Some(display.clone());
    });
}

/// Called as the window closes. Returns true when ilia must keep running to
/// serve its copied text, in which case it exits once the clipboard holds
/// something else; false when the application may quit now.
pub fn keep_after_close(app: &gtk::Application) -> bool {
    let (display, keeping) = STATE.with(|state| {
        let state = state.borrow();
        (
            state.owned.and(state.display.clone()),
            state.release.is_some(),
        )
    });
    let Some(display) = display else {
        return false;
    };
    if keeping {
        return true;
    }
    if display.supports_clipboard_persistence() {
        // An X11 clipboard manager fetches and keeps the text.
        gtk::Clipboard::for_display(&display, &gdk::SELECTION_CLIPBOARD).store();
        return false;
    }
    let hold = app.hold();
    // A launcher started later replaces this one on the bus; serving the
    // clipboard, this one stays.
    let name_lost = app.connect_local("name-lost", false, |_| Some(true.to_value()));
    let app = app.clone();
    STATE.with(|state| {
        state.borrow_mut().release = Some(Box::new(move || {
            app.disconnect(name_lost);
            drop(hold);
            app.quit();
        }));
    });
    true
}

/// The text targets GTK offers for a string.
fn text_targets() -> Vec<gtk::TargetEntry> {
    [
        "UTF8_STRING",
        "text/plain;charset=utf-8",
        "STRING",
        "TEXT",
        "COMPOUND_TEXT",
        "text/plain",
    ]
    .into_iter()
    .map(|target| gtk::TargetEntry::new(target, gtk::TargetFlags::empty(), 0))
    .collect()
}
