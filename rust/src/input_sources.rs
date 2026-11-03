//! The desktop's keyboard layout switch, while ilia is open.
//!
//! Regolith's layout-switch shortcut (`switch-input-source` in
//! `org.gnome.desktop.wm.keybindings`, Alt+Super+BackSpace by default) holds
//! Super, one of ilia's exit keys. On X11 ilia also grabs the keyboard, so
//! the shortcut, which gnome-flashback grabs, cannot reach the desktop while
//! ilia is open. ilia recognises the shortcut itself and asks gnome-flashback
//! over D-Bus to activate the input source the shortcut would have chosen,
//! keeping its grab. On Wayland the compositor handles the shortcut; ilia
//! only has to stay open.

use gio::prelude::*;
use gtk::gdk;
use gtk::gdk::ModifierType;

const KEYBINDINGS_SCHEMA: &str = "org.gnome.desktop.wm.keybindings";
const INPUT_SOURCES_SCHEMA: &str = "org.gnome.desktop.input-sources";
const FLASHBACK_BUS_NAME: &str = "org.gnome.Flashback.InputSources";
const FLASHBACK_OBJECT_PATH: &str = "/org/gnome/Flashback/InputSources";
const FLASHBACK_INTERFACE: &str = "org.gnome.Flashback.InputSources";
const DBUS_TIMEOUT_MS: i32 = 1000;

const VIRTUAL_MODIFIERS: ModifierType = ModifierType::SUPER_MASK
    .union(ModifierType::HYPER_MASK)
    .union(ModifierType::META_MASK);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Forward,
    Backward,
}

/// A shortcut, with its modifiers as real X modifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Accel {
    keyval: u32,
    mods: ModifierType,
}

pub struct InputSourceSwitcher {
    forward: Vec<Accel>,
    backward: Vec<Accel>,
    // Real modifiers that take part in matching (not Caps Lock or Num Lock).
    mod_mask: ModifierType,
    // The real modifier that carries Super.
    super_mask: ModifierType,
}

impl InputSourceSwitcher {
    /// `forward` and `backward` are parsed accelerators, as keyvals and
    /// modifiers. `keymap` maps the virtual modifiers (Super, Hyper, Meta) to
    /// real ones; without one, or without a mapping, the usual X11 mapping is
    /// assumed.
    fn new(
        forward: &[(u32, ModifierType)],
        backward: &[(u32, ModifierType)],
        keymap: Option<&gdk::Keymap>,
    ) -> Self {
        // GTK's default accelerator modifiers.
        let mod_mask = to_real(
            ModifierType::SHIFT_MASK
                | ModifierType::CONTROL_MASK
                | ModifierType::MOD1_MASK
                | VIRTUAL_MODIFIERS,
            keymap,
        );
        let accels = |parsed: &[(u32, ModifierType)]| -> Vec<Accel> {
            parsed
                .iter()
                .filter(|(keyval, _)| *keyval != 0)
                .map(|&(keyval, mods)| Accel {
                    keyval: lower(keyval),
                    mods: to_real(mods, keymap) & mod_mask,
                })
                .collect()
        };
        Self {
            forward: accels(forward),
            backward: accels(backward),
            mod_mask,
            super_mask: to_real(ModifierType::SUPER_MASK, keymap),
        }
    }

    /// The switcher for the shortcuts configured in the GNOME settings, or
    /// `None` when there are none.
    pub fn from_settings(keymap: Option<&gdk::Keymap>) -> Option<Self> {
        let settings = settings(KEYBINDINGS_SCHEMA)?;
        let parse = |key: &str| -> Vec<(u32, ModifierType)> {
            settings
                .strv(key)
                .iter()
                .map(|accel| gtk::accelerator_parse(accel))
                .collect()
        };
        let switcher = Self::new(
            &parse("switch-input-source"),
            &parse("switch-input-source-backward"),
            keymap,
        );
        (!switcher.forward.is_empty() || !switcher.backward.is_empty()).then_some(switcher)
    }

    /// Matches a key press against the layout-switch shortcuts. `base_keyval`
    /// is the same physical key in the first layout, so that a shortcut on a
    /// letter key still matches while a non-Latin layout is active.
    pub fn match_key(
        &self,
        keyval: u32,
        base_keyval: u32,
        state: ModifierType,
    ) -> Option<Direction> {
        let (keyval, base_keyval) = (lower(keyval), lower(base_keyval));
        let mods = state & self.mod_mask;
        let matches = |accels: &[Accel]| {
            accels.iter().any(|accel| {
                (accel.keyval == keyval || accel.keyval == base_keyval) && accel.mods == mods
            })
        };
        if matches(&self.backward) {
            Some(Direction::Backward)
        } else if matches(&self.forward) {
            Some(Direction::Forward)
        } else {
            None
        }
    }

