use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gio::prelude::*;
use gtk::gdk;
use gtk::prelude::*;

use crate::filter::fold_text;
use crate::ipc::{self, Config};
use crate::pages::{follow_fuzzy_setting, select_first_row, DialogPage, RowFilter, FUZZY_MATCHING};
use crate::xkb;

const SPEC: u32 = 0;
const SUMMARY: u32 = 1;
const EXEC: u32 = 2;
const FOLDED_SPEC: u32 = 3;
const FOLDED_SUMMARY: u32 = 4;

const TRAWL_BUS_NAME: &str = "org.regolith.Trawl";
const TRAWL_OBJECT_PATH: &str = "/org/regolith/Trawl";
const TRAWL_INTERFACE: &str = "org.regolith.trawl1";
const DBUS_TIMEOUT_MS: i32 = 1000;

/// The mark the page shows for the Super modifier.
const SUPER_MARK: &str = "\u{f17a}";

/// A documented binding: a `## category // label // keys ##` comment and the
/// binding lines that belong to it.
#[derive(Clone, Debug, PartialEq)]
struct Keybinding {
    category: String,
    label: String,
    /// The key text of the comment, as written.
    spec: String,
    /// The command of the first binding, as the window manager holds it.
    exec: Option<String>,
    bindings: Vec<Binding>,
}

/// One `bindsym` or `bindcode` line of a documented binding.
#[derive(Clone, Debug, PartialEq)]
struct Binding {
    /// Whether the line is a `bindcode`, whose key is a key code.
    code: bool,
    /// The modifiers, as written and in the order written.
    modifiers: Vec<String>,
    /// The key, as written.
    key: String,
    /// Whether a later line took the keys away again (sway): it bound them
    /// to something else or unbound them.
    removed: bool,
}

/// A displayed key combination: its modifiers and its key.
type Combination = (Vec<String>, String);

/// How sway reads binding lines, which i3 does not: a later binding of the
/// same keys takes an earlier one's place, an unbinding takes it away, and
/// `bindsym --to-code` keys become key codes through `keymap`, the first
/// layout the configuration declares for keyboards (`None` when libxkbcommon
/// cannot build it).
#[derive(Clone, Copy)]
struct SwayRules<'a> {
    keymap: Option<&'a xkb::Keymap>,
}

/// What tells two key bindings apart for sway: a later binding that agrees in
/// all of this takes the earlier one's place, and `unbindsym` or
/// `unbindcode` with the same takes it away. A `bindsym --to-code` whose keys
/// sway converted is a key code binding here, with the codes as its keys.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BindingId {
    code: bool,
    mode: String,
    /// The options that set a binding apart, as written.
    flags: BTreeSet<String>,
    /// The device `--input-device` names, `*` for every device.
    device: String,
    /// The modifiers, by their X11 names.
    modifiers: BTreeSet<&'static str>,
    keys: BTreeSet<KeyId>,
}

/// A key as sway identifies it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum KeyId {
    /// The keysym of a keysym binding, or the key code of a code binding.
    Number(u32),
    /// A key name that gives neither, lower-cased.
    Unknown(String),
}

/// A `bindsym`, `bindcode`, `unbindsym` or `unbindcode` line, read.
struct BindingLine {
    binding: Binding,
    /// The command, as the window manager holds it.
    exec: Option<String>,
    /// What identifies the binding for sway, when sway's rules apply.
    id: Option<BindingId>,
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

