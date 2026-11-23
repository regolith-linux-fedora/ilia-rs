//! The form that adds an application: a new desktop entry in the user's
//! applications directory.
//!
//! It is shown inside ilia's window, over the application list, rather than
//! as a dialog window of its own: ilia's window takes the keyboard (a grab on
//! X11, an exclusive overlay layer surface on Wayland), so a window of its
//! own would never get the keys. While it is shown, the window hands every
//! key to `handle_key`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::atk::prelude::AtkObjectExt;
use gtk::gdk;
use gtk::prelude::*;

use crate::new_entry::{EntryInput, Field, NewEntry};

type Callback<T> = RefCell<Option<Box<dyn Fn(T)>>>;

pub struct AddAppForm {
    root: gtk::EventBox,
    name: gtk::Entry,
    command: gtk::Entry,
    icon: gtk::Entry,
    comment: gtk::Entry,
    keywords: gtk::Entry,
    terminal: gtk::CheckButton,
    error: gtk::Label,
    // The controls in Tab order.
    focus_order: Vec<gtk::Widget>,
    cancelled: Callback<()>,
    added: Callback<PathBuf>,
}

impl AddAppForm {
    pub fn new() -> Rc<Self> {
        let grid = gtk::Grid::new();
        grid.set_row_spacing(4);
        grid.set_column_spacing(8);
        let fields: Vec<gtk::Entry> = ["Name", "Command", "Icon", "Comment", "Keywords"]
            .iter()
            .enumerate()
            .map(|(row, title)| add_field(&grid, row as i32, title))
            .collect();
        let [name, command, icon, comment, keywords]: [gtk::Entry; 5] =
            fields.try_into().expect("five fields");

        let terminal = gtk::CheckButton::with_label("Run in terminal");

        // Empty and hidden unless an Add failed. The accessible name stays
        // "Error"; the text is the message.
        let error = gtk::Label::new(None);
        error.style_context().add_class("error");
        if let Some(accessible) = error.accessible() {
            accessible.set_name("Error");
        }
        error.set_xalign(0.0);
        error.set_line_wrap(true);
        error.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
        error.set_lines(2);
        error.set_ellipsize(gtk::pango::EllipsizeMode::End);
        error.set_no_show_all(true);

        let cancel = gtk::Button::with_label("Cancel");
        let add = gtk::Button::with_label("Add");
        add.style_context().add_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.pack_end(&add, false, false, 0);
        buttons.pack_end(&cancel, false, false, 0);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.style_context().add_class("add_app_form");
        content.set_border_width(6);
        content.pack_start(&grid, false, false, 0);
        content.pack_start(&terminal, false, false, 0);
        content.pack_start(&error, false, false, 0);
        content.pack_end(&buttons, false, false, 0);
        content.show_all();

        // A window of its own (and the theme's background) so that the list
        // below neither shows through nor gets the form's clicks.
        let root = gtk::EventBox::new();
        root.style_context().add_class("background");
        root.add(&content);
        root.set_no_show_all(true);
        // Keys the focused control does not use stop here, short of the
        // notebook's and the window's key bindings.
        root.connect_key_press_event(|_, _| glib::Propagation::Stop);

        let focus_order = vec![
            name.clone().upcast(),
            command.clone().upcast(),
            icon.clone().upcast(),
            comment.clone().upcast(),
            keywords.clone().upcast(),
            terminal.clone().upcast(),
            cancel.clone().upcast(),
            add.clone().upcast(),
        ];
        let form = Rc::new(Self {
            root,
            name,
            command,
            icon,
            comment,
            keywords,
            terminal,
            error,
            focus_order,
            cancelled: RefCell::default(),
            added: RefCell::default(),
        });

        // Enter in any field does what Add does.
        for field in form.fields() {
            let weak = Rc::downgrade(&form);
            field.connect_activate(move |_| {
                if let Some(form) = weak.upgrade() {
                    form.submit();
                }
            });
        }
        let weak = Rc::downgrade(&form);
        add.connect_clicked(move |_| {
            if let Some(form) = weak.upgrade() {
                form.submit();
            }
        });
        let weak = Rc::downgrade(&form);
        cancel.connect_clicked(move |_| {
            if let Some(form) = weak.upgrade() {
                form.cancel();
            }
        });
        form
    }

    pub fn widget(&self) -> &gtk::EventBox {
        &self.root
    }

    /// Called when the form is closed without adding anything.
    pub fn connect_cancelled(&self, callback: impl Fn() + 'static) {
        *self.cancelled.borrow_mut() = Some(Box::new(move |()| callback()));
    }

    /// Called with the path of each desktop entry written.
    pub fn connect_added(&self, callback: impl Fn(PathBuf) + 'static) {
        *self.added.borrow_mut() = Some(Box::new(callback));
    }

    pub fn is_open(&self) -> bool {
        self.root.is_visible()
    }