    /// Whether Super, pressed while the modifiers in `state` are held, may
    /// complete the modifiers of a layout-switch shortcut. Super pressed on
    /// its own still means "exit", so Super+<key> keeps reaching the window
    /// manager.
    pub fn super_starts_shortcut(&self, state: ModifierType) -> bool {
        let held = state & self.mod_mask & !self.super_mask;
        if held.is_empty() {
            return false;
        }
        let wanted = held | self.super_mask;
        self.forward
            .iter()
            .chain(&self.backward)
            .any(|accel| accel.mods.contains(wanted))
    }

    /// Asks gnome-flashback to activate the next or the previous input
    /// source. Failures are reported on stderr.
    pub fn switch_source(direction: Direction) {
        glib::MainContext::default().spawn_local(async move {
            if let Err(error) = activate_source(direction).await {
                eprintln!("Unable to switch keyboard layout: {}", error.message());
            }
        });
    }
}

/// Maps the virtual modifiers (Super, Hyper, Meta) to the real ones that
/// carry them. Without an answer from the keymap, assume the usual X11
/// mapping: Super and Hyper on Mod4, Meta on Mod1.
fn to_real(mods: ModifierType, keymap: Option<&gdk::Keymap>) -> ModifierType {
    let mut real = mods & !VIRTUAL_MODIFIERS & ModifierType::MODIFIER_MASK;
    for virtual_mod in [
        ModifierType::SUPER_MASK,
        ModifierType::HYPER_MASK,
        ModifierType::META_MASK,
    ] {
        if !mods.contains(virtual_mod) {
            continue;
        }
        let mut mapped = virtual_mod;
        if let Some(keymap) = keymap {
            keymap.map_virtual_modifiers(&mut mapped);
        }
        mapped &= !VIRTUAL_MODIFIERS & ModifierType::MODIFIER_MASK;
        if mapped.is_empty() {
            mapped = if virtual_mod == ModifierType::META_MASK {
                ModifierType::MOD1_MASK
            } else {
                ModifierType::MOD4_MASK
            };
        }
        real |= mapped;
    }
    real
}

fn lower(keyval: u32) -> u32 {
    *gdk::keys::Key::from(keyval).to_lower()
}

/// The settings for `schema`, or `None` when it is not installed.
fn settings(schema: &str) -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(schema, true)?;
    Some(gio::Settings::new(schema))
}

/// Configured input sources as `type:id`.
fn read_sources(settings: &gio::Settings, key: &str) -> Vec<String> {
    settings
        .value(key)
        .get::<Vec<(String, String)>>()
        .unwrap_or_default()
        .into_iter()
        .map(|(kind, id)| format!("{kind}:{id}"))
        .collect()
}

