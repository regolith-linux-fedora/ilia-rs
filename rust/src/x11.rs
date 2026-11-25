//! The few Xlib and XTEST calls the X11 key forwarding needs, behind a safe
//! API: the modifier that carries Num Lock, and pressing a key again for the
//! program that grabs it. Also the display's X resource database.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use std::ptr::NonNull;

use glib::translate::ToGlibPtr;
use gtk::gdk;
use gtk::prelude::*;

use crate::wm_bindings::{LOCK_MASK, REAL_MODIFIERS};

const NUM_LOCK_KEYSYM: c_ulong = 0xff7f;
/// Where Num Lock usually is, when the server does not say.
const DEFAULT_NUMLOCK_MASK: u32 = 1 << 4;

/// Xlib's predefined STRING atom.
const XA_STRING: c_ulong = 31;
/// The most of the RESOURCE_MANAGER property read, in 32-bit units.
const RESOURCE_PROPERTY_LENGTH: c_long = 1 << 24;

#[repr(C)]
struct XModifierKeymap {
    max_keypermod: c_int,
    modifiermap: *mut u8,
}

#[repr(C)]
struct XrmValue {
    size: c_uint,
    addr: *mut c_char,
}

// GDK's X11 backend, part of the GDK library already linked.
extern "C" {
    fn gdk_x11_display_get_xdisplay(display: *mut gdk::ffi::GdkDisplay) -> *mut c_void;
    fn gdk_x11_display_error_trap_push(display: *mut gdk::ffi::GdkDisplay);
    fn gdk_x11_display_error_trap_pop_ignored(display: *mut gdk::ffi::GdkDisplay);
}

