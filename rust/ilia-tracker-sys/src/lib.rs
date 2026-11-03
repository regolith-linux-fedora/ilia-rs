//! Minimal safe wrapper over the synchronous Tracker calls used by Ilia.
//!
//! This crate deliberately binds only the four Tracker symbols required by
//! the pinned Vala implementation. Raw pointers and all unsafe operations stay
//! private to this crate.

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString};
use std::fmt;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::str;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackerError(String);

impl TrackerError {
    fn interior_nul() -> Self {
        Self("Tracker argument contains an interior NUL byte".to_owned())
    }

    fn missing_glib_error() -> Self {
        Self("Tracker call failed without a GError".to_owned())
    }
}

impl fmt::Display for TrackerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TrackerError {}

pub struct Connection {
    pointer: NonNull<c_void>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl Connection {
    /// Connect to a Tracker endpoint on the default session bus.
    ///
    /// A `None` object path selects Tracker's default endpoint path. The
    /// underlying `GDBusConnection *` is intentionally fixed to `NULL`, which
    /// is the only call shape used by Ilia.
    pub fn bus_new(service_name: &str, object_path: Option<&str>) -> Result<Self, TrackerError> {
        let service_name = CString::new(service_name).map_err(|_| TrackerError::interior_nul())?;
        let object_path = object_path
            .map(CString::new)
            .transpose()
            .map_err(|_| TrackerError::interior_nul())?;
        let object_path = object_path
            .as_ref()
            .map_or(ptr::null(), |path| path.as_ptr());
        let mut error = ptr::null_mut();

        // SAFETY: both C strings live through the call, nullable arguments are
        // valid for this API, and ownership of a successful full-transfer
        // result is immediately captured by `Connection`.
        let pointer = unsafe {
            tracker_sparql_connection_bus_new(
                service_name.as_ptr(),
                object_path,
                ptr::null_mut(),
                &mut error,
            )
        };

        NonNull::new(pointer)
            .map(|pointer| Self {
                pointer,
                _not_send_or_sync: PhantomData,
            })
            .ok_or_else(|| unsafe { take_error(error) })
    }

    pub fn query(&self, sparql: &str) -> Result<Cursor, TrackerError> {
        let sparql = CString::new(sparql).map_err(|_| TrackerError::interior_nul())?;
        let mut error = ptr::null_mut();

        // SAFETY: `self` owns a live connection, `sparql` lives through the
        // call, the cancellable may be NULL, and the full-transfer result is
        // immediately captured by `Cursor`.
        let pointer = unsafe {
            tracker_sparql_connection_query(
                self.pointer.as_ptr(),
                sparql.as_ptr(),
                ptr::null_mut(),
                &mut error,
            )
        };

        NonNull::new(pointer)
            .map(|pointer| Cursor {
                pointer,
                _not_send_or_sync: PhantomData,
            })
            .ok_or_else(|| unsafe { take_error(error) })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a full-transfer Tracker constructor
        // and this is its sole owning wrapper.
        unsafe { gobject_sys::g_object_unref(self.pointer.as_ptr().cast::<gobject_sys::GObject>()) }
    }
}

pub struct Cursor {
    pointer: NonNull<c_void>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl Cursor {
    pub fn next(&mut self) -> Result<bool, TrackerError> {
        let mut error = ptr::null_mut();

        // SAFETY: `self` owns a live cursor and both nullable arguments follow
        // Tracker's synchronous cursor contract.
        let has_row = unsafe {
            tracker_sparql_cursor_next(self.pointer.as_ptr(), ptr::null_mut(), &mut error)
        };

        if error.is_null() {
            Ok(has_row != glib_sys::GFALSE)
        } else {
            Err(unsafe { take_error(error) })
        }
    }

    /// Borrow a UTF-8 value from the cursor's current row.
    ///
    /// The result cannot outlive this cursor borrow, so a mutable `next()`
    /// cannot invalidate it while it is in use.
    pub fn get_string(&self, column: i32) -> Option<&str> {
        let mut length: c_long = 0;

        // SAFETY: `self` owns a live cursor and `length` is a valid out pointer.
        // Tracker promises that a non-NULL result is UTF-8 and remains valid
        // until the cursor advances.
        let pointer =
            unsafe { tracker_sparql_cursor_get_string(self.pointer.as_ptr(), column, &mut length) };

        if pointer.is_null() || length < 0 {
            return None;
        }

        // SAFETY: Tracker returned `length` readable bytes tied to this cursor.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length as usize) };
        str::from_utf8(bytes).ok()
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a full-transfer Tracker query result
        // and this is its sole owning wrapper.
        unsafe { gobject_sys::g_object_unref(self.pointer.as_ptr().cast::<gobject_sys::GObject>()) }
    }
}

unsafe fn take_error(error: *mut glib_sys::GError) -> TrackerError {
    if error.is_null() {
        return TrackerError::missing_glib_error();
    }

    // SAFETY: Tracker returned an owned `GError *`; its message is a valid
    // NUL-terminated string for the lifetime of the error.
    let message = unsafe {
        if (*error).message.is_null() {
            "Tracker call failed".to_owned()
        } else {
            CStr::from_ptr((*error).message)
                .to_string_lossy()
                .into_owned()
        }
    };
    // SAFETY: the `GError *` is full-transfer on failure and freed once here.
    unsafe { glib_sys::g_error_free(error) };
    TrackerError(message)
}

extern "C" {
    fn tracker_sparql_connection_bus_new(
        service_name: *const c_char,
        object_path: *const c_char,
        dbus_connection: *mut gio_sys::GDBusConnection,
        error: *mut *mut glib_sys::GError,
    ) -> *mut c_void;

    fn tracker_sparql_connection_query(
        connection: *mut c_void,
        sparql: *const c_char,
        cancellable: *mut gio_sys::GCancellable,
        error: *mut *mut glib_sys::GError,
    ) -> *mut c_void;

    fn tracker_sparql_cursor_next(
        cursor: *mut c_void,
        cancellable: *mut gio_sys::GCancellable,
        error: *mut *mut glib_sys::GError,
    ) -> glib_sys::gboolean;

    fn tracker_sparql_cursor_get_string(
        cursor: *mut c_void,
        column: c_int,
        length: *mut c_long,
    ) -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::TrackerError;

    #[test]
    fn wrapper_errors_have_stable_messages() {
        assert_eq!(
            TrackerError::interior_nul().to_string(),
            "Tracker argument contains an interior NUL byte"
        );
    }
}
