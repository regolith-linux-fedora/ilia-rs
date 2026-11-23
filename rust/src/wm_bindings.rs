//! The window manager's keyboard bindings, read and matched as the window
//! manager reads and matches them.
//!
//! - i3: the bindings of its configuration, matched against a key press
//!   the way i3 matches its own key events (`bindings.c`): only the current
//!   binding mode counts; a `bindsym` key is looked up in the binding's layout
//!   (the first unless it names one, and then only while that layout is
//!   active) with the binding's modifiers, falling back to the key with Shift
//!   toggled, except on the keypad; a `bindcode` key by its code; the
//!   modifiers held must be the binding's, Caps Lock aside and Num Lock aside
//!   where it does not change the key.
//! - sway: whether a binding's command does nothing but switch the keyboard
//!   layout.

use gtk::gdk;

pub const SHIFT_MASK: u32 = 1 << 0;
pub const LOCK_MASK: u32 = 1 << 1;
const CONTROL_MASK: u32 = 1 << 2;
const MOD1_MASK: u32 = 1 << 3;
const MOD2_MASK: u32 = 1 << 4;
const MOD3_MASK: u32 = 1 << 5;
const MOD4_MASK: u32 = 1 << 6;
const MOD5_MASK: u32 = 1 << 7;
/// The real X modifiers, Shift to Mod5.
pub const REAL_MODIFIERS: u32 = 0xff;
/// The part of a key event's state i3 compares: the modifiers and mouse
/// buttons, without the layout bits.
pub const EVENT_STATE_MASK: u32 = 0x1fff;

const DEFAULT_MODE: &str = "default";

/// The key of a binding.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// `bindsym`: a key symbol.
    Symbol(u32),
    /// `bindcode`: an X key code.
    Code(u32),
}

/// One keyboard binding of an i3 configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    /// The binding mode it belongs to.
    pub mode: String,
    pub input: Input,
    /// Its modifiers, as real X modifiers.
    pub mods: u32,
    /// The layouts it is limited to, one bit per layout from the first
    /// (`Group1`); zero for every layout.
    pub groups: u32,
    /// `--release`: it runs when the key is released.
    pub release: bool,
    /// The command, as the configuration holds it.
    pub command: String,
}

impl Binding {
    /// The layout its key symbol is looked up in: the one it names, else the
    /// first. With several named, i3 takes the second, then the third, then
    /// the fourth.
    fn layout(&self) -> u32 {
        [(0b0010, 1), (0b0100, 2), (0b1000, 3)]
            .into_iter()
            .find(|(bit, _)| self.groups & bit != 0)
            .map_or(0, |(_, layout)| layout)
    }
}

/// The key a key code gives in a layout with some modifiers held, as the
/// keyboard map has it.
pub trait Keymap {
    /// The key symbol and its shift level (from 0), or `None` when the key
    /// gives no symbol there. `mods` are real X modifiers; `layout` counts
    /// from 0.
    fn translate(&self, keycode: u32, mods: u32, layout: u32) -> Option<(u32, i32)>;
}

impl Keymap for gdk::Keymap {
    fn translate(&self, keycode: u32, mods: u32, layout: u32) -> Option<(u32, i32)> {
        self.translate_keyboard_state(
            keycode,
            gdk::ModifierType::from_bits_truncate(mods),
            layout as i32,
        )
        .map(|(keyval, _, level, _)| (keyval, level))
    }
}

/// The keyboard bindings of i3's configuration files, in i3's reading
/// order, each with its variables already replaced (i3's
/// `variable_replaced_contents`).
pub fn parse_i3_config(files: &[String]) -> Vec<Binding> {
    let mut bindings = Vec::new();
    for text in files {
        parse_i3_file(text, &mut bindings);
    }
    bindings
}