#[link(name = "X11")]
extern "C" {
    fn XKeysymToKeycode(display: *mut c_void, keysym: c_ulong) -> u8;
    fn XGetModifierMapping(display: *mut c_void) -> *mut XModifierKeymap;
    fn XFreeModifiermap(map: *mut XModifierKeymap) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XQueryPointer(
        display: *mut c_void,
        window: c_ulong,
        root_return: *mut c_ulong,
        child_return: *mut c_ulong,
        root_x_return: *mut c_int,
        root_y_return: *mut c_int,
        win_x_return: *mut c_int,
        win_y_return: *mut c_int,
        mask_return: *mut c_uint,
    ) -> c_int;
    fn XQueryKeymap(display: *mut c_void, keys_return: *mut [u8; 32]) -> c_int;
    fn XGrabServer(display: *mut c_void) -> c_int;
    fn XUngrabServer(display: *mut c_void) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XInternAtom(display: *mut c_void, name: *const c_char, only_if_exists: c_int) -> c_ulong;
    #[allow(clippy::too_many_arguments)]
    fn XGetWindowProperty(
        display: *mut c_void,
        window: c_ulong,
        property: c_ulong,
        long_offset: c_long,
        long_length: c_long,
        delete: c_int,
        req_type: c_ulong,
        actual_type_return: *mut c_ulong,
        actual_format_return: *mut c_int,
        nitems_return: *mut c_ulong,
        bytes_after_return: *mut c_ulong,
        prop_return: *mut *mut c_uchar,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XrmInitialize();
    fn XrmGetStringDatabase(data: *const c_char) -> *mut c_void;
    fn XrmGetResource(
        database: *mut c_void,
        name: *const c_char,
        class: *const c_char,
        type_return: *mut *mut c_char,
        value_return: *mut XrmValue,
    ) -> c_int;
    fn XrmDestroyDatabase(database: *mut c_void);
}

#[link(name = "Xtst")]
extern "C" {
    fn XTestQueryExtension(
        display: *mut c_void,
        event_base_return: *mut c_int,
        error_base_return: *mut c_int,
        major_version_return: *mut c_int,
        minor_version_return: *mut c_int,
    ) -> c_int;
    fn XTestFakeKeyEvent(
        display: *mut c_void,
        keycode: c_uint,
        is_press: c_int,
        delay: c_ulong,
    ) -> c_int;
}

/// GDK's connection to the X server.
pub struct X11Display {
    display: gdk::Display,
    xdisplay: NonNull<c_void>,
}

impl X11Display {
    /// The X connection behind `display`, or `None` when GDK does not run on
    /// X11.
    pub fn new(display: &gdk::Display) -> Option<Self> {
        if display.type_().name() != "GdkX11Display" {
            return None;
        }
        // SAFETY: `display` is a GdkX11Display, checked above, and stays alive
        // while `self` holds it, and with it its Xlib connection.
        let xdisplay = unsafe { gdk_x11_display_get_xdisplay(display.to_glib_none().0) };
        Some(Self {
            display: display.clone(),
            xdisplay: NonNull::new(xdisplay)?,
        })
    }

    /// The real modifier that carries Num Lock.
    pub fn numlock_mask(&self) -> u32 {
        let modifiers = self.modifier_keys();
        // SAFETY: a valid connection; the call only reads the keyboard map.
        let numlock = unsafe { XKeysymToKeycode(self.xdisplay.as_ptr(), NUM_LOCK_KEYSYM) };
        if numlock == 0 {
            return DEFAULT_NUMLOCK_MASK;
        }
        modifiers
            .iter()
            .position(|keys| keys.contains(&numlock))
            .map_or(DEFAULT_NUMLOCK_MASK, |index| 1 << index)
    }

    /// The key codes of each real modifier, Shift to Mod5.
    fn modifier_keys(&self) -> Vec<Vec<u8>> {
        let mut result = vec![Vec::new(); 8];
        // SAFETY: a valid connection. The map the server returns holds
        // `8 * max_keypermod` key codes, is only read here, and is freed once.
        unsafe {
            let map = XGetModifierMapping(self.xdisplay.as_ptr());
            let Some(map) = NonNull::new(map) else {
                return result;
            };
            let per_modifier = usize::try_from(map.as_ref().max_keypermod).unwrap_or(0);
            let codes = map.as_ref().modifiermap;
            if !codes.is_null() {
                let codes = std::slice::from_raw_parts(codes, 8 * per_modifier);
                for (index, keys) in result.iter_mut().enumerate() {
                    keys.extend(
                        codes[index * per_modifier..(index + 1) * per_modifier]
                            .iter()
                            .copied()
                            .filter(|code| *code != 0),
                    );
                }
            }
            XFreeModifiermap(map.as_ptr());
        }
        result
    }

    /// The real modifiers held now.
    fn held_modifiers(&self) -> u32 {
        let (mut root, mut child) = (0, 0);
        let (mut root_x, mut root_y, mut window_x, mut window_y) = (0, 0, 0, 0);
        let mut mask: c_uint = 0;
        // SAFETY: a valid connection and pointers to locals for every result.
        unsafe {
            let xdisplay = self.xdisplay.as_ptr();
            XQueryPointer(
                xdisplay,
                XDefaultRootWindow(xdisplay),
                &mut root,
                &mut child,
                &mut root_x,
                &mut root_y,
                &mut window_x,
                &mut window_y,
                &mut mask,
            );
        }
        mask & REAL_MODIFIERS
    }

    /// Whether the key with code `keycode` is down now.
    fn key_is_down(&self, keycode: u16) -> bool {
        let mut keys = [0u8; 32];
        // SAFETY: a valid connection and a 32-byte buffer, the size of the
        // bit vector the call fills.
        unsafe { XQueryKeymap(self.xdisplay.as_ptr(), &mut keys) };
        let code = usize::from(keycode);
        keys.get(code / 8)
            .is_some_and(|byte| byte & (1 << (code % 8)) != 0)
    }

    /// The value of the resource `name` (such as `wm.workspace.03.name`) in
    /// the display's resource database, the RESOURCE_MANAGER property of the
    /// root window as it is now, which `xrdb` loads. `None` when the resource
    /// is not set. The value is decoded as i3 decodes it, so a backslash that
    /// starts no escape stays (`C:\work`).
    pub fn resource(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        let text = self.resource_manager()?;
        let text = CString::new(with_xcb_xrm_escapes(text.as_bytes())).ok()?;
        // SAFETY: a NUL-terminated database text and resource name. The value
        // points into the database, so it is copied before the database is
        // destroyed, once.
        unsafe {
            XrmInitialize();
            let database = NonNull::new(XrmGetStringDatabase(text.as_ptr()))?;
            let mut kind: *mut c_char = std::ptr::null_mut();
            let mut value = XrmValue {
                size: 0,
                addr: std::ptr::null_mut(),
            };
            let found = XrmGetResource(
                database.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                &mut kind,
                &mut value,
            ) != 0;
            let result = (found && !value.addr.is_null())
                .then(|| CStr::from_ptr(value.addr).to_string_lossy().into_owned());
            XrmDestroyDatabase(database.as_ptr());
            result
        }
    }

    /// The text of the root window's RESOURCE_MANAGER property.
    fn resource_manager(&self) -> Option<CString> {
        let xdisplay = self.xdisplay.as_ptr();
        let (mut kind, mut format, mut items, mut after) = (0, 0, 0, 0);
        let mut data: *mut c_uchar = std::ptr::null_mut();
        // SAFETY: a valid connection, a NUL-terminated atom name and pointers
        // to locals. The property data, NUL-terminated by Xlib and holding
        // `items` bytes for format 8, is copied and freed once.
        unsafe {
            let atom = XInternAtom(xdisplay, c"RESOURCE_MANAGER".as_ptr(), 1);
            if atom == 0 {
                return None;
            }
            let status = XGetWindowProperty(
                xdisplay,
                XDefaultRootWindow(xdisplay),
                atom,
                0,
                RESOURCE_PROPERTY_LENGTH,
                0,
                XA_STRING,
                &mut kind,
                &mut format,
                &mut items,
                &mut after,
                &mut data,
            );
            let data = NonNull::new(data)?;
            let text = (status == 0 && kind == XA_STRING && format == 8).then(|| {
                let bytes = std::slice::from_raw_parts(data.as_ptr(), items as usize);
                let end = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(bytes.len());
                CString::new(&bytes[..end]).ok()
            });
            XFree(data.as_ptr().cast());
            text.flatten()
        }
    }

    /// Presses and releases the key with code `keycode` through XTEST, as if
    /// typed, with the real modifiers `mods` held for it: a modifier already
    /// let go of is pressed again around the key, one still held stays as it
    /// is. The lock modifiers are never pressed. A key still held is let up
    /// first, so the press is a new one rather than a repeat, which the X
    /// server drops when keys do not repeat and which never starts a grab.
    /// The server serves no other client from reading which modifiers are
    /// held until the key is in, so a key another client lets go of
    /// meanwhile cannot drop out of the press. Waits until the X server has
    /// handled it all.
    pub fn press_key(&self, keycode: u16, mods: u32) -> Result<(), String> {
        let xdisplay = self.xdisplay.as_ptr();
        let (mut event_base, mut error_base, mut major, mut minor) = (0, 0, 0, 0);
        // SAFETY: a valid connection and pointers to locals.
        let has_xtest = unsafe {
            XTestQueryExtension(
                xdisplay,
                &mut event_base,
                &mut error_base,
                &mut major,
                &mut minor,
            )
        } != 0;
        if !has_xtest {
            return Err("the X server has no XTEST extension".to_owned());
        }
        // Other clients wait from reading which modifiers are held until the
        // key has gone in. A modifier another client lets go of in between
        // would otherwise count as held and be missing from the press.
        // SAFETY: a valid connection; nothing below returns before the
        // matching XUngrabServer.
        unsafe { XGrabServer(xdisplay) };
        let missing =
            mods & REAL_MODIFIERS & !LOCK_MASK & !self.numlock_mask() & !self.held_modifiers();
        let modifier_keys: Vec<u8> = self
            .modifier_keys()
            .into_iter()
            .enumerate()
            .filter(|(index, _)| missing & (1 << index) != 0)
            .filter_map(|(_, keys)| keys.first().copied())
            .collect();
        let gdk_display: *mut gdk::ffi::GdkDisplay = self.display.to_glib_none().0;
        // SAFETY: a valid connection; X errors are trapped by GDK for the
        // duration, so a key the server refuses does not end the program.
        unsafe {
            gdk_x11_display_error_trap_push(gdk_display);
            for key in &modifier_keys {
                XTestFakeKeyEvent(xdisplay, c_uint::from(*key), 1, 0);
            }
            if self.key_is_down(keycode) {
                XTestFakeKeyEvent(xdisplay, c_uint::from(keycode), 0, 0);
            }
            XTestFakeKeyEvent(xdisplay, c_uint::from(keycode), 1, 0);
            XTestFakeKeyEvent(xdisplay, c_uint::from(keycode), 0, 0);
            for key in modifier_keys.iter().rev() {
                XTestFakeKeyEvent(xdisplay, c_uint::from(*key), 0, 0);
            }
            XUngrabServer(xdisplay);
            XSync(xdisplay, 0);
            gdk_x11_display_error_trap_pop_ignored(gdk_display);
        }
        Ok(())
    }
}

/// The resource database text `text` rewritten so that Xlib decodes every
/// value the way xcb-xrm, which i3 reads resources with, decodes it. Both
/// take `\\`, `\n` and a three-digit octal escape, and a backslash ending a
/// line continues the value on the next; but where Xlib drops a backslash
/// before anything else, xcb-xrm keeps it. Such a backslash is doubled here.
/// Comment (`!`) and directive (`#`) lines, and resource names, stay as
/// they are.
fn with_xcb_xrm_escapes(text: &[u8]) -> Vec<u8> {
    let line_end = |from: usize| {
        text[from..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(text.len(), |offset| from + offset + 1)
    };
    let mut rewritten = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let first = text[at..]
            .iter()
            .find(|&&byte| !matches!(byte, b' ' | b'\t'));
        let colon = text[at..]
            .iter()
            .position(|&byte| matches!(byte, b':' | b'\n'))
            .map(|offset| at + offset)
            .filter(|&colon| text[colon] == b':');
        let Some(colon) = colon.filter(|_| !matches!(first, Some(b'!' | b'#'))) else {
            let end = line_end(at);
            rewritten.extend_from_slice(&text[at..end]);
            at = end;
            continue;
        };
        rewritten.extend_from_slice(&text[at..=colon]);
        at = colon + 1;
        // The value, up to the first newline no backslash continues.
        while let Some(&byte) = text.get(at) {
            if byte != b'\\' {
                rewritten.push(byte);
                at += 1;
                if byte == b'\n' {
                    break;
                }
                continue;
            }
            let escape = match &text[at + 1..] {
                [b'\\' | b'n' | b'\n', ..] => 2,
                [a, b, c, ..] if [a, b, c].iter().all(|digit| (b'0'..=b'7').contains(digit)) => 4,
                _ => {
                    rewritten.extend_from_slice(b"\\\\");
                    at += 1;
                    continue;
                }
            };
            rewritten.extend_from_slice(&text[at..at + escape]);
            at += escape;
        }
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::with_xcb_xrm_escapes;

    fn rewritten(text: &str) -> String {
        String::from_utf8(with_xcb_xrm_escapes(text.as_bytes())).unwrap()
    }

    #[test]
    fn backslashes_starting_no_escape_survive_xlib() {
        assert_eq!(
            rewritten("wm.workspace.03.name:\t3: C:\\work\n"),
            "wm.workspace.03.name:\t3: C:\\\\work\n"
        );
        assert_eq!(rewritten("a:\tx\\ty\\x41\\\n"), "a:\tx\\\\ty\\\\x41\\\n");
        assert_eq!(rewritten("a:\tend\\"), "a:\tend\\\\");
        assert_eq!(rewritten("a:\t\\12x"), "a:\t\\\\12x");
    }

    #[test]
    fn escapes_both_libraries_share_stay() {
        let text = "a:\tback\\\\slash new\\nline \\101 \\777\n";
        assert_eq!(rewritten(text), text);
        assert_eq!(
            rewritten("a:\tone\\\n\ttwo\\w\nb:\t\\w\n"),
            "a:\tone\\\n\ttwo\\\\w\nb:\t\\\\w\n"
        );
    }

    #[test]
    fn names_comments_and_directives_stay() {
        let text = "! C:\\w: x\n# include \"a\\b\"\nno colon \\w\nwm\\x.name:\tv\\w\n";
        assert_eq!(
            rewritten(text),
            "! C:\\w: x\n# include \"a\\b\"\nno colon \\w\nwm\\x.name:\tv\\\\w\n"
        );
        assert_eq!(
            rewritten("a:\t3: say \"hi\" $x\n"),
            "a:\t3: say \"hi\" $x\n"
        );
    }
}