        let bindings = ipc::get_config(&wm_name).and_then(page_rows);
        match bindings {
            Ok(bindings) => {
                for (spec, binding) in bindings {
                    let row = model.append();
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
    fn get_keybinding(&self) -> Option<char> {
        Some('k')
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

/// The rows of the page: the documented bindings, each with the key text it
/// shows.
///
/// A row shows its comment's key text when it has no binding, and when its
/// bindings are the key combinations they would be with no resource set.
/// Otherwise it shows the keys it is bound to. i3 reports each file both as
/// written and as it bound it, resources included; under sway the
/// configuration is read twice, with the resource service's values and with
/// none.
fn page_rows(config: Config) -> Result<Vec<(String, Keybinding)>, String> {
    let (bindings, unset, code_keys) = match config {
        Config::I3 { files, written } => {
            let bindings = parse_files(files.iter().map(|file| file.lines()), None)?;
            // Without the files as written there is nothing to compare with,
            // and every row keeps its comment text.
            let unset = match written {
                Some(written) => {
                    let replaced = ipc::replace_i3_variables(&written);
                    Some(parse_files(replaced.iter().map(|file| file.lines()), None)?)
                }
                None => None,
            };
            (bindings, unset, CodeKeys::Display)
        }
        Config::Sway { main, path } => {
            let read = |resources: &HashMap<String, String>| {
                let files = ipc::read_config_files(&main, path.as_deref(), resources);
                let keymap = xkb::Keymap::new(&xkb_names(&files));
                let rules = SwayRules {
                    keymap: keymap.as_ref(),
                };
                let bindings = parse_files(
                    files.iter().map(|file| file.iter().map(String::as_str)),
                    Some(rules),
                );
                (bindings, keymap)
            };
            let (bindings, keymap) = read(&service_resources());
            let (unset, _) = read(&HashMap::new());
            (bindings?, Some(unset?), CodeKeys::Xkb(keymap))
        }
    };
    let code_keys = code_keys.resolve(bindings.iter().chain(unset.iter().flatten()));
    let mut unset_rows: HashMap<(&str, &str), Vec<&Keybinding>> = HashMap::new();
    for binding in unset.iter().flatten() {
        unset_rows
            .entry((&binding.category, &binding.label))
            .or_default()
            .push(binding);
    }
    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    let mut rows = Vec::with_capacity(bindings.len());
    for binding in &bindings {
        let title = (binding.category.as_str(), binding.label.as_str());
        let occurrence = seen.entry(title).or_default();
        let text = match &unset {
            None => format_spec(&binding.spec),
            // A row that only the resources bring in has no default.
            Some(_) => {
                let default = unset_rows
                    .get(&title)
                    .and_then(|rows| rows.get(*occurrence))
                    .copied();
                row_text(binding, default, &code_keys)
            }
        };
        *occurrence += 1;
        rows.push(text);
    }
    Ok(rows.into_iter().zip(bindings).collect())
}

/// The key text of a row: the comment's when the row has no binding or is
/// bound as it is with no resource set (`default`), else its bound keys. A
/// row that lost keys to a later line shows the keys it has left.
fn row_text(
    binding: &Keybinding,
    default: Option<&Keybinding>,
    code_keys: &HashMap<u32, String>,
) -> String {
    if binding.bindings.is_empty() {
        return format_spec(&binding.spec);
    }
    let shown: Vec<_> = binding
        .bindings
        .iter()
        .map(|bound| shown_keys(bound, code_keys))
        .collect();
    if binding.bindings.iter().any(|bound| bound.removed) {
        let left = binding
            .bindings
            .iter()
            .zip(shown)
            .filter(|(bound, _)| !bound.removed)
            .map(|(_, shown)| shown);
        return format_keys(left);
    }
    if let Some(default) = default {
        let unset: Vec<_> = default
            .bindings
            .iter()
            .map(|bound| shown_keys(bound, code_keys))
            .collect();
        let same = shown.len() == unset.len()
            && shown
                .iter()
                .zip(&unset)
                .all(|(left, right)| same_combination(left, right));
        if same {
            return format_spec(&binding.spec);
        }
    }
    format_keys(shown)
}

/// Whether two displayed combinations are the same keys: spelling, case and
/// the order the modifiers are written in aside, the same modifiers and the
/// same key.
fn same_combination(left: &Combination, right: &Combination) -> bool {
    let fold = |modifiers: &[String]| -> BTreeSet<String> {
        modifiers
            .iter()
            .map(|modifier| modifier.to_lowercase())
            .collect()
    };
    fold(&left.0) == fold(&right.0) && left.1.to_lowercase() == right.1.to_lowercase()
}

/// A row's displayed combinations, each shown once: when they all have the
/// same modifiers, the modifiers once and then every key, else every
/// combination in full, joined by "or".
fn format_keys(shown: impl IntoIterator<Item = Combination>) -> String {
    let mut distinct: Vec<Combination> = Vec::new();
    for combination in shown {
        if !distinct.contains(&combination) {
            distinct.push(combination);
        }
    }
    let Some((first, _)) = distinct.first() else {
        return String::new();
    };
    if distinct.iter().all(|(modifiers, _)| modifiers == first) {
        let keys = distinct.iter().map(|(_, key)| key.as_str());
        return first
            .iter()
            .map(String::as_str)
            .chain(keys)
            .collect::<Vec<_>>()
            .join(" ");
    }
    distinct
        .iter()
        .map(|(modifiers, key)| {
            let mut words: Vec<&str> = modifiers.iter().map(String::as_str).collect();
            words.push(key);
            words.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The displayed modifiers and key of one binding. Each modifier is shown
/// once, in the order written; a key code shows the key it gives.
fn shown_keys(binding: &Binding, code_keys: &HashMap<u32, String>) -> Combination {
    let mut modifiers: Vec<String> = Vec::new();
    for modifier in &binding.modifiers {
        let shown = modifier_text(modifier);
        if !modifiers
            .iter()
            .any(|known| known.to_lowercase() == shown.to_lowercase())
        {
            modifiers.push(shown);
        }
    }
    let key = if binding.code {
        binding
            .key
            .parse::<u32>()
            .ok()
            .and_then(|code| code_keys.get(&code))
            .map_or_else(|| binding.key.clone(), |name| key_text(name))
    } else {
        key_text(&binding.key)
    };
    (modifiers, key)
}

/// The modifier `name` stands for, by its X11 name, as i3 and sway read
/// modifier names: without regard to case.
fn modifier(name: &str) -> Option<&'static str> {
    Some(match name.to_lowercase().as_str() {
        "mod4" | "super" | "logo" => "Mod4",
        "mod1" | "alt" => "Mod1",
        "control" | "ctrl" => "Control",
        "shift" => "Shift",
        "lock" | "caps" => "Lock",
        "mod2" => "Mod2",
        "mod3" => "Mod3",
        "mod5" => "Mod5",
        _ => return None,
    })
}

/// How a modifier is shown; its name is recognised without regard to case.
fn modifier_text(name: &str) -> String {
    match modifier(name) {
        Some("Mod4") => SUPER_MARK,
        Some("Mod1") => "Alt",
        Some("Control") => "Ctrl",
        Some("Shift") => "Shift",
        _ => name,
    }
    .to_owned()
}

/// How a key is shown; its name is recognised without regard to case. A
/// key name that types a visible character, keypad keys aside, shows the
/// character.
fn key_text(name: &str) -> String {
    let shown = match name.to_lowercase().as_str() {
        "return" => "Enter",
        "space" => "Space",
        "left" => "\u{2190}",
        "right" => "\u{2192}",
        "up" => "\u{2191}",
        "down" => "\u{2193}",
        "question" => "?",
        "grave" => "`",
        "period" => ".",
        "plus" => "+",
        "minus" => "-",
        "backspace" => "Backspace",
        _ => {
            let keypad = name
                .get(..3)
                .is_some_and(|start| start.eq_ignore_ascii_case("kp_"));
            if name.chars().nth(1).is_none() || keypad {
                return name.to_owned();
            }
            return xkb::keysym(name)
                .and_then(|keysym| gdk::keys::Key::from(keysym).to_unicode())
                .filter(|character| !character.is_control() && !character.is_whitespace())
                .map_or_else(|| name.to_owned(), String::from);
        }
    };
    shown.to_owned()
}

/// Where the keys of key codes come from.
enum CodeKeys {
    /// The display's keymap: X11 under i3.
    Display,
    /// The keymap sway builds from the configuration's settings, when it
    /// could be built.
    Xkb(Option<xkb::Keymap>),
}

impl CodeKeys {
    /// The name of the key each code of `bindings` gives in the first
    /// layout with no modifier held; codes that give none are left out.
    fn resolve<'a>(&self, bindings: impl Iterator<Item = &'a Keybinding>) -> HashMap<u32, String> {
        let codes: BTreeSet<u32> = bindings
            .flat_map(|binding| &binding.bindings)
            .filter(|bound| bound.code)
            .filter_map(|bound| bound.key.parse().ok())
            .collect();
        if codes.is_empty() {
            return HashMap::new();
        }
        let keysym: Box<dyn Fn(u32) -> Option<u32> + '_> = match self {
            Self::Display => {
                let keymap =
                    gdk::Display::default().and_then(|display| gdk::Keymap::for_display(&display));
                Box::new(move |code| {
                    keymap
                        .as_ref()?
                        .entries_for_keycode(code)
                        .into_iter()
                        .find_map(|(key, keyval)| {
                            (key.group() == 0 && key.level() == 0).then_some(keyval)
                        })
                })
            }
            Self::Xkb(keymap) => Box::new(|code| keymap.as_ref()?.first_keysym(code)),
        };
        codes
            .into_iter()
            .filter_map(|code| {
                let name = gdk::keys::Key::from(keysym(code)?).name()?;
                Some((code, name.to_string()))
            })
            .collect()
    }
}

/// The keymap settings sway takes from `input` lines for every keyboard
/// (`*` or `type:keyboard`), in either the one-line or the block form.
fn xkb_names(files: &[Vec<String>]) -> xkb::Names {
    let mut names = xkb::Names::default();
    let mut setting = |words: &[&str]| {
        let [name, value, ..] = words else {
            return;
        };
        let value = Some(unquote(value).to_owned());
        match *name {
            "xkb_rules" => names.rules = value,
            "xkb_model" => names.model = value,
            "xkb_layout" => names.layout = value,
            "xkb_variant" => names.variant = value,
            "xkb_options" => names.options = value,
            _ => {}
        }
    };
    for file in files {
        // Inside an `input` block: whether it is for every keyboard.
        let mut block = None;
        for line in file {
            let words = ipc::split_words(line);
            match words.as_slice() {
                ["input", identifier, rest @ ..] if !rest.is_empty() => {
                    let every = matches!(unquote(identifier), "*" | "type:keyboard");
                    if rest == ["{"] {
                        block = Some(every);
                    } else if every {
                        setting(rest);
                    }
                }
                ["}", ..] => block = None,
                words if block == Some(true) => setting(words),
                _ => {}
            }
        }
    }
    names
}

fn unquote(word: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = word
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    word
}

/// The values `set_from_resource` reads under sway: those of the desktop's
/// resource service on the session bus. Empty when the service is not
/// running, in which case every `set_from_resource` takes its default.
pub fn service_resources() -> HashMap<String, String> {
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

/// The documented bindings of one text.
#[cfg(test)]
fn parse_config<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Vec<Keybinding>, String> {
    parse_files([lines], None)
}

/// The documented bindings of the configuration's files, in reading order.
///
/// A `## category // label // keys ##` comment starts a row, which takes the
/// `bindsym` and `bindcode` lines after it. `set` and `set_from_resource`
/// lines and other comments do not end it; a blank line, any other line and
/// the end of the file do. Comments inside a block form rows of their own
/// from the lines in the block. A comment before a `mode` block, with at
/// most `set` lines and other comments between them, instead takes the first
/// binding after the block, at its own level. With `sway`'s rules, a later
/// binding of the same keys (same mode, same distinguishing options) or an
/// unbinding takes the keys away from a row.
fn parse_files<'a, L>(
    files: impl IntoIterator<Item = L>,
    sway: Option<SwayRules>,
) -> Result<Vec<Keybinding>, String>
where
    L: IntoIterator<Item = &'a str>,
{
    let mut bindings: Vec<Keybinding> = Vec::new();
    // The binding that holds each key set, if it belongs to a row.
    let mut holders: HashMap<BindingId, Option<(usize, usize)>> = HashMap::new();
    for lines in files {
        // The row taking bindings: its index, and whether it only takes one
        // more binding (after its block).
        let mut open: Option<(usize, bool)> = None;
        // Rows whose comment stood before a mode block, with the depth their
        // comment is at.
        let mut waiting: Vec<(usize, usize)> = Vec::new();
        // The mode (or other block) each open block stands for.
        let mut blocks: Vec<String> = Vec::new();
        for untrimmed in lines {
            let line = untrimmed.trim();
            if let Some(binding) = row_comment(line)? {
                bindings.push(binding);
                open = Some((bindings.len() - 1, false));
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let words = ipc::split_words(line);
            match words.first().copied() {
                None => open = None,
                // The window managers read these keywords without regard to
                // case.
                Some(keyword)
                    if keyword.eq_ignore_ascii_case("set")
                        || keyword.eq_ignore_ascii_case("set_from_resource") => {}
                Some("bindsym" | "bindcode") => {
                    let line = binding_line(&words, untrimmed, &blocks, sway);
                    let position = open.map(|(row, ..)| (row, bindings[row].bindings.len()));
                    if let Some((row, index)) = position {
                        if index == 0 {
                            bindings[row].exec = line.exec;
                        }
                        bindings[row].bindings.push(line.binding);
                    }
                    let replaced = line.id.and_then(|id| holders.insert(id, position));
                    if let Some(Some((row, index))) = replaced {
                        bindings[row].bindings[index].removed = true;
                    }
                    if let Some((_, true)) = open {
                        open = None;
                    }
                }
                Some("unbindsym" | "unbindcode") => {
                    open = None;
                    let line = binding_line(&words, untrimmed, &blocks, sway);
                    if let Some(Some((row, index))) = line.id.and_then(|id| holders.remove(&id)) {
                        bindings[row].bindings[index].removed = true;
                    }
                }
                Some(keyword) if line.ends_with('{') => {
                    // A row that has no binding yet waits for the block to
                    // end: only lines that do not end a row came between.
                    if let (Some((row, _)), "mode") = (open, keyword) {
                        if bindings[row].bindings.is_empty() {
                            waiting.push((row, blocks.len()));
                        }
                    }
                    open = None;
                    blocks.push(block_name(&words));
                }
                Some(_) if line.starts_with('}') => {
                    blocks.pop();
                    open = None;
                    if waiting
                        .last()
                        .is_some_and(|(_, depth)| *depth == blocks.len())
                    {
                        open = waiting.pop().map(|(row, _)| (row, true));
                    }
                }
                Some(_) => open = None,
            }
        }
    }
    // Gee.TreeMap yields categories in lexical order while preserving each
    // category's file order.
    bindings.sort_by(|left, right| left.category.cmp(&right.category));
    Ok(bindings)
}

/// The row a `## category // label // keys ##` comment starts, if `line` is
/// one.
fn row_comment(line: &str) -> Result<Option<Keybinding>, String> {
    if !(line.len() > 13
        && line.starts_with("##")
        && line.get(3..).is_some_and(|rest| rest.contains("##"))
        && line.contains("//"))
    {
        return Ok(None);
    }
    let Some(end) = line
        .get(3..)
        .and_then(|rest| rest.find("##"))
        .map(|i| i + 3)
    else {
        return Ok(None);
    };
    let parts: Vec<_> = line[2..end].split("//").map(str::trim).collect();
    if parts.len() != 3 {
        return Err(format!("Invalid line: {line}\n"));
    }
    Ok(Some(Keybinding {
        category: parts[0].to_owned(),
        label: parts[1].to_owned(),
        spec: parts[2].to_owned(),
        exec: None,
        bindings: Vec::new(),
    }))
}

/// A `bindsym`, `bindcode`, `unbindsym` or `unbindcode` line, given as its
/// words and as written: its binding, its command and, with `sway`'s rules,
/// what identifies it for sway. The keys are the first word after the
/// options, which start with `--`; the command is the text after the keys,
/// as written.
fn binding_line(
    words: &[&str],
    line: &str,
    blocks: &[String],
    sway: Option<SwayRules>,
) -> BindingLine {
    let code = words[0].ends_with("code");
    let options: Vec<&str> = words[1..]
        .iter()
        .copied()
        .take_while(|word| word.starts_with("--"))
        .collect();
    let keys = words.get(options.len() + 1).copied();
    let exec = keys
        .map(|keys| text_after(line, keys).to_owned())
        .filter(|command| !command.is_empty());
    let keys = keys.unwrap_or_default();
    let mut parts: Vec<&str> = keys.split('+').collect();
    let key = parts.pop().unwrap_or_default().to_owned();
    let binding = Binding {
        code,
        modifiers: parts.iter().map(|part| (*part).to_owned()).collect(),
        key,
        removed: false,
    };
    let id = sway.map(|sway| binding_id(code, &options, keys, blocks, sway));
    BindingLine { binding, exec, id }
}

/// What identifies a binding for sway: its keys and options as sway reads
/// them, in the innermost of `blocks`.
fn binding_id(
    code: bool,
    options: &[&str],
    keys: &str,
    blocks: &[String],
    sway: SwayRules,
) -> BindingId {
    let mut flags = BTreeSet::new();
    let mut device = "*".to_owned();
    let mut to_code = false;
    for option in options {
        match *option {
            "--to-code" => to_code = true,
            // Options that do not set the binding apart from another.
            "--no-warn" | "--no-repeat" => {}
            // The last device named counts, without its quotes.
            _ => match option.strip_prefix("--input-device=") {
                Some(named) => device = unquote(named).to_owned(),
                None => {
                    flags.insert((*option).to_owned());
                }
            },
        }
    }
    // sway tells modifiers from keys by name; it reads key names without
    // regard to case, into keysyms, and key codes as numbers.
    let mut modifiers = BTreeSet::new();
    let mut names = Vec::new();
    for part in keys.split('+') {
        match modifier(part) {
            Some(modifier) => {
                modifiers.insert(modifier);
            }
            None => names.push(part),
        }
    }
    let key_id = |number: Option<u32>, name: &str| {
        number.map_or_else(|| KeyId::Unknown(name.to_lowercase()), KeyId::Number)
    };
    let (by_code, keys) = if code {
        let codes = names.iter().map(|name| key_id(name.parse().ok(), name));
        (true, codes.collect())
    } else {
        let keysyms: Vec<_> = names
            .iter()
            .map(|name| xkb::keysym_ignoring_case(name))
            .collect();
        // `--to-code` binds a keysym binding by key code when each of its
        // keys has exactly one code in the first layout, and leaves it a
        // keysym binding otherwise.
        let codes = sway.keymap.filter(|_| to_code).and_then(|keymap| {
            keysyms
                .iter()
                .map(|keysym| keymap.single_code((*keysym)?).map(KeyId::Number))
                .collect::<Option<BTreeSet<_>>>()
        });
        match codes {
            Some(codes) => (true, codes),
            None => {
                let keysyms = names.iter().zip(keysyms);
                (
                    false,
                    keysyms.map(|(name, keysym)| key_id(keysym, name)).collect(),
                )
            }
        }
    };
    BindingId {
        code: by_code,
        mode: blocks
            .last()
            .cloned()
            .unwrap_or_else(|| "default".to_owned()),
        flags,
        device,
        modifiers,
        keys,
    }
}

/// The text of `line` after `word`, one of the words `ipc::split_words`
/// found in it, without the blanks that follow the word.
fn text_after<'a>(line: &'a str, word: &str) -> &'a str {
    let start = (word.as_ptr() as usize).wrapping_sub(line.as_ptr() as usize);
    debug_assert!(line.get(start..start + word.len()) == Some(word));
    line.get(start + word.len()..)
        .unwrap_or_default()
        .trim_start()
}

/// What a block stands for: the name of a mode for `mode "name" {`, else its
/// opening line, set apart from mode names.
fn block_name(words: &[&str]) -> String {
    match words {
        ["mode", rest @ ..] => {
            let name = rest
                .iter()
                .find(|word| !word.starts_with("--") && **word != "{")
                .copied()
                .unwrap_or_default();
            unquote(name).to_owned()
        }
        _ => format!("\0{}", words.join(" ")),
    }
}

pub fn format_spec(raw: &str) -> String {
    raw.replace('<', "").replace('>', " ").replace("  ", " ")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        format_keys, format_spec, key_text, modifier_text, page_rows, parse_config, parse_files,
        row_text, wm_cli, xkb_names, Config, Keybinding, SwayRules, SUPER_MARK,
    };
    use crate::filter::{fold_text, match_fields, query_words, Match};
    use crate::xkb;

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
        // A key code binding is a binding too.
        assert_eq!(
            command("bindcode --release Mod4+52 exec a").as_deref(),
            Some("exec a")
        );
        // A comment is skipped; any other line ends the row.
        assert_eq!(
            command("# bindsym x exec a\nbindsym y exec next").as_deref(),
            Some("exec next")
        );
        assert_eq!(command("bindsymx a b\nbindsym y exec next"), None);
    }

    #[test]
    fn rows_take_their_own_lines() {
        let parsed = parse(concat!(
            "## A // Blank // a ##\nbindsym a exec a\n\nbindsym b exec b\n",
            "## A // Other // a ##\nbindsym a exec a\nfocus_follows_mouse no\nbindsym b exec b\n",
            "## A // Between // a ##\nbindsym a exec a\nset $x y\n# note\n",
            "set_from_resource $y y y\nbindcode 52 exec b\n",
        ));
        let keys = |binding: &Keybinding| -> Vec<String> {
            binding
                .bindings
                .iter()
                .map(|bound| bound.key.clone())
                .collect()
        };
        assert_eq!(keys(&parsed[0]), ["a"]);
        assert_eq!(keys(&parsed[1]), ["a"]);
        assert_eq!(keys(&parsed[2]), ["a", "52"]);
        assert!(parsed[2].bindings[1].code);
    }

    #[test]
    fn mode_blocks_hold_rows_and_the_row_before_takes_the_next_binding() {
        let parsed = parse(concat!(
            "## M // Enter // a ##\n",
            "mode \"m\" {\n",
            "    ## M // Inside // b ##\n",
            "    bindsym b mode default\n",
            "}\n",
            "bindsym $mod+e mode m\n",
            "bindsym $mod+f exec f\n",
            "## M // Set Lines Between // c ##\n",
            "set $x y\n",
            "# an ordinary comment\n",
            "set_from_resource $y wm.y 6\n",
            "mode \"n\" {\n",
            "}\n",
            "set $z n\n",
            "bindsym $mod+n mode n\n",
            "bindsym $mod+o exec o\n",
            "## M // Bound Before // d ##\n",
            "bindsym $mod+d exec d\n",
            "mode \"o\" {\n",
            "}\n",
            "bindsym $mod+p mode o\n",
            "## M // Blank Between // g ##\n",
            "\n",
            "mode \"p\" {\n",
            "}\n",
            "bindsym $mod+q mode p\n",
        ));
        let row = |label: &str| {
            parsed
                .iter()
                .find(|binding| binding.label == label)
                .unwrap()
        };
        assert_eq!(row("Enter").exec.as_deref(), Some("mode m"));
        assert_eq!(row("Enter").bindings.len(), 1);
        assert_eq!(row("Inside").exec.as_deref(), Some("mode default"));
        // Lines that do not end a row may stand between the comment and the
        // block; only the first binding after the block counts.
        assert_eq!(row("Set Lines Between").exec.as_deref(), Some("mode n"));
        assert_eq!(row("Set Lines Between").bindings.len(), 1);
        // A row that already has a binding, or that a blank line ended, does
        // not wait for the block.
        assert_eq!(row("Bound Before").exec.as_deref(), Some("exec d"));
        assert_eq!(row("Bound Before").bindings.len(), 1);
        assert!(row("Blank Between").bindings.is_empty());
        assert_eq!(row("Blank Between").exec, None);
    }

    #[test]
    fn sway_rebinding_and_unbinding_take_keys_away() {
        let text = concat!(
            "## A // Rebound // a ##\nbindsym $mod+a exec a\nbindsym Mod4+b exec b\n\n",
            "## A // Release // c ##\nbindsym --release c exec c\n\n",
            "## A // Mode // d ##\nbindsym d exec d\n\n",
            "## A // Code // e ##\nbindcode 52 exec e\n\n",
            "## A // To Code // f ##\nbindsym f exec f\n\n",
            "## A // Code Of F // 41 ##\nbindcode 41 exec g\n\n",
            "## A // Unconverted // question ##\nbindsym question exec q\n\n",
            "## A // No Repeat // h ##\nbindsym h exec h\n\n",
            "bindsym $MOD+A exec other\n",
            "bindsym --to-code f exec code\n",
            "bindsym --to-code question exec help\n",
            "bindsym --no-repeat --no-warn h exec other\n",
            "unbindsym super+B\n",
            "bindsym c exec c\n",
            "mode \"m\" {\n    bindsym d exec d\n}\n",
            "unbindcode 52\n",
        );
        // On a US keyboard f has the one code 41; question has none.
        let us = us_keymap();
        let removed = |sway: Option<SwayRules>| -> Vec<Vec<bool>> {
            parse_files([text.lines()], sway)
                .unwrap()
                .iter()
                .map(|binding| binding.bindings.iter().map(|bound| bound.removed).collect())
                .collect()
        };
        // A converted `--to-code` binding is a key code binding: it leaves
        // the keysym f alone and takes code 41's place; one that does not
        // convert stays a keysym binding and takes the keysym's place.
        assert_eq!(
            removed(Some(SwayRules { keymap: Some(&us) })),
            [
                vec![true, true],
                vec![false],
                vec![false],
                vec![true],
                vec![false],
                vec![true],
                vec![true],
                vec![true]
            ]
        );
        // i3 does not rebind.
        let mut untouched = vec![vec![false]; 8];
        untouched[0].push(false);
        assert_eq!(removed(None), untouched);
    }

    fn us_keymap() -> xkb::Keymap {
        xkb::Keymap::new(&xkb::Names {
            layout: Some("us".to_owned()),
            ..xkb::Names::default()
        })
        .unwrap()
    }

    #[test]
    fn sway_tells_bindings_apart_as_sway_does() {
        // Each pair is an earlier and a later line: whether the later one
        // takes the earlier one's keys.
        let us = us_keymap();
        let rules = SwayRules { keymap: Some(&us) };
        let replaces = |earlier: &str, later: &str| {
            let text = format!("## A // B // c ##\n{earlier} exec a\n{later} exec b\n");
            parse_files([text.lines()], Some(rules)).unwrap()[0].bindings[0].removed
        };
        // Names of one keysym, its number and its case are the same key.
        assert!(replaces("bindsym Mod4+Prior", "bindsym Mod4+Page_Up"));
        assert!(replaces("bindsym Mod4+y", "bindsym Mod4+0x79"));
        assert!(replaces(
            "bindsym Mod4+bracketleft",
            "bindsym Super+BRACKETLEFT"
        ));
        assert!(!replaces("bindsym Mod4+y", "bindsym Mod4+z"));
        // The device is compared as named, without quotes, the last one
        // counting.
        let device = "bindsym --input-device=1:1:Kbd Mod4+v";
        assert!(replaces(
            device,
            "bindsym --input-device=\"1:1:Kbd\" Mod4+v"
        ));
        assert!(!replaces(device, "bindsym --input-device=1:1:kbd Mod4+v"));
        assert!(!replaces(device, "bindsym Mod4+v"));
        assert!(replaces(
            device,
            "bindsym --input-device=1:1:Other --input-device=1:1:Kbd Mod4+v"
        ));
        // Options that set bindings apart, and those that do not.
        assert!(!replaces("bindsym Mod4+x", "bindsym --locked Mod4+x"));
        assert!(!replaces("bindsym Mod4+x", "bindsym --release Mod4+x"));
        assert!(replaces(
            "bindsym Mod4+x",
            "bindsym --no-repeat --no-warn Mod4+x"
        ));
        assert!(replaces("bindcode Mod4+28", "bindcode --to-code Mod4+28"));
        // A converted `--to-code` binding is one by code.
        assert!(replaces("bindcode Mod4+28", "bindsym --to-code Mod4+T"));
        assert!(!replaces("bindsym Mod4+t", "bindsym --to-code Mod4+t"));
        assert!(replaces(
            "bindsym Mod4+question",
            "bindsym --to-code Mod4+question"
        ));
        // Modes are apart.
        assert!(!replaces("bindsym x", "mode \"m\" {\nbindsym x"));
    }

    #[test]
    fn i3_files_do_not_run_into_each_other() {
        let rows = page_rows(Config::I3 {
            files: vec![
                "## A // One // a ##\nbindsym a exec one\n## A // Two // b ##".to_owned(),
                "bindsym b exec two\n## A // Three // c ##\nbindsym c exec three".to_owned(),
            ],
            written: None,
        })
        .unwrap();
        let commands: Vec<_> = rows
            .iter()
            .map(|(_, binding)| binding.exec.as_deref())
            .collect();
        assert_eq!(commands, [Some("exec one"), None, Some("exec three")]);
    }

    #[test]
    fn keys_are_shown_in_the_pages_words() {
        assert_eq!(modifier_text("mOd4"), SUPER_MARK);
        assert_eq!(modifier_text("Logo"), SUPER_MARK);
        assert_eq!(modifier_text("SUPER"), SUPER_MARK);
        assert_eq!(modifier_text("alt"), "Alt");
        assert_eq!(modifier_text("Mod1"), "Alt");
        assert_eq!(modifier_text("Control"), "Ctrl");
        assert_eq!(modifier_text("cTrL"), "Ctrl");
        assert_eq!(modifier_text("sHiFt"), "Shift");
        assert_eq!(modifier_text("Mod3"), "Mod3");
        for (name, shown) in [
            ("rEtUrN", "Enter"),
            ("space", "Space"),
            ("Left", "\u{2190}"),
            ("RIGHT", "\u{2192}"),
            ("up", "\u{2191}"),
            ("Down", "\u{2193}"),
            ("question", "?"),
            ("grave", "`"),
            ("period", "."),
            ("plus", "+"),
            ("minus", "-"),
            ("BackSpace", "Backspace"),
            ("bracketleft", "["),
            ("BRACKETLEFT", "["),
            ("odiaeresis", "\u{f6}"),
            ("Odiaeresis", "\u{d6}"),
            ("ODIAERESIS", "\u{f6}"),
            ("Tab", "Tab"),
            ("Escape", "Escape"),
            ("Delete", "Delete"),
            ("nobreakspace", "nobreakspace"),
            ("F9", "F9"),
            ("KP_Enter", "KP_Enter"),
            ("KP_Add", "KP_Add"),
            ("XF86AudioMute", "XF86AudioMute"),
            ("Prtscr", "Prtscr"),
            ("y", "y"),
            ("7", "7"),
        ] {
            assert_eq!(key_text(name), shown, "{name}");
        }
    }

    #[test]
    fn shared_modifiers_are_shown_once() {
        let combination = |modifiers: &[&str], key: &str| {
            (
                modifiers
                    .iter()
                    .map(|modifier| modifier.to_string())
                    .collect::<Vec<_>>(),
                key.to_owned(),
            )
        };
        assert_eq!(
            format_keys([
                combination(&["A", "B"], "y"),
                combination(&["A", "B"], "j"),
                combination(&["A", "B"], "y"),
            ]),
            "A B y j"
        );
        assert_eq!(
            format_keys([combination(&[], "Enter"), combination(&["A"], "x")]),
            "Enter or A x"
        );
        assert_eq!(
            format_keys([combination(&["A", "B"], "y"), combination(&["B", "A"], "v")]),
            "A B y or B A v"
        );
        assert_eq!(format_keys([]), "");
    }

    #[test]
    fn rows_at_their_defaults_keep_the_comment() {
        let row =
            |text: &str| parse(&format!("## A // B // <\u{f17a}> Enter ##\n{text}\n")).remove(0);
        let text = |current: &str, unset: &str| {
            row_text(&row(current), Some(&row(unset)), &HashMap::new())
        };
        let comment = format!("{SUPER_MARK} Enter");
        assert_eq!(
            text("bindsym Mod4+Return exec a", "bindsym Mod4+Return exec b"),
            comment
        );
        // Spelling and case alone change nothing.
        assert_eq!(
            text("bindsym super+rEtUrN exec a", "bindsym Mod4+Return exec a"),
            comment
        );
        assert_eq!(
            text("bindsym Ctrl+mod1+x exec a", "bindsym Control+Alt+X exec a"),
            comment
        );
        assert_eq!(
            text("bindsym Mod4+y exec a", "bindsym Mod4+Return exec a"),
            format!("{SUPER_MARK} y")
        );
        // So does the order the same modifiers are written in.
        assert_eq!(
            text("bindsym Mod1+Mod4+l exec a", "bindsym Mod4+Mod1+l exec a"),
            comment
        );
        assert_eq!(
            text("bindsym Mod1+Mod4+l exec a", "bindsym Mod4+Shift+l exec a"),
            format!("Alt {SUPER_MARK} l")
        );
        assert_eq!(
            text(
                "bindsym Mod2+Control+Ctrl+y exec a",
                "bindsym Mod2+Ctrl+F1 exec a"
            ),
            "Mod2 Ctrl y"
        );
        // A row with no binding keeps its comment.
        assert_eq!(text("", "bindsym x exec a"), comment);
        // A key code shows the key it gives, or its number.
        let codes: HashMap<u32, String> = [(52, "z".to_owned()), (36, "Return".to_owned())].into();
        let shown = row_text(
            &row(
                "bindsym Mod5+z exec a\nbindcode --release Mod5+52 exec a\nbindcode Mod5+8 exec b",
            ),
            Some(&row("bindsym Mod5+F1 exec a")),
            &codes,
        );
        assert_eq!(shown, "Mod5 z 8");
        // A row whose keys a later line took shows the keys it has left.
        let mut lost = row("bindsym Mod5+F20 exec a\nbindsym Mod5+F21 exec a");
        lost.bindings[0].removed = true;
        assert_eq!(row_text(&lost, Some(&lost.clone()), &codes), "Mod5 F21");
        lost.bindings[1].removed = true;
        assert_eq!(row_text(&lost, Some(&lost.clone()), &codes), "");
    }

    #[test]
    fn sway_rows_take_continued_bindings_whole() {
        let files = crate::ipc::read_config_files(
            "## A // Joined // a ##\nSET $x y\nbindsym a \\\n    exec echo 'one\\\n  two' $x\n",
            None,
            &HashMap::new(),
        );
        let parsed = parse_files(
            files.iter().map(|file| file.iter().map(String::as_str)),
            None,
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].bindings.len(), 1);
        assert_eq!(parsed[0].exec.as_deref(), Some("exec echo 'one  two' y"));
    }

    #[test]
    fn sway_keymap_settings_come_from_input_lines() {
        let files = vec![
            vec![
                "input type:touchpad xkb_layout fr".to_owned(),
                "input \"*\" xkb_layout de,us".to_owned(),
            ],
            vec![
                "input type:keyboard {".to_owned(),
                "xkb_variant nodeadkeys".to_owned(),
                "}".to_owned(),
                "input 1:1:x {".to_owned(),
                "xkb_layout gb".to_owned(),
                "}".to_owned(),
            ],
        ];
        assert_eq!(
            xkb_names(&files),
            xkb::Names {
                layout: Some("de,us".to_owned()),
                variant: Some("nodeadkeys".to_owned()),
                ..xkb::Names::default()
            }
        );
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