fn parse_i3_file(text: &str, bindings: &mut Vec<Binding>) {
    // Bindings count at the top level and directly inside a mode block;
    // other blocks (`bar { }`) hold bindings of their own.
    let mut depth = 0usize;
    let mut mode: Option<String> = None;
    // A mode whose opening brace is on the next line.
    let mut awaiting_brace: Option<String> = None;
    for line in continued_lines(text) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = awaiting_brace.take() {
            if line == "{" {
                mode = Some(name);
                depth += 1;
                continue;
            }
        }
        // A command is the rest of its line, so a binding may end in a brace.
        if let Some(mut binding) = parse_binding(line) {
            if depth == 0 || (depth == 1 && mode.is_some()) {
                binding.mode = mode.clone().unwrap_or_else(|| DEFAULT_MODE.to_owned());
                bindings.push(binding);
            }
            continue;
        }
        if line.starts_with('}') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                mode = None;
            }
            continue;
        }
        if depth == 0 {
            if let Some((name, braced)) = parse_mode_line(line) {
                if braced {
                    mode = Some(name);
                    depth += 1;
                } else {
                    awaiting_brace = Some(name);
                }
                continue;
            }
        }
        if line.ends_with('{') {
            depth += 1;
        }
    }
}

/// The lines of a file, a line that ends in a backslash continuing on the
/// next.
fn continued_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(start) => current.push_str(start),
            None => {
                current.push_str(line);
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The next word of `text` from `position`, honouring double quotes, and the
/// position after it; `None` at the end. With `unquote` the quotes and the
/// backslashes escaping in them are dropped.
fn next_word(text: &str, position: usize, unquote: bool) -> Option<(String, usize)> {
    let rest = &text[position..];
    let start = position + (rest.len() - rest.trim_start().len());
    if start == text.len() {
        return None;
    }
    let mut word = String::new();
    let (mut quoted, mut escaped) = (false, false);
    let mut end = text.len();
    for (index, c) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            word.push(c);
        } else if quoted && c == '\\' {
            escaped = true;
            if !unquote {
                word.push(c);
            }
        } else if c == '"' {
            quoted = !quoted;
            if !unquote {
                word.push(c);
            }
        } else if !quoted && c.is_whitespace() {
            end = start + index;
            break;
        } else {
            word.push(c);
        }
    }
    Some((word, end))
}

/// `mode [--pango_markup] <name> [{]`: the mode's name and whether the
/// brace is on this line.
fn parse_mode_line(line: &str) -> Option<(String, bool)> {
    let (head, braced) = match line.strip_suffix('{') {
        Some(head) => (head.trim_end(), true),
        None => (line, false),
    };
    let (keyword, position) = next_word(head, 0, false)?;
    if keyword != "mode" {
        return None;
    }
    let (mut name, mut position) = next_word(head, position, true)?;
    if name == "--pango_markup" {
        (name, position) = next_word(head, position, true)?;
    }
    (!name.is_empty() && next_word(head, position, false).is_none()).then_some((name, braced))
}

/// `--release` and the options that do not change which key a binding is.
fn is_option(word: &str, release: &mut bool) -> bool {
    match word {
        "--release" => {
            *release = true;
            true
        }
        "--border" | "--whole-window" | "--exclude-titlebar" => true,
        _ => false,
    }
}

/// `bindsym|bindcode [options] [modifiers+]key [options] command`.
fn parse_binding(line: &str) -> Option<Binding> {
    let (keyword, mut position) = next_word(line, 0, false)?;
    let by_code = match keyword.as_str() {
        "bindsym" => false,
        // `bind` is the old name of `bindcode`.
        "bindcode" | "bind" => true,
        _ => return None,
    };
    let mut release = false;
    let spec = loop {
        let (word, after) = next_word(line, position, false)?;
        position = after;
        if !is_option(&word, &mut release) {
            break word;
        }
    };
    // Options may also follow the key.
    let mut command_start = position;
    while let Some((word, after)) = next_word(line, command_start, false) {
        if !is_option(&word, &mut release) {
            break;
        }
        command_start = after;
    }
    let command = line[command_start..].trim();
    if command.is_empty() {
        return None;
    }
    let (modifiers, key) = match spec.rfind('+') {
        Some(plus) if plus + 1 < spec.len() => (&spec[..plus], &spec[plus + 1..]),
        _ => ("", spec.as_str()),
    };
    let (mods, groups) = modifiers_from_str(modifiers);
    let input = if by_code {
        Input::Code(
            key.parse::<u32>()
                .ok()
                .filter(|code| (1..=255).contains(code))?,
        )
    } else {
        // Mouse buttons are not keys.
        if key.len() >= 6 && key[..6].eq_ignore_ascii_case("button") {
            return None;
        }
        // i3 looks key names up with libxkbcommon, case and all.
        Input::Symbol(crate::xkb::keysym_exact(key)?)
    };
    Some(Binding {
        mode: String::new(),
        input,
        mods,
        groups,
        release,
        command: command.to_owned(),
    })
}

