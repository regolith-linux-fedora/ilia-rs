//! The few Xlib and XTEST calls the X11 key forwarding needs, behind a safe
//! API: the modifier that carries Num Lock, and pressing a key again for the
//! program that grabs it.

use std::os::raw::{c_int, c_uint, c_ulong, c_void};
use std::ptr::NonNull;

use glib::translate::ToGlibPtr;
use gtk::gdk;
use gtk::prelude::*;

use crate::wm_bindings::{LOCK_MASK, REAL_MODIFIERS};

const NUM_LOCK_KEYSYM: c_ulong = 0xff7f;
/// Where Num Lock usually is, when the server does not say.
const DEFAULT_NUMLOCK_MASK: u32 = 1 << 4;

#[repr(C)]
struct XModifierKeymap {
    max_keypermod: c_int,
    modifiermap: *mut u8,
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
