use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::filter::matches_all_words;
use crate::ipc;
use crate::pages::{select_first_row, DialogPage};

const SPEC: u32 = 0;
const SUMMARY: u32 = 1;
const EXEC: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
struct Keybinding {
    category: String,
    label: String,
    spec: String,
    exec: Option<String>,
}

pub struct KeybindingsPage {
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: RefCell<gtk::TreePath>,
    wm_name: String,
    quit: Rc<dyn Fn()>,
}

impl KeybindingsPage {
    pub fn new(entry: gtk::Entry, wm_name: String, quit: Rc<dyn Fn()>) -> Self {
        let model = gtk::ListStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        let filter = gtk::TreeModelFilter::new(&model, None);
        filter.set_visible_func(move |tree_model, iter| {
            matches_all_words(
                entry.text().as_str(),
                &[
                    &string_value(tree_model, iter, SUMMARY as i32),
                    &string_value(tree_model, iter, SPEC as i32),
                ],
            )
        });

        match ipc::get_config(&wm_name).and_then(|config| parse_config(&config)) {
            Ok(bindings) => {
                for binding in bindings {
                    let row = model.append();
                    let spec = format_spec(&binding.spec);
                    let summary = format!("{} - {}", binding.category, binding.label);
                    let exec = binding.exec.unwrap_or_default();
                    model.set(&row, &[(SPEC, &spec), (SUMMARY, &summary), (EXEC, &exec)]);
                }
            }
            Err(error) => eprintln!("Failed to read config from {wm_name}: {error}"),
        }
        model.set_sort_column_id(gtk::SortColumn::Index(SUMMARY), gtk::SortType::Ascending);

        let view = gtk::TreeView::with_model(&filter);
        view.style_context().add_class("keybindings");
        view.set_headers_visible(false);
        view.set_fixed_height_mode(true);
        view.set_enable_search(false);
        view.selection().set_mode(gtk::SelectionMode::Single);
        for (column_index, title) in [(SPEC, "Keybinding"), (SUMMARY, "Action")] {
            let renderer = gtk::CellRendererText::new();
            let column = gtk::TreeViewColumn::with_attributes(
                title,
                &renderer,
                &[("text", column_index as i32)],
            );
            column.set_sizing(gtk::TreeViewColumnSizing::Fixed);
            view.append_column(&column);
        }
        view.set_activate_on_single_click(true);
        let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scrolled.add(&view);
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);

        let page = Self {
            filter,
            view,
            root: scrolled.upcast(),
            path: RefCell::new(gtk::TreePath::new_first()),
            wm_name,
            quit,
        };
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let wm_name = self.wm_name.clone();
        let quit = self.quit.clone();
        self.view.connect_row_activated(move |view, path, _| {
            let Some(model) = view.model() else {
                return;
            };
            let Some(iter) = model.iter(path) else {
                return;
            };
            let exec = string_value(&model, &iter, EXEC as i32);
            if !exec.is_empty() {
                execute(&wm_name, &exec, &quit);
            }
        });
    }

    /// Highlights the first row: the best match for the text typed.
    fn set_selection(&self) {
        self.view.selection().set_mode(gtk::SelectionMode::Single);
        *self.path.borrow_mut() = select_first_row(&self.view);
    }

    fn execute_selected(&self) -> bool {
        let Some(iter) = self.filter.iter(&self.path.borrow()) else {
            return false;
        };
        let exec = string_value(&self.filter, &iter, EXEC as i32);
        if exec.is_empty() {
            return false;
        }
        execute(&self.wm_name, &exec, &self.quit);
        true
    }

    fn navigate(&self, event: &gdk::EventKey) -> bool {
        navigate(&self.view, &self.path, event)
    }
}

impl DialogPage for KeybindingsPage {
    fn get_root(&self) -> gtk::Widget {
        self.root.clone()
    }

    fn on_entry_changed(&self) {
        self.filter.refilter();
        self.set_selection();
    }

    fn on_entry_activated(&self) {
        self.execute_selected();
    }

