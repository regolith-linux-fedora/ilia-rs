//! The keys a keyboard layout puts on key codes, from libxkbcommon: sway
//! builds its keymaps from the configuration's `xkb_*` settings with the same
//! library, and GDK cannot build a keymap for a layout it was not given.

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;

#[repr(C)]
struct RuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

#[link(name = "xkbcommon")]
extern "C" {
    fn xkb_context_new(flags: c_int) -> *mut c_void;
    fn xkb_context_unref(context: *mut c_void);
    fn xkb_keymap_new_from_names(
        context: *mut c_void,
        names: *const RuleNames,
        flags: c_int,
    ) -> *mut c_void;
    fn xkb_keymap_unref(keymap: *mut c_void);
    fn xkb_keymap_min_keycode(keymap: *mut c_void) -> u32;
    fn xkb_keymap_max_keycode(keymap: *mut c_void) -> u32;
    fn xkb_keysym_from_name(name: *const c_char, flags: c_int) -> u32;
    fn xkb_keymap_key_get_syms_by_level(
        keymap: *mut c_void,
        key: u32,
        layout: u32,
        level: u32,
        syms_out: *mut *const u32,
    ) -> c_int;
}

/// The settings a keymap is built from, as sway's `xkb_rules`, `xkb_model`,
/// `xkb_layout`, `xkb_variant` and `xkb_options` give them. An unset one takes
/// libxkbcommon's default, as it does in sway.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Names {
    pub rules: Option<String>,
    pub model: Option<String>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
}

const KEYSYM_NO_FLAGS: c_int = 0;
const KEYSYM_CASE_INSENSITIVE: c_int = 1;

/// The keysym `name` names, read the way sway reads key names: without
/// regard to case, taking the lower-case keysym where case alone tells two
/// apart. `None` for a name that names no keysym.
pub fn keysym_ignoring_case(name: &str) -> Option<u32> {
    keysym_from_name(name, KEYSYM_CASE_INSENSITIVE)
}

/// The keysym `name` names: the one spelled exactly so if there is one, else
/// the one it names without regard to case.
pub fn keysym(name: &str) -> Option<u32> {
    keysym_from_name(name, KEYSYM_NO_FLAGS).or_else(|| keysym_ignoring_case(name))
}

/// The keysym `name` spells exactly, read the way i3 reads key names: case
/// counts, and hexadecimal (`0x1008ff13`) and Unicode (`U20AC`) names work.
pub fn keysym_exact(name: &str) -> Option<u32> {
    keysym_from_name(name, KEYSYM_NO_FLAGS)
}

fn keysym_from_name(name: &str, flags: c_int) -> Option<u32> {
    let name = CString::new(name).ok()?;
    // SAFETY: the name outlives the call, which does not keep it.
    let keysym = unsafe { xkb_keysym_from_name(name.as_ptr(), flags) };
    (keysym != 0).then_some(keysym)
}

/// A compiled keymap.
pub struct Keymap(*mut c_void);

impl Keymap {
    /// The keymap `names` describe, or `None` when libxkbcommon cannot build
    /// it.
    pub fn new(names: &Names) -> Option<Self> {
        let text =
            |value: &Option<String>| value.as_deref().and_then(|value| CString::new(value).ok());
        let (rules, model, layout, variant, options) = (
            text(&names.rules),
            text(&names.model),
            text(&names.layout),
            text(&names.variant),
            text(&names.options),
        );
        let pointer =
            |value: &Option<CString>| value.as_ref().map_or(ptr::null(), |value| value.as_ptr());
        let names = RuleNames {
            rules: pointer(&rules),
            model: pointer(&model),
            layout: pointer(&layout),
            variant: pointer(&variant),
            options: pointer(&options),
        };
        // SAFETY: the strings outlive the call, which copies what it keeps;
        // the context is released once the keymap holds its own reference.
        unsafe {
            let context = xkb_context_new(0);
            if context.is_null() {
                return None;
            }
            let keymap = xkb_keymap_new_from_names(context, &names, 0);
            xkb_context_unref(context);
            (!keymap.is_null()).then_some(Self(keymap))
        }
    }

