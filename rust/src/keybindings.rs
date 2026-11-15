use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::filter::fold_text;
use crate::ipc::{self, Config};
use crate::pages::{follow_fuzzy_setting, select_first_row, DialogPage, RowFilter, FUZZY_MATCHING};

const SPEC: u32 = 0;
const SUMMARY: u32 = 1;
const EXEC: u32 = 2;
const FOLDED_SPEC: u32 = 3;
const FOLDED_SUMMARY: u32 = 4;

const TRAWL_BUS_NAME: &str = "org.regolith.Trawl";
const TRAWL_OBJECT_PATH: &str = "/org/regolith/Trawl";
const TRAWL_INTERFACE: &str = "org.regolith.trawl1";
const DBUS_TIMEOUT_MS: i32 = 1000;

#[derive(Clone, Debug, PartialEq)]
struct Keybinding {
    category: String,
    label: String,
    spec: String,
    exec: Option<String>,
}

pub struct KeybindingsPage {
    entry: gtk::Entry,
    model: gtk::ListStore,
    row_filter: Rc<RowFilter>,
    filter: gtk::TreeModelFilter,
    view: gtk::TreeView,
    root: gtk::Widget,
    path: Rc<RefCell<gtk::TreePath>>,
    wm_name: String,
    quit: Rc<dyn Fn()>,
    /// Set once a row has run: the launcher closes after the first choice.
    ran: Rc<Cell<bool>>,
}

impl KeybindingsPage {
    pub fn new(
        settings: gio::Settings,
        entry: gtk::Entry,
        wm_name: String,
        quit: Rc<dyn Fn()>,
    ) -> Self {
        let model = gtk::ListStore::new(&[
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
            String::static_type(),
        ]);
        // Each word may match the description or the key text, by the word
        // rule or by subsequence.
        let row_filter = RowFilter::new(
            &[FOLDED_SUMMARY, FOLDED_SPEC],
            settings.boolean(FUZZY_MATCHING),
        );
        let filter = gtk::TreeModelFilter::new(&model, None);
        let row_filter_for_filter = row_filter.clone();
        filter.set_visible_func(move |tree_model, iter| {
            row_filter_for_filter.row_match(tree_model, iter).is_some()
        });

        let bindings = ipc::get_config(&wm_name).and_then(|config| {
            let lines = config_lines(config);
            parse_config(lines.iter().map(String::as_str))
        });
        match bindings {
            Ok(bindings) => {
                for binding in bindings {
                    let row = model.append();
                    let spec = format_spec(&binding.spec);
                    let summary = format!("{} - {}", binding.category, binding.label);
                    let exec = binding.exec.unwrap_or_default();
                    model.set(
                        &row,
                        &[
                            (SPEC, &spec),
                            (SUMMARY, &summary),
                            (EXEC, &exec),
                            (FOLDED_SPEC, &fold_text(&spec)),
                            (FOLDED_SUMMARY, &fold_text(&summary)),
                        ],
                    );
                }
            }
            Err(error) => eprintln!("Failed to read config from {wm_name}: {error}"),
        }
        row_filter.sort(&model, SUMMARY);
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

        let path = Rc::new(RefCell::new(gtk::TreePath::new_first()));
        follow_fuzzy_setting(
            &settings,
            &row_filter,
            &model,
            SUMMARY,
            &filter,
            &view,
            &path,
        );
        let page = Self {
            entry,
            model,
            row_filter,
            filter,
            view,
            root: scrolled.upcast(),
            path: path.clone(),
            wm_name,
            quit,
            ran: Rc::new(Cell::new(false)),
        };
        page.connect_activation();
        page.set_selection();
        page
    }

