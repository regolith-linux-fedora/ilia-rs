//! The Calculator page: the answer to the typed expression, shown as it is
//! typed, printed and put on the clipboard with Enter.

mod answer;
mod evaluation;
mod expression;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::gdk;
use gtk::prelude::*;

use crate::clipboard_keeper;
use crate::pages::{is_enter_key, DialogPage};
use evaluation::Evaluation;

/// An answer not worked out this long after the last change is given up.
const TIME_LIMIT: Duration = Duration::from_secs(3);
/// Shown when the answer was given up on.
const TOO_LONG: &str = "Too long";
/// The page's text on the Help page.
pub const HELP: &str = "This dialog works out the expression typed in the top text box as it is typed: numbers, + - * / ^, ! after a number, brackets, sqrt, sin, cos, tan (in radians), ln, exp, abs, pi and e. Enter prints the answer and copies it to the clipboard, where it stays after the dialog closes.";

/// Where the answer to the current text stands.
enum Answer {
    /// The text has no answer (it is empty, incomplete, or has no value).
    None,
    /// The answer is being worked out.
    Pending {
        /// Dropping it abandons the work.
        _evaluation: Evaluation,
        /// Removed once it has run out.
        time_limit: Option<glib::SourceId>,
        /// Enter was pressed: confirm the answer once it arrives.
        confirm_on_arrival: bool,
    },
    /// The answer, as shown.
    Shown(String),
    /// The answer was not worked out in time.
    TooLong,
}

pub struct CalculatorPage {
    entry: gtk::Entry,
    label: gtk::Label,
    root: gtk::Widget,
    quit: Rc<dyn Fn()>,
    answer: RefCell<Answer>,
    /// The text the answer belongs to.
    text: RefCell<String>,
    /// Counts the changes to the text, so that a late answer to earlier
    /// text is never taken for the answer to the current text.
    generation: Cell<u64>,
    this: Weak<CalculatorPage>,
}

impl CalculatorPage {
    pub fn new(entry: gtk::Entry, quit: Rc<dyn Fn()>) -> Rc<Self> {
        // A label rather than a list row, always present directly under the
        // entry, and empty whenever there is no answer to show.
        let label = gtk::Label::new(None);
        label.style_context().add_class("calculator_answer");
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_halign(gtk::Align::Fill);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.pack_start(&label, false, false, 8);
        let page = Rc::new_cyclic(|this| Self {
            entry,
            label,
            root: root.upcast(),
            quit,
            answer: RefCell::new(Answer::None),
            text: RefCell::new(String::new()),
            generation: Cell::new(0),
            this: this.clone(),
        });
        // Closing the window abandons the work in progress.
        let this = page.this.clone();
        page.root.connect_destroy(move |_| {
            if let Some(page) = this.upgrade() {
                page.set_answer(Answer::None);
            }
        });
        page
    }

    /// Abandons the work for the previous text and starts on `text`.
    fn evaluate(&self, text: &str) {
        self.text.replace(text.to_owned());
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        // Dropping a pending answer kills its helper and cancels a waiting
        // Enter.
        self.set_answer(Answer::None);
        let Some(expression) = expression::translate(text) else {
            return;
        };
        let this = self.this.clone();
        let started = Evaluation::start(&expression, move |value| {
            if let Some(page) = this.upgrade() {
                page.arrived(generation, value);
            }
        });
        let evaluation = match started {
            Ok(evaluation) => evaluation,
            Err(error) => {
                eprintln!("Cannot start the calculator: {}", error.message());
                return;
            }
        };
        let this = self.this.clone();
        let time_limit = glib::timeout_add_local_once(TIME_LIMIT, move || {
            if let Some(page) = this.upgrade() {
                page.timed_out(generation);
            }
        });
        // While the answer is worked out the label stays empty.
        self.answer.replace(Answer::Pending {
            _evaluation: evaluation,
            time_limit: Some(time_limit),
            confirm_on_arrival: false,
        });
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation
            && matches!(*self.answer.borrow(), Answer::Pending { .. })
    }

    fn arrived(&self, generation: u64, value: Option<answer::Value>) {
        if !self.is_current(generation) {
            return;
        }
        let confirm = match &*self.answer.borrow() {
            Answer::Pending {
                confirm_on_arrival, ..
            } => *confirm_on_arrival,
            _ => false,
        };
        match value {
            Some(value) => {
                let shown = value.show();
                self.set_answer(Answer::Shown(shown.clone()));
                if confirm {
                    self.confirm(&shown);
                }
            }
            None => self.set_answer(Answer::None),
        }
    }

    fn timed_out(&self, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        // The source that ran out is gone already.
        if let Answer::Pending { time_limit, .. } = &mut *self.answer.borrow_mut() {
            time_limit.take();
        }
        self.set_answer(Answer::TooLong);
    }

    /// Replaces the answer and shows it, abandoning the previous one.
    fn set_answer(&self, answer: Answer) {
        self.label.set_text(match &answer {
            Answer::Shown(shown) => shown,
            Answer::TooLong => TOO_LONG,
            Answer::None | Answer::Pending { .. } => "",
        });
        // Dropping a pending answer kills its helper.
        if let Answer::Pending {
            time_limit: Some(time_limit),
            ..
        } = self.answer.replace(answer)
        {
            time_limit.remove();
        }
    }

    /// Enter: confirm the answer shown, or the one on its way.
    fn enter(&self) {
        let shown = match &mut *self.answer.borrow_mut() {
            Answer::Shown(shown) => Some(shown.clone()),
            Answer::Pending {
                confirm_on_arrival, ..
            } => {
                *confirm_on_arrival = true;
                None
            }
            Answer::None | Answer::TooLong => None,
        };
        if let Some(shown) = shown {
            self.confirm(&shown);
        }
    }

    /// Prints the answer, puts it on the clipboard and closes the window.
    fn confirm(&self, shown: &str) {
        let mut stdout = io::stdout().lock();
        let _ = writeln!(stdout, "{shown}").and_then(|()| stdout.flush());
        drop(stdout);
        clipboard_keeper::copy(&WidgetExt::display(&self.entry), shown);
        (self.quit)();
    }
}

impl DialogPage for CalculatorPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn on_entry_changed(&self) {
        let text = self.entry.text();
        // Coming back to the page brings its text back unchanged: the answer
        // to it, or the work on it and its time limit, carry on.
        if *self.text.borrow() != text.as_str() {
            self.evaluate(text.as_str());
        }
    }

    fn on_entry_activated(&self) {
        self.enter();
    }

    fn get_id(&self) -> &'static str {
        "calculator"
    }

    fn get_name(&self) -> &'static str {
        "Ca<u>l</u>culator"
    }

    fn get_icon_name(&self) -> &'static str {
        "accessories-calculator"
    }

    fn get_help(&self) -> &'static str {
        HELP
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Print and Copy Answer")].into_iter().collect()
    }

    fn key_event(&self, event: &gdk::EventKey) -> bool {
        if is_enter_key(&event.keyval()) {
            self.enter();
            return true;
        }
        false
    }

    fn get_keybinding(&self) -> Option<char> {
        Some('l')
    }

    fn show(&self) {
        self.entry.grab_focus_without_selecting();
    }

    fn hide(&self) {
        // Leaving the page cancels a waiting Enter; the answer is still
        // worked out, for when the page comes back.
        if let Answer::Pending {
            confirm_on_arrival, ..
        } = &mut *self.answer.borrow_mut()
        {
            *confirm_on_arrival = false;
        }
    }

    fn edits_with_arrow_keys(&self) -> bool {
        true
    }
}