    /// Shows the form, emptied, with the keyboard focus on Name.
    pub fn open(&self) {
        for field in self.fields() {
            field.set_text("");
        }
        self.terminal.set_active(false);
        self.set_error("");
        self.root.show();
        self.name.grab_focus();
    }

    pub fn close(&self) {
        self.root.hide();
        self.set_error("");
    }

    /// Gives the keyboard focus to the form, unless one of its controls has it.
    pub fn take_focus(&self) {
        let focus = self.window().and_then(|window| window.focused_widget());
        if !focus.is_some_and(|focus| focus.is_ancestor(&self.root)) {
            self.name.grab_focus();
        }
    }

    /// Handles a key the window received while the form is shown. Every key
    /// belongs to the form: the page and window bindings (page switching,
    /// list navigation, Escape quitting) never see them.
    pub fn handle_key(&self, event: &gdk::EventKey) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        self.take_focus();
        let key = event.keyval();
        match key {
            gdk::keys::constants::Escape => {
                self.cancel();
                return true;
            }
            gdk::keys::constants::Tab
            | gdk::keys::constants::KP_Tab
            | gdk::keys::constants::ISO_Left_Tab => {
                let backwards = key == gdk::keys::constants::ISO_Left_Tab
                    || event.state().contains(gdk::ModifierType::SHIFT_MASK);
                self.move_focus(window.focused_widget(), backwards);
                return true;
            }
            gdk::keys::constants::Return
            | gdk::keys::constants::KP_Enter
            | gdk::keys::constants::ISO_Enter
            | gdk::keys::constants::space
            | gdk::keys::constants::KP_Space => {
                // GTK's own key activation of a button grabs the keyboard
                // for a moment, which would end ilia's grab on X11.
                if let Some(button) = window.focused_widget().and_downcast::<gtk::Button>() {
                    button.clicked();
                    return true;
                }
            }
            _ => {}
        }
        // Alt and a letter switches pages outside the form: nothing here.
        if !event.state().contains(gdk::ModifierType::MOD1_MASK) {
            window.propagate_key_event(event);
        }
        true
    }

    /// Whether the form acts on a key beyond the dialog's own keys: Tab and
    /// Shift+Tab, which move between its controls.
    pub fn uses_key(&self, event: &gdk::EventKey) -> bool {
        matches!(
            event.keyval(),
            gdk::keys::constants::Tab
                | gdk::keys::constants::KP_Tab
                | gdk::keys::constants::ISO_Left_Tab
        ) && !event.state().intersects(
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::MOD1_MASK
                | gdk::ModifierType::SUPER_MASK,
        )
    }

    fn window(&self) -> Option<gtk::Window> {
        self.root.toplevel().and_downcast::<gtk::Window>()
    }

    fn fields(&self) -> [&gtk::Entry; 5] {
        [
            &self.name,
            &self.command,
            &self.icon,
            &self.comment,
            &self.keywords,
        ]
    }

    fn move_focus(&self, focus: Option<gtk::Widget>, backwards: bool) {
        let count = self.focus_order.len();
        let next = focus
            .and_then(|focus| self.focus_order.iter().position(|widget| *widget == focus))
            .map_or(0, |index| {
                if backwards {
                    (index + count - 1) % count
                } else {
                    (index + 1) % count
                }
            });
        self.focus_order[next].grab_focus();
    }

    fn set_error(&self, message: &str) {
        self.error.set_text(message);
        self.error.set_visible(!message.is_empty());
    }

    fn cancel(&self) {
        if let Some(callback) = self.cancelled.borrow().as_ref() {
            callback(());
        }
    }

    fn submit(&self) {
        let (name, command, icon, comment, keywords) = (
            self.name.text(),
            self.command.text(),
            self.icon.text(),
            self.comment.text(),
            self.keywords.text(),
        );
        let input = EntryInput {
            name: &name,
            command: &command,
            icon: &icon,
            comment: &comment,
            keywords: &keywords,
            terminal: self.terminal.is_active(),
        };
        match NewEntry::validate(&input).and_then(|entry| entry.save()) {
            Ok(path) => {
                self.set_error("");
                if let Some(callback) = self.added.borrow().as_ref() {
                    callback(path);
                }
            }
            Err(error) => {
                self.set_error(error.message());
                match error.field() {
                    Some(Field::Name) => self.name.grab_focus(),
                    Some(Field::Command) => self.command.grab_focus(),
                    Some(Field::Icon) => self.icon.grab_focus(),
                    None => {}
                }
            }
        }
    }
}

/// A labelled text field on `row` of the grid. Its accessible name is the
/// label's text.
fn add_field(grid: &gtk::Grid, row: i32, title: &str) -> gtk::Entry {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    let field = gtk::Entry::new();
    field.set_hexpand(true);
    if let Some(accessible) = field.accessible() {
        accessible.set_name(title);
    }
    label.set_mnemonic_widget(Some(&field));
    // No context menu: it would open a window of its own and break the grab.
    field.connect_button_press_event(|_, event| {
        if event.button() == gdk::BUTTON_SECONDARY {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(&field, 1, row, 1, 1);
    field
}
