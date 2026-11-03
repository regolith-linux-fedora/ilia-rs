use std::collections::BTreeMap;
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;

use crate::apps::ApplicationsPage;

#[derive(Clone, Copy)]
pub struct Page {
    pub id: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub key: char,
    pub help: &'static str,
    pub bindings: &'static [(&'static str, &'static str)],
}

pub trait DialogPage {
    fn initialize(&self) {}
    fn get_root(&self) -> gtk::Widget;
    fn on_entry_changed(&self) {}
    fn on_entry_activated(&self) {}
    fn get_id(&self) -> &'static str;
    fn get_name(&self) -> &'static str;
    fn get_icon_name(&self) -> &'static str;
    fn get_help(&self) -> &'static str;
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str>;
    fn key_event(&self, _event: &gdk::EventKey) -> bool {
        false
    }
    fn get_keybinding(&self) -> char;
    fn show(&self) {}
}

impl DialogPage for Page {
    fn get_root(&self) -> gtk::Widget {
        gtk::ListBox::new().upcast()
    }

    fn get_id(&self) -> &'static str {
        self.id
    }

    fn get_name(&self) -> &'static str {
        self.name
    }

    fn get_icon_name(&self) -> &'static str {
        self.icon
    }

    fn get_help(&self) -> &'static str {
        self.help
    }

    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        self.bindings.iter().copied().collect()
    }

    fn get_keybinding(&self) -> char {
        self.key
    }
}

pub const PAGES: &[Page] = &[
    Page { id: "apps", name: "<u>A</u>pplications", icon: "system-run", key: 'a', help: "This dialog allows for the launching of desktop applications. Initially all desktop apps are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it.", bindings: &[("enter", "Launch Application")] },
    Page { id: "terminal", name: "<u>C</u>ommands", icon: "utilities-terminal", key: 'c', help: "This dialog allows for the executing commands on the user path. Initially all commands are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will launch it.", bindings: &[("enter", "Execute Command")] },
    Page { id: "notifications", name: "<u>N</u>otifications", icon: "mail-message-new", key: 'n', help: "This dialog manages desktop notifications. Initially all desktop apps are presented. The user may filter the list in the top text box.", bindings: &[("del", "Delete Notification"), ("shift del", "Delete All Notifications")] },
    Page { id: "keybindings", name: "<u>K</u>eybindings", icon: "input-keyboard", key: 'k', help: "This dialog allows for viewing desktop keybindings. Initially all keybindings are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will perform the keybinding action.", bindings: &[("enter", "Invoke Keybinding")] },
    Page { id: "textlist", name: "TextList", icon: "utilities-terminal", key: 't', help: "This dialog presents a filterable list of items to select from.", bindings: &[("enter", "Select Item")] },
    Page { id: "windows", name: "<u>W</u>indows", icon: "window-new", key: 'w', help: "This dialog allows for navigating between open windows in the desktop environment.  Selecting a window from the list will cause it to be viewed on screen and focused.", bindings: &[("enter", "Navigate to Window")] },
    Page { id: "tracker", name: "<u>F</u>iles", icon: "emblem-documents", key: 'f', help: "This dialog allows for searching for files based on content. Type in a text query, hit enter and select from the list of files which contain the search string to open it.", bindings: &[("enter", "Open File")] },
];

pub fn find(name: &str) -> Option<&'static Page> {
    PAGES.iter().find(|page| page.id.eq_ignore_ascii_case(name))
}

pub fn create(
    page: &'static Page,
    settings: &gio::Settings,
    entry: &gtk::Entry,
    quit: Rc<dyn Fn()>,
) -> Rc<dyn DialogPage> {
    if page.id == "apps" {
        Rc::new(ApplicationsPage::new(settings.clone(), entry.clone(), quit))
    } else {
        Rc::new(*page)
    }
}