    fn get_id(&self) -> &'static str {
        "keybindings"
    }
    fn get_name(&self) -> &'static str {
        "<u>K</u>eybindings"
    }
    fn get_icon_name(&self) -> &'static str {
        "input-keyboard"
    }
    fn get_help(&self) -> &'static str {
        "This dialog allows for viewing desktop keybindings. Initially all keybindings are presented. The user may filter the list in the top text box. The arrow keys may be used to select from the list, and enter or clicking on an item will perform the keybinding action."
    }
    fn get_keybindings(&self) -> BTreeMap<&'static str, &'static str> {
        [("enter", "Invoke Keybinding")].into_iter().collect()
    }
    fn key_event(&self, event: &gdk::EventKey) -> bool {
        // Typed text that matches no binding is nothing this page can run,
        // so Enter then does nothing.
        self.navigate(event)
    }
    fn get_keybinding(&self) -> char {
        'k'
    }
    fn show(&self) {
        self.view.grab_focus();
    }
}

fn string_value(
    model: &impl glib::IsA<gtk::TreeModel>,
    iter: &gtk::TreeIter,
    column: i32,
) -> String {
    model
        .value(iter, column)
        .get::<String>()
        .unwrap_or_default()
}

pub(crate) fn navigate(
    view: &gtk::TreeView,
    path: &RefCell<gtk::TreePath>,
    event: &gdk::EventKey,
) -> bool {
    if !event.state().contains(gdk::ModifierType::CONTROL_MASK) {
        return false;
    }
    let moving_up = matches!(
        event.keyval(),
        gdk::keys::constants::p | gdk::keys::constants::k
    );
    let moving_down = matches!(
        event.keyval(),
        gdk::keys::constants::n | gdk::keys::constants::j
    );
    if !moving_up && !moving_down {
        return false;
    }
    let selection = view.selection();
    // The arrow keys, Home/End and the mouse move the selection without
    // updating `path`: start from the highlighted row.
    if let Some(selected) = selection
        .selected()
        .and_then(|(model, iter)| model.path(&iter))
    {
        *path.borrow_mut() = selected;
    }
    let is_last = selection
        .selected()
        .is_some_and(|(model, iter)| !model.iter_next(&iter));
    let mut path = path.borrow_mut();
    if moving_up {
        path.prev();
    } else if !is_last {
        path.next();
    } else {
        return true;
    }
    selection.select_path(&path);
    view.set_cursor(&path, None::<&gtk::TreeViewColumn>, false);
    true
}

pub fn wm_cli(wm_name: &str) -> Option<&'static str> {
    match wm_name {
        "i3" => Some("/usr/bin/i3-msg "),
        "sway" => Some("/usr/bin/swaymsg "),
        _ => None,
    }
}

fn execute(wm_name: &str, exec: &str, quit: &Rc<dyn Fn()>) {
    let Some(commandline) = keybinding_commandline(wm_name, exec) else {
        eprint!("Error: execute_keybinding failed - Action not supported for your WM");
        return;
    };
    match gio::AppInfo::create_from_commandline(&commandline, None, gio::AppInfoCreateFlags::NONE)
        .and_then(|app| app.launch(&[], None::<&gio::AppLaunchContext>))
    {
        Ok(()) => quit(),
        Err(error) => eprintln!("Error: execute_keybinding failed: {}", error.message()),
    }
}

fn keybinding_commandline(wm_name: &str, exec: &str) -> Option<String> {
    wm_cli(wm_name).map(|cli| format!("{cli}\"{exec}\""))
}