/// The modifiers and layouts a binding's modifier text names, read as i3
/// reads it: each name counts wherever it appears.
fn modifiers_from_str(text: &str) -> (u32, u32) {
    let mut mods = 0;
    for (name, mask) in [
        ("Mod1", MOD1_MASK),
        ("Mod2", MOD2_MASK),
        ("Mod3", MOD3_MASK),
        ("Mod4", MOD4_MASK),
        ("Mod5", MOD5_MASK),
        ("Control", CONTROL_MASK),
        ("Ctrl", CONTROL_MASK),
        ("Shift", SHIFT_MASK),
    ] {
        if text.contains(name) {
            mods |= mask;
        }
    }
    let mut groups = 0;
    for (name, bit) in [
        ("Group1", 0b0001),
        ("Group2", 0b0010),
        ("Mode_switch", 0b0010),
        ("Group3", 0b0100),
        ("Group4", 0b1000),
    ] {
        if text.contains(name) {
            groups |= bit;
        }
    }
    (mods, groups)
}

/// Keypad keys keep their own symbol: KP_1 can be bound apart from KP_End.
fn is_keypad_symbol(keyval: u32) -> bool {
    (*gdk::keys::constants::KP_Space..=*gdk::keys::constants::KP_Equal).contains(&keyval)
}

/// A key press, as i3 sees it.
#[derive(Clone, Copy, Debug)]
pub struct Press {
    pub keycode: u32,
    /// The event state: modifiers and mouse buttons.
    pub state: u32,
    /// The active layout, from 0.
    pub layout: u32,
}

/// Whether `binding` matches `press`. `numlock_mask` is the real modifier
/// that carries Num Lock.
pub fn matches(binding: &Binding, press: &Press, numlock_mask: u32, keymap: &dyn Keymap) -> bool {
    if binding.groups != 0 && binding.groups & (1 << press.layout) != binding.groups {
        return false;
    }
    let layout = binding.layout();
    let mods = binding.mods & EVENT_STATE_MASK;
    let symbol = |mods: u32| {
        keymap
            .translate(press.keycode, mods, layout)
            .map(|(sym, _)| sym)
    };
    // Whether the key is the binding's, and whether Num Lock leaves it so.
    let numlock_keeps_key = match binding.input {
        Input::Code(code) => {
            if press.keycode != code {
                return false;
            }
            symbol(mods) == symbol(mods | numlock_mask)
        }
        Input::Symbol(wanted) => {
            let Some((sym, level)) = keymap.translate(press.keycode, mods, layout) else {
                return false;
            };
            if sym == wanted {
                symbol(mods | numlock_mask) == Some(wanted)
            } else if level <= 1
                && !is_keypad_symbol(sym)
                && symbol(mods ^ SHIFT_MASK) == Some(wanted)
            {
                symbol((mods | numlock_mask) ^ SHIFT_MASK) == Some(wanted)
            } else {
                return false;
            }
        }
    };
    let state = press.state & EVENT_STATE_MASK & !LOCK_MASK;
    state == mods
        || (numlock_keeps_key
            && mods & numlock_mask != numlock_mask
            && state == mods | numlock_mask)
}