    /// The keysyms `code` gives in the first layout with no modifier held.
    fn keysyms(&self, code: u32) -> &[u32] {
        let mut syms = ptr::null();
        // SAFETY: the keymap is alive. The array it returns belongs to the
        // keymap, which the returned slice borrows.
        unsafe {
            let count = xkb_keymap_key_get_syms_by_level(self.0, code, 0, 0, &mut syms);
            match usize::try_from(count) {
                Ok(count) if count > 0 && !syms.is_null() => {
                    std::slice::from_raw_parts(syms, count)
                }
                _ => &[],
            }
        }
    }

    /// The keysym `code` gives in the first layout with no modifier held, or
    /// `None` when it gives none.
    pub fn first_keysym(&self, code: u32) -> Option<u32> {
        self.keysyms(code).first().copied()
    }

    /// The one key code whose key gives `keysym`, and nothing else, in the
    /// first layout with no modifier held, as sway converts `bindsym
    /// --to-code` keys. `None` when no code or more than one gives it: sway
    /// then keeps the binding as a keysym binding.
    pub fn single_code(&self, keysym: u32) -> Option<u32> {
        // SAFETY: the keymap is alive.
        let (first, last) = unsafe {
            (
                xkb_keymap_min_keycode(self.0),
                xkb_keymap_max_keycode(self.0),
            )
        };
        let mut codes = (first..=last).filter(|&code| self.keysyms(code) == [keysym]);
        let code = codes.next()?;
        codes.next().is_none().then_some(code)
    }
}

impl Drop for Keymap {
    fn drop(&mut self) {
        // SAFETY: the keymap was created by `new` and is released once.
        unsafe { xkb_keymap_unref(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::{keysym, keysym_ignoring_case, Keymap, Names};

    #[test]
    fn codes_give_the_first_layouts_keys() {
        let names = |layout: &str| Names {
            layout: Some(layout.to_owned()),
            ..Names::default()
        };
        let german = Keymap::new(&names("de,us")).unwrap();
        // Code 52 is Y on a German keyboard and Z on a US one.
        assert_eq!(german.first_keysym(52), Some(0x79));
        assert_eq!(german.first_keysym(36), Some(0xff0d));
        let us = Keymap::new(&names("us,de")).unwrap();
        assert_eq!(us.first_keysym(52), Some(0x7a));
        // Code 8 has no key.
        assert_eq!(us.first_keysym(8), None);
    }

    #[test]
    fn to_code_keys_need_exactly_one_code() {
        let names = |layout: &str| Names {
            layout: Some(layout.to_owned()),
            ..Names::default()
        };
        let code = |keymap: &Keymap, name: &str| keymap.single_code(keysym_ignoring_case(name)?);
        let us = Keymap::new(&names("us")).unwrap();
        assert_eq!(code(&us, "t"), Some(28));
        assert_eq!(code(&us, "T"), Some(28));
        assert_eq!(code(&us, "return"), Some(36));
        // Question is a shifted key on a US keyboard.
        assert_eq!(code(&us, "question"), None);
        assert_eq!(code(&us, "nosuchkey"), None);
        let german = Keymap::new(&names("de")).unwrap();
        assert_eq!(code(&german, "y"), Some(52));
    }

    #[test]
    fn key_names_give_keysyms() {
        // sway reads names without regard to case, preferring lower case.
        assert_eq!(keysym_ignoring_case("BRACKETLEFT"), Some(0x5b));
        assert_eq!(keysym_ignoring_case("Odiaeresis"), Some(0xf6));
        // Different names of one keysym, and its number, are the same key.
        assert_eq!(
            keysym_ignoring_case("Prior"),
            keysym_ignoring_case("page_up")
        );
        assert_eq!(keysym_ignoring_case("0x79"), Some(0x79));
        assert_eq!(keysym_ignoring_case("nosuchkey"), None);
        // A name spelled exactly keeps its case.
        assert_eq!(keysym("Odiaeresis"), Some(0xd6));
        assert_eq!(keysym("ODIAERESIS"), Some(0xf6));
    }
}