fn parse_config(config: &str) -> Result<Vec<Keybinding>, String> {
    let mut bindings = Vec::new();
    let mut pending = None;
    let mut variables = HashMap::new();
    for line in config.lines() {
        let line = line.trim();
        if line.len() > 13
            && line.starts_with("##")
            && line.get(3..).is_some_and(|rest| rest.contains("##"))
            && line.contains("//")
        {
            let Some(end) = line
                .get(3..)
                .and_then(|rest| rest.find("##"))
                .map(|i| i + 3)
            else {
                continue;
            };
            let parts: Vec<_> = line[2..end].split("//").map(str::trim).collect();
            if parts.len() != 3 {
                return Err(format!("Invalid line: {line}\n"));
            }
            bindings.push(Keybinding {
                category: parts[0].to_owned(),
                label: parts[1].to_owned(),
                spec: parts[2].to_owned(),
                exec: None,
            });
            pending = Some(bindings.len() - 1);
        } else if line.len() > 13 && line.starts_with("bindsym ") {
            if let Some(index) = pending.take() {
                let first = line.find(' ').unwrap_or_default();
                if let Some(second) = line[first + 1..].find(' ').map(|i| first + 1 + i) {
                    let mut expression = String::new();
                    for token in line[second + 1..].trim().split(' ') {
                        expression.push_str(variables.get(token).map_or(token, String::as_str));
                        expression.push(' ');
                    }
                    bindings[index].exec = Some(expression);
                }
            }
        } else if line.len() > 13 && line.starts_with("set_from_resource ") {
            let spaces: Vec<_> = line.match_indices(' ').map(|(index, _)| index).collect();
            // Vala-exact space indexing (parseSetFromResourceLine), bug
            // replicated: with only two spaces the reference reads sp3
            // as -1 and inserts the whole line as the value. Fewer
            // spaces are undefined there (negative substring length)
            // and ignored here.
            if spaces.len() == 2 {
                let key = line[spaces[0]..spaces[1]].trim();
                if !key.is_empty() {
                    variables.insert(key.to_owned(), line.to_owned());
                }
            } else if spaces.len() >= 3 {
                let key = line[spaces[0]..spaces[1]].trim();
                let value = line[spaces[2] + 1..].trim();
                if !key.is_empty() && !value.is_empty() {
                    variables.insert(key.to_owned(), value.to_owned());
                }
            }
        }
    }
    // Gee.TreeMap yields categories in lexical order while preserving each
    // category's file order.
    bindings.sort_by(|left, right| left.category.cmp(&right.category));
    Ok(bindings)
}

pub fn format_spec(raw: &str) -> String {
    raw.replace('<', "").replace('>', " ").replace("  ", " ")
}

#[cfg(test)]
mod tests {
    use super::{format_spec, keybinding_commandline, parse_config, wm_cli};

    #[test]
    fn parser_attaches_one_command_and_substitutes_whole_tokens() {
        let parsed = parse_config(
            "set_from_resource $term ignored fixture-terminal\n## Apps // Open // <Super><Return> ##\nbindsym x exec $term\nbindsym x ignored\n",
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].exec.as_deref(), Some("exec fixture-terminal "));
    }

    #[test]
    fn parser_replicates_missing_value_whole_line_insert() {
        // Replicated reference bug: a 3-token set_from_resource line has
        // no third space, so the reference inserts the whole line as the
        // value. An empty key still inserts nothing.
        let parsed = parse_config(
            "set_from_resource $k res\n## Cat // Uses missing value // k ##\nbindsym k exec $k end\n",
        )
        .unwrap();
        assert_eq!(
            parsed[0].exec.as_deref(),
            Some("exec set_from_resource $k res end ")
        );
        let parsed = parse_config(
            "set_from_resource  res ignored\n## Cat // Uses empty key // e ##\nbindsym e exec a  b\n",
        )
        .unwrap();
        assert_eq!(parsed[0].exec.as_deref(), Some("exec a  b "));
    }

    #[test]
    fn parser_orders_categories_like_vala_tree_map() {
        let parsed = parse_config(
            "## Zed // First // z ##\nbindsym z exec z\n## Alpha // Second // a ##\nbindsym a exec a\n",
        )
        .unwrap();
        assert_eq!(parsed[0].category, "Alpha");
        assert_eq!(parsed[1].category, "Zed");
    }

    #[test]
    fn format_and_wm_commands_match_vala() {
        assert_eq!(format_spec("<Super>  <Return>"), "Super  Return ");
        assert_eq!(wm_cli("i3"), Some("/usr/bin/i3-msg "));
        assert_eq!(wm_cli("sway"), Some("/usr/bin/swaymsg "));
        assert_eq!(wm_cli("Unknown"), None);
        assert_eq!(
            keybinding_commandline("i3", "exec fixture ").as_deref(),
            Some("/usr/bin/i3-msg \"exec fixture \"")
        );
    }
}