/// What i3 does with `press` in binding mode `mode`: the first binding that
/// runs on the press, or else the first release binding the press arms.
pub fn lookup<'a>(
    bindings: &'a [Binding],
    mode: &str,
    press: &Press,
    numlock_mask: u32,
    keymap: &dyn Keymap,
) -> Option<&'a Binding> {
    let mut on_release = None;
    for binding in bindings {
        if binding.mode != mode || !matches(binding, press, numlock_mask, keymap) {
            continue;
        }
        if !binding.release {
            return Some(binding);
        }
        on_release = on_release.or(Some(binding));
    }
    on_release
}

/// Whether a sway command consists only of keyboard-layout switches:
/// `input <identifier> xkb_switch_layout <layout>`, one or more, joined by
/// commas or semicolons.
pub fn is_layout_switch_only(command: &str) -> bool {
    let mut switches = 0;
    for part in split_commands(command) {
        let words = command_words(&part);
        if words.is_empty() {
            continue;
        }
        if words.len() != 4
            || !words[0].eq_ignore_ascii_case("input")
            || !words[2].eq_ignore_ascii_case("xkb_switch_layout")
        {
            return false;
        }
        switches += 1;
    }
    switches > 0
}

/// A command list split at the commas and semicolons outside quotes.
fn split_commands(commands: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut quote, mut escaped) = (None, false);
    for c in commands.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if let Some(open) = quote {
            if c == open {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == ',' || c == ';' {
            parts.push(std::mem::take(&mut current));
            continue;
        }
        current.push(c);
    }
    parts.push(current);
    parts
}