/// The input source a layout-switch shortcut activates, as gnome-flashback's
/// own switcher picks it: sources in most-recently-used order with the
/// current one first; the shortcut picks the second, the backward shortcut
/// the last. `None` when there is nothing to switch to.
fn pick_source(
    sources: &[String],
    mru: &[String],
    current: Option<usize>,
    direction: Direction,
) -> Option<usize> {
    let mut order: Vec<usize> = Vec::with_capacity(sources.len());
    let recent = mru
        .iter()
        .filter_map(|recent| sources.iter().position(|source| source == recent));
    for index in recent.chain(0..sources.len()) {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    if let Some(current) = current.filter(|current| *current < sources.len()) {
        order.retain(|index| *index != current);
        order.insert(0, current);
    }
    if order.len() < 2 {
        return None;
    }
    match direction {
        Direction::Forward => Some(order[1]),
        Direction::Backward => order.last().copied(),
    }
}

async fn activate_source(direction: Direction) -> Result<(), glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::Session).await?;
    let reply = bus
        .call_future(
            Some(FLASHBACK_BUS_NAME),
            FLASHBACK_OBJECT_PATH,
            FLASHBACK_INTERFACE,
            "GetInputSources",
            None,
            Some(glib::VariantTy::new("(a(ussb)a{sv})").expect("valid type string")),
            gio::DBusCallFlags::NO_AUTO_START,
            DBUS_TIMEOUT_MS,
        )
        .await?;
    let loaded = reply
        .child_value(0)
        .get::<Vec<(u32, String, String, bool)>>()
        .unwrap_or_default();
    let current = loaded
        .iter()
        .find(|(_, _, _, active)| *active)
        .map(|(index, ..)| *index as usize);

    let (mut sources, mut mru) = settings(INPUT_SOURCES_SCHEMA)
        .map(|settings| {
            (
                read_sources(&settings, "sources"),
                read_sources(&settings, "mru-sources"),
            )
        })
        .unwrap_or_default();
    // gnome-flashback numbers the sources it could load; fall back to that
    // order when it does not match the settings.
    if sources.len() != loaded.len() {
        sources = (0..loaded.len()).map(|index| index.to_string()).collect();
        mru.clear();
    }
    let Some(target) = pick_source(&sources, &mru, current, direction) else {
        return Ok(());
    };
    bus.call_future(
        Some(FLASHBACK_BUS_NAME),
        FLASHBACK_OBJECT_PATH,
        FLASHBACK_INTERFACE,
        "Activate",
        Some(&(target as u32,).to_variant()),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        DBUS_TIMEOUT_MS,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{pick_source, Direction, InputSourceSwitcher};
    use gtk::gdk::{keys::constants as keys, ModifierType};

    const ALT: ModifierType = ModifierType::MOD1_MASK;
    const SUPER: ModifierType = ModifierType::MOD4_MASK;

    // <Super><Alt>BackSpace and <Shift><Super><Alt>BackSpace, as GTK parses them.
    fn switcher() -> InputSourceSwitcher {
        let mods = ModifierType::SUPER_MASK | ModifierType::MOD1_MASK;
        InputSourceSwitcher::new(
            &[(*keys::BackSpace, mods)],
            &[(*keys::BackSpace, mods | ModifierType::SHIFT_MASK)],
            None,
        )
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn shortcuts_match_on_real_modifiers() {
        let switcher = switcher();
        let backspace = *keys::BackSpace;
        assert_eq!(
            switcher.match_key(backspace, backspace, ALT | SUPER),
            Some(Direction::Forward)
        );
        // Lock modifiers and the virtual Super bit do not get in the way.
        assert_eq!(
            switcher.match_key(
                backspace,
                backspace,
                ALT | SUPER
                    | ModifierType::LOCK_MASK
                    | ModifierType::MOD2_MASK
                    | ModifierType::SUPER_MASK
            ),
            Some(Direction::Forward)
        );
        assert_eq!(
            switcher.match_key(backspace, backspace, ALT | SUPER | ModifierType::SHIFT_MASK),
            Some(Direction::Backward)
        );
        assert_eq!(switcher.match_key(backspace, backspace, SUPER), None);
        assert_eq!(
            switcher.match_key(*keys::Delete, *keys::Delete, ALT | SUPER),
            None
        );
    }

    #[test]
    fn letter_shortcuts_match_through_the_first_layout() {
        let switcher = InputSourceSwitcher::new(
            &[
                (*keys::space, ModifierType::SUPER_MASK),
                (*keys::k, ModifierType::MOD1_MASK),
            ],
            &[],
            None,
        );
        // The key typed is a Cyrillic letter; in the first layout it is k.
        assert_eq!(
            switcher.match_key(*keys::Cyrillic_ka, *keys::k, ALT),
            Some(Direction::Forward)
        );
        assert_eq!(
            switcher.match_key(*keys::K, *keys::K, ALT),
            Some(Direction::Forward)
        );
    }

    #[test]
    fn super_waits_only_when_it_completes_a_shortcut() {
        let switcher = switcher();
        assert!(switcher.super_starts_shortcut(ALT));
        assert!(switcher.super_starts_shortcut(ALT | ModifierType::SHIFT_MASK));
        // Super pressed first or alone still exits.
        assert!(!switcher.super_starts_shortcut(ModifierType::empty()));
        assert!(!switcher.super_starts_shortcut(ModifierType::LOCK_MASK));
        assert!(!switcher.super_starts_shortcut(ModifierType::CONTROL_MASK));
    }

    #[test]
    fn unparsable_accelerators_are_ignored() {
        // GTK parses an empty or invalid accelerator as keyval 0.
        let switcher = InputSourceSwitcher::new(&[(0, ModifierType::MOD1_MASK)], &[], None);
        assert!(!switcher.super_starts_shortcut(ALT));
        assert_eq!(switcher.match_key(*keys::a, *keys::a, ALT), None);
    }

    #[test]
    fn source_choice_follows_most_recent_use() {
        let sources = strings(&["xkb:us", "xkb:de", "xkb:fr"]);
        let mru = strings(&["xkb:de", "xkb:fr", "xkb:us"]);
        assert_eq!(
            pick_source(&sources, &mru, Some(1), Direction::Forward),
            Some(2)
        );
        assert_eq!(
            pick_source(&sources, &mru, Some(1), Direction::Backward),
            Some(0)
        );
        // The current source goes first even when the recent list is stale.
        assert_eq!(
            pick_source(&sources, &mru, Some(0), Direction::Forward),
            Some(1)
        );
        // Without a recent list, settings order.
        assert_eq!(
            pick_source(&sources, &[], Some(0), Direction::Forward),
            Some(1)
        );
        assert_eq!(
            pick_source(&sources, &[], None, Direction::Backward),
            Some(2)
        );
        let two = strings(&["xkb:us", "xkb:de"]);
        assert_eq!(
            pick_source(&two, &two, Some(1), Direction::Forward),
            Some(0)
        );
        assert_eq!(
            pick_source(&two[..1], &[], Some(0), Direction::Forward),
            None
        );
    }
}