    fn connect_activation(&self) {
        let wm_name = self.wm_name.clone();
        let quit = self.quit.clone();
        let ran = self.ran.clone();
        self.view.connect_row_activated(move |view, path, _| {
            if let Some(exec) = row_command(view, path) {
                execute(&wm_name, &exec, &quit, &ran);
            }
        });
        // Under the X11 input grab the view does not activate a clicked row
        // itself.
        let wm_name = self.wm_name.clone();
        let quit = self.quit.clone();
        let ran = self.ran.clone();
        self.view.connect_button_release_event(move |view, event| {
            if let Some(exec) = clicked_row(view, event).and_then(|path| row_command(view, &path)) {
                execute(&wm_name, &exec, &quit, &ran);
            }
            glib::Propagation::Proceed
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
        execute(&self.wm_name, &exec, &self.quit, &self.ran);
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
        self.row_filter.set_text(self.entry.text().as_str());
        self.row_filter.sort(&self.model, SUMMARY);
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

/// The command of the row at `path` in the view, if the row has one.
fn row_command(view: &gtk::TreeView, path: &gtk::TreePath) -> Option<String> {
    let model = view.model()?;
    let iter = model.iter(path)?;
    Some(string_value(&model, &iter, EXEC as i32)).filter(|command| !command.is_empty())
}

/// The row under the pointer when the primary button is released with
/// neither Shift nor Ctrl held, whichever row is highlighted.
fn clicked_row(view: &gtk::TreeView, event: &gdk::EventButton) -> Option<gtk::TreePath> {
    let modifiers = gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK;
    if event.button() != gdk::BUTTON_PRIMARY || event.state().intersects(modifiers) {
        return None;
    }
    let rows = view.bin_window()?;
    // The position is relative to the window the event came to, which under
    // the grab need not be the one the rows are drawn in.
    let (x, y) = if event.window().as_ref() == Some(&rows) {
        event.position()
    } else {
        let (_, origin_x, origin_y) = rows.origin();
        let (root_x, root_y) = event.root();
        (root_x - f64::from(origin_x), root_y - f64::from(origin_y))
    };
    view.path_at_pos(x as i32, y as i32)
        .and_then(|(path, ..)| path)
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

/// Runs `exec` in the window manager over IPC, once: the launcher closes
/// after the first choice, also when the window manager reports that the
/// command failed.
fn execute(wm_name: &str, exec: &str, quit: &Rc<dyn Fn()>, ran: &Cell<bool>) {
    // One click can choose a row twice: through the view's own activation
    // and on the button release.
    if ran.replace(true) {
        return;
    }
    if let Err(error) = ipc::run_command(wm_name, exec) {
        eprintln!("Error: execute_keybinding failed: {exec}: {error}");
    }
    quit();
}

/// The lines of the configuration, each binding with its command as the
/// window manager holds it.
fn config_lines(config: Config) -> Vec<String> {
    match config {
        // i3 has applied its variables and X resources already. Each file is
        // split on its own, so a file without a final newline does not hide
        // the first line of the next.
        Config::I3 { files } => files
            .iter()
            .flat_map(|file| file.lines())
            .map(str::to_owned)
            .collect(),
        Config::Sway { main, path } => {
            ipc::walk_included_configs(&main, path.as_deref(), &service_resources())
        }
    }
}

/// The values `set_from_resource` reads under sway: those of the desktop's
/// resource service on the session bus. Empty when the service is not
/// running, in which case every `set_from_resource` takes its default.
fn service_resources() -> HashMap<String, String> {
    gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .and_then(|bus| {
            bus.call_sync(
                Some(TRAWL_BUS_NAME),
                TRAWL_OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&(TRAWL_INTERFACE, "Resources").to_variant()),
                Some(glib::VariantTy::new("(v)").expect("valid type string")),
                gio::DBusCallFlags::NO_AUTO_START,
                DBUS_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
        })
        .ok()
        .and_then(|reply| reply.child_value(0).as_variant())
        .and_then(|resources| resources.get::<HashMap<String, String>>())
        .unwrap_or_default()
}

/// The documented bindings: a `## category // label // keys ##` comment and
/// the command of the first `bindsym` line after it, if any.
fn parse_config<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Vec<Keybinding>, String> {
    let mut bindings = Vec::new();
    let mut pending = None;
    for untrimmed in lines {
        let line = untrimmed.trim();
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
        } else if let Some(keys) = untrimmed
            .trim_start()
            .strip_prefix("bindsym")
            .filter(|rest| rest.starts_with(char::is_whitespace))
        {
            if let Some(index) = pending.take() {
                // The command is the text after the options, which start
                // with `--`, and the keys, as it is.
                let mut rest = keys.trim_start();
                loop {
                    let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    rest = after.trim_start();
                    if !word.starts_with("--") {
                        break;
                    }
                }
                bindings[index].exec = Some(rest.to_owned()).filter(|command| !command.is_empty());
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
    use super::{config_lines, format_spec, parse_config, wm_cli, Config, Keybinding};
    use crate::filter::{fold_text, match_fields, query_words, Match};

    fn parse(text: &str) -> Vec<Keybinding> {
        parse_config(text.lines()).unwrap()
    }

    #[test]
    fn a_row_takes_the_first_binding_after_it() {
        let parsed = parse(
            "bindsym x before\n## Apps // Open // <Super><Return> ##\nset $a b\nbindsym x exec $a\nbindsym x ignored\n",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].exec.as_deref(), Some("exec $a"));
        // A row followed directly by the next row has no binding.
        let parsed = parse("## A // One // a ##\n## A // Two // b ##\nbindsym b exec two\n");
        assert_eq!(parsed[0].exec, None);
        assert_eq!(parsed[1].exec.as_deref(), Some("exec two"));
    }

    #[test]
    fn the_command_is_the_text_after_options_and_keys() {
        // The command of the row whose binding is `line`.
        let command = |line: &str| {
            parse(&format!("## A // B // c ##\n{line}\n"))[0]
                .exec
                .clone()
        };
        assert_eq!(
            command("  bindsym --release --to-code $mod+x   exec  a \"b  c\"\t ").as_deref(),
            Some("exec  a \"b  c\"\t ")
        );
        assert_eq!(
            command("bindsym\tMod4+q\t[class=\"x  y\"] kill").as_deref(),
            Some("[class=\"x  y\"] kill")
        );
        // An empty command runs nothing.
        assert_eq!(command("bindsym x"), None);
        assert_eq!(command("bindsym --release"), None);
        assert_eq!(command("bindsym x  "), None);
        // Other lines are no binding: the row takes the next one.
        for other in ["bindsymx a b", "bindcode 10 exec a", "# bindsym x exec a"] {
            assert_eq!(
                command(&format!("{other}\nbindsym y exec next")).as_deref(),
                Some("exec next")
            );
        }
    }

    #[test]
    fn i3_files_do_not_run_into_each_other() {
        let lines = config_lines(Config::I3 {
            files: vec![
                "## A // One // a ##\nbindsym a exec one".to_owned(),
                "## A // Two // b ##\nbindsym b exec two\n".to_owned(),
            ],
        });
        let parsed = parse_config(lines.iter().map(String::as_str)).unwrap();
        let commands: Vec<_> = parsed
            .iter()
            .map(|binding| binding.exec.as_deref())
            .collect();
        assert_eq!(commands, [Some("exec one"), Some("exec two")]);
    }

    #[test]
    fn parser_orders_categories_like_vala_tree_map() {
        let parsed = parse(
            "## Zed // First // z ##\nbindsym z exec z\n## Alpha // Second // a ##\nbindsym a exec a\n",
        );
        assert_eq!(parsed[0].category, "Alpha");
        assert_eq!(parsed[1].category, "Zed");
    }

    #[test]
    fn rows_match_the_folded_description_or_key_text() {
        let parsed = parse(
            "## Café // Ouvrir le Navigateur // <Super><Shift><Return> ##\nbindsym $mod+Shift+Return exec browser\n",
        );
        // The row's searchable fields, as the page folds them.
        let summary = fold_text(&format!("{} - {}", parsed[0].category, parsed[0].label));
        let spec = fold_text(&format_spec(&parsed[0].spec));
        assert_eq!(summary, "cafe - ouvrir le navigateur");
        assert_eq!(spec, "super shift return ");
        let matching = |query: &str, subsequence: bool| {
            match_fields(&query_words(query), &[&summary, &spec], subsequence)
        };
        assert_eq!(matching("CAFÉ navigateur", false), Some(Match::Words));
        // Words may come from the description and the key text.
        assert_eq!(matching("cafe RETURN", false), Some(Match::Words));
        assert_eq!(matching("shrt", false), None);
        // "shrt" spans "shift ret" in the key text.
        assert_eq!(matching("shrt", true), Some(Match::Subsequence(9)));
        // Each word takes its shortest span from its own best field...
        assert_eq!(matching("cfe shrt", true), Some(Match::Subsequence(4 + 9)));
        // ...but one word never joins the two fields.
        assert_eq!(matching("nvshrt", true), None);
    }

    #[test]
    fn format_and_wm_commands_match_vala() {
        assert_eq!(format_spec("<Super>  <Return>"), "Super  Return ");
        assert_eq!(wm_cli("i3"), Some("/usr/bin/i3-msg "));
        assert_eq!(wm_cli("sway"), Some("/usr/bin/swaymsg "));
        assert_eq!(wm_cli("Unknown"), None);
    }
}