/// The words of one command, quotes removed.
fn command_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut quote, mut escaped, mut in_word) = (None, false, false);
    for c in command.chars() {
        if escaped {
            escaped = false;
            current.push(c);
        } else if c == '\\' {
            escaped = true;
            in_word = true;
        } else if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            in_word = true;
        } else if c.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut current));
                in_word = false;
            }
        } else {
            current.push(c);
            in_word = true;
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A keyboard map from a table: (keycode, layout) → symbols by level
    /// (plain, Shift). Num Lock (Mod2) moves keypad keys to their digits.
    struct TableKeymap {
        keys: HashMap<(u32, u32), Vec<u32>>,
        keypad: HashMap<u32, (u32, u32)>,
    }

    impl Keymap for TableKeymap {
        fn translate(&self, keycode: u32, mods: u32, layout: u32) -> Option<(u32, i32)> {
            if let Some(&(plain, numlock)) = self.keypad.get(&keycode) {
                let shifted = (mods & MOD2_MASK != 0) != (mods & SHIFT_MASK != 0);
                return Some(if shifted { (numlock, 1) } else { (plain, 0) });
            }
            let levels = self
                .keys
                .get(&(keycode, layout))
                .or_else(|| self.keys.get(&(keycode, 0)))?;
            let level = usize::from(mods & SHIFT_MASK != 0).min(levels.len() - 1);
            Some((levels[level], level as i32))
        }
    }

    fn sym(name: &str) -> u32 {
        *gdk::keys::Key::from_name(name)
    }

    fn keymap() -> TableKeymap {
        let mut keys = HashMap::new();
        // y and z swap places in the second layout, as in us,de.
        keys.insert((29, 0), vec![sym("y"), sym("Y")]);
        keys.insert((29, 1), vec![sym("z"), sym("Z")]);
        keys.insert((52, 0), vec![sym("z"), sym("Z")]);
        keys.insert((52, 1), vec![sym("y"), sym("Y")]);
        keys.insert((10, 0), vec![sym("1"), sym("exclam")]);
        keys.insert((75, 0), vec![sym("F9")]);
        keys.insert((122, 0), vec![sym("XF86AudioLowerVolume")]);
        let mut keypad = HashMap::new();
        keypad.insert(87, (sym("KP_End"), sym("KP_1")));
        TableKeymap { keys, keypad }
    }

    fn press(keycode: u32, state: u32, layout: u32) -> Press {
        Press {
            keycode,
            state,
            layout,
        }
    }

    const CTRL_ALT: u32 = CONTROL_MASK | MOD1_MASK;

    fn find<'a>(bindings: &'a [Binding], mode: &str, p: Press) -> Option<&'a str> {
        lookup(bindings, mode, &p, MOD2_MASK, &keymap()).map(|b| b.command.as_str())
    }

    #[test]
    fn bindings_come_from_every_file_and_mode() {
        let files = vec![
            "font pango:monospace 8\nbindsym Control+Mod1+t exec a\nmode \"probe mode\" {\n  bindsym F9 exec b\n  bindsym F7 mode \"default\"\n}\nbar {\n  bindsym button4 nop\n}\n".to_owned(),
            "bindsym --release Mod4+y exec c --flag \"q x\"; exec d\nbindcode Control+Mod1+29 exec e\nbindsym Mod1+button1 nop\n".to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        let summary: Vec<(&str, &str, bool)> = bindings
            .iter()
            .map(|b| (b.mode.as_str(), b.command.as_str(), b.release))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("default", "exec a", false),
                ("probe mode", "exec b", false),
                ("probe mode", "mode \"default\"", false),
                ("default", "exec c --flag \"q x\"; exec d", true),
                ("default", "exec e", false),
            ]
        );
        assert_eq!(bindings[0].input, Input::Symbol(sym("t")));
        assert_eq!(bindings[0].mods, CTRL_ALT);
        assert_eq!(bindings[3].mods, MOD4_MASK);
        assert_eq!(bindings[4].input, Input::Code(29));
    }

    #[test]
    fn mode_names_options_and_groups_parse_as_i3_reads_them() {
        let files = vec![
            "mode --pango_markup \"<b>resize</b>\"\n{\nbindsym Group2+Mod1+y --release exec r\n}\nbindsym Mode_switch+Group4+x exec n\n".to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].mode, "<b>resize</b>");
        assert!(bindings[0].release);
        assert_eq!(bindings[0].command, "exec r");
        assert_eq!(bindings[0].groups, 0b0010);
        assert_eq!(bindings[0].layout(), 1);
        assert_eq!(bindings[1].groups, 0b1010);
        assert_eq!(bindings[1].layout(), 1);
    }

    #[test]
    fn key_names_are_read_as_i3_reads_them() {
        let files = vec![
            "bindsym Mod1+0x74 exec hex\nbindsym Mod1+U00E9 exec eacute\nbindsym XF86AudioMute exec mute\nbindsym Mod1+Return exec enter\nbindsym Mod1+return exec lower\nbindsym Mod1+notakey exec none\n".to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        let inputs: Vec<(&str, &Input)> = bindings
            .iter()
            .map(|b| (b.command.as_str(), &b.input))
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("exec hex", &Input::Symbol(sym("t"))),
                ("exec eacute", &Input::Symbol(sym("eacute"))),
                ("exec mute", &Input::Symbol(sym("AudioMute"))),
                ("exec enter", &Input::Symbol(sym("Return"))),
            ]
        );
    }

    #[test]
    fn only_the_current_mode_counts() {
        let files = vec![
            "bindsym F9 exec outer\nmode \"probe mode\" {\nbindsym F9 exec inner\n}\n".to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        assert_eq!(
            find(&bindings, "default", press(75, 0, 0)),
            Some("exec outer")
        );
        assert_eq!(
            find(&bindings, "probe mode", press(75, 0, 0)),
            Some("exec inner")
        );
        assert_eq!(find(&bindings, "other", press(75, 0, 0)), None);
    }

    #[test]
    fn symbols_match_in_the_first_layout_or_the_named_one() {
        let files = vec![
            "bindsym Control+Mod1+y exec y\nbindsym Control+Mod1+Group2+Shift+y exec g2\n"
                .to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        // Code 29 is y in the first layout: it matches in both layouts.
        assert_eq!(
            find(&bindings, "default", press(29, CTRL_ALT, 0)),
            Some("exec y")
        );
        assert_eq!(
            find(&bindings, "default", press(29, CTRL_ALT, 1)),
            Some("exec y")
        );
        assert_eq!(find(&bindings, "default", press(52, CTRL_ALT, 1)), None);
        // The Group2 binding: y of the second layout, only while it is active.
        let shifted = CTRL_ALT | SHIFT_MASK;
        assert_eq!(
            find(&bindings, "default", press(52, shifted, 1)),
            Some("exec g2")
        );
        assert_eq!(find(&bindings, "default", press(52, shifted, 0)), None);
    }

    #[test]
    fn modifiers_match_exactly_but_for_caps_lock_and_harmless_num_lock() {
        let files =
            vec!["bindsym Control+Mod1+y exec y\nbindcode Control+Mod1+52 exec c\n".to_owned()];
        let bindings = parse_i3_config(&files);
        let locks = LOCK_MASK | MOD2_MASK;
        assert_eq!(
            find(&bindings, "default", press(29, CTRL_ALT | locks, 1)),
            Some("exec y")
        );
        assert_eq!(find(&bindings, "default", press(29, CONTROL_MASK, 0)), None);
        assert_eq!(
            find(&bindings, "default", press(29, CTRL_ALT | SHIFT_MASK, 0)),
            None
        );
        assert_eq!(
            find(&bindings, "default", press(29, CTRL_ALT | MOD4_MASK, 0)),
            None
        );
        assert_eq!(
            find(&bindings, "default", press(52, CTRL_ALT | locks, 1)),
            Some("exec c")
        );
        assert_eq!(
            find(&bindings, "default", press(52, CTRL_ALT | SHIFT_MASK, 0)),
            None
        );
    }

    #[test]
    fn shifted_fallback_applies_except_on_the_keypad() {
        let files = vec![
            "bindsym Control+Mod1+exclam exec bang\nbindsym Control+Mod1+KP_1 exec one\n"
                .to_owned(),
        ];
        let bindings = parse_i3_config(&files);
        // Without Shift: the key gives 1, with Shift toggled exclam.
        assert_eq!(
            find(&bindings, "default", press(10, CTRL_ALT, 0)),
            Some("exec bang")
        );
        // KP_1 needs Num Lock; the keypad never falls back to Shift.
        assert_eq!(find(&bindings, "default", press(87, CTRL_ALT, 0)), None);
        assert_eq!(
            find(&bindings, "default", press(87, CTRL_ALT | MOD2_MASK, 0)),
            None
        );
        let numlocked = parse_i3_config(&["bindsym Control+Mod1+Mod2+KP_1 exec one\n".to_owned()]);
        assert_eq!(
            find(&numlocked, "default", press(87, CTRL_ALT | MOD2_MASK, 0)),
            Some("exec one")
        );
    }

    #[test]
    fn a_press_binding_wins_over_a_release_binding() {
        let files = vec!["bindsym --release F9 exec late\nbindsym F9 exec now\n".to_owned()];
        let bindings = parse_i3_config(&files);
        let found = lookup(&bindings, "default", &press(75, 0, 0), MOD2_MASK, &keymap()).unwrap();
        assert_eq!((found.command.as_str(), found.release), ("exec now", false));
        let release_only = parse_i3_config(&["bindsym --release F9 exec late\n".to_owned()]);
        let found = lookup(
            &release_only,
            "default",
            &press(75, 0, 0),
            MOD2_MASK,
            &keymap(),
        )
        .unwrap();
        assert!(found.release);
    }

    #[test]
    fn layout_switch_only_commands() {
        for command in [
            "input type:keyboard xkb_switch_layout next",
            "input \"*\" xkb_switch_layout 1",
            "input type:keyboard xkb_switch_layout next, input type:keyboard xkb_switch_layout 1",
            "input type:keyboard xkb_switch_layout next; input type:keyboard xkb_switch_layout prev",
            "INPUT type:keyboard XKB_SWITCH_LAYOUT next;",
        ] {
            assert!(is_layout_switch_only(command), "{command}");
        }
        for command in [
            "",
            "exec probe",
            "input type:keyboard xkb_switch_layout next; exec probe",
            "input type:keyboard xkb_switch_layout next, exec probe",
            "input type:keyboard xkb_layout us",
            "exec \"input a xkb_switch_layout 1; x\"",
        ] {
            assert!(!is_layout_switch_only(command), "{command}");
        }
    }
}
