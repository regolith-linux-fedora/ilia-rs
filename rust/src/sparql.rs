//! The SPARQL client library, loaded the first time the Files page searches.
//!
//! tracker 3 ships the library as `libtracker-sparql-3.0.so.0`; since 3.8 it
//! is called TinySPARQL and ships as `libtinysparql-3.0.so.0`, with the same
//! C functions. Loading whichever is present at run time, instead of linking
//! one, lets every other page work when neither is installed and lets one
//! build run on both kinds of systems.

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use glib::translate::{from_glib_full, ToGlibPtr};

/// Tried in this order.
const LIBRARY_NAMES: [&str; 2] = ["libtinysparql-3.0.so.0", "libtracker-sparql-3.0.so.0"];

type GError = glib::ffi::GError;
type GCancellable = gio::ffi::GCancellable;

// TrackerSparqlConnection *tracker_sparql_connection_bus_new (const gchar *service_name,
//     const gchar *object_path, GDBusConnection *dbus_connection, GError **error)
type BusNewFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *mut gio::ffi::GDBusConnection,
    *mut *mut GError,
) -> *mut c_void;
// TrackerSparqlStatement *tracker_sparql_connection_query_statement (
//     TrackerSparqlConnection *connection, const gchar *sparql, GCancellable *cancellable,
//     GError **error)
type QueryStatementFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *mut GCancellable,
    *mut *mut GError,
) -> *mut c_void;
// void tracker_sparql_statement_bind_string (TrackerSparqlStatement *stmt, const gchar *name,
//     const gchar *value)
type BindStringFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);
// TrackerSparqlCursor *tracker_sparql_statement_execute (TrackerSparqlStatement *stmt,
//     GCancellable *cancellable, GError **error)
type ExecuteFn =
    unsafe extern "C" fn(*mut c_void, *mut GCancellable, *mut *mut GError) -> *mut c_void;
// gboolean tracker_sparql_cursor_next (TrackerSparqlCursor *cursor, GCancellable *cancellable,
//     GError **error)
type CursorNextFn =
    unsafe extern "C" fn(*mut c_void, *mut GCancellable, *mut *mut GError) -> glib::ffi::gboolean;
// const gchar *tracker_sparql_cursor_get_string (TrackerSparqlCursor *cursor, gint column,
//     glong *length)
type CursorGetStringFn = unsafe extern "C" fn(*mut c_void, c_int, *mut c_long) -> *const c_char;

struct Library {
    bus_new: BusNewFn,
    query_statement: QueryStatementFn,
    bind_string: BindStringFn,
    execute: ExecuteFn,
    cursor_next: CursorNextFn,
    cursor_get_string: CursorGetStringFn,
}

/// The loaded library, loading it on the first call. The error says why
/// file search is unavailable.
fn library() -> Result<&'static Library, glib::Error> {
    static LIBRARY: OnceLock<Result<Library, String>> = OnceLock::new();
    LIBRARY
        .get_or_init(Library::load)
        .as_ref()
        .map_err(|reason| {
            glib::Error::new(
                gio::IOErrorEnum::NotFound,
                &format!("File search is unavailable: {reason}"),
            )
        })
}

impl Library {
    fn load() -> Result<Self, String> {
        let mut errors = Vec::new();
        for name in LIBRARY_NAMES {
            match Self::open(name) {
                Ok(library) => return Ok(library),
                Err(error) => errors.push(error),
            }
        }
        Err(format!(
            "neither {} nor {} could be loaded ({})",
            LIBRARY_NAMES[0],
            LIBRARY_NAMES[1],
            errors.join("; ")
        ))
    }

    fn open(name: &str) -> Result<Self, String> {
        let name = CString::new(name).map_err(|error| error.to_string())?;
        // SAFETY: `name` is a NUL-terminated library name. The handle is
        // never closed once the symbols are taken, so the function pointers
        // stay valid for the life of the process.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(dl_error());
        }
        // SAFETY: the handle is live.
        let library = unsafe { Self::symbols(handle) };
        if library.is_err() {
            // SAFETY: the handle came from dlopen and nothing from it is kept.
            unsafe { libc::dlclose(handle) };
        }
        library
    }

    /// # Safety
    ///
    /// `handle` must be a live `dlopen` handle.
    unsafe fn symbols(handle: *mut c_void) -> Result<Self, String> {
        // SAFETY: each symbol is looked up with the C signature declared
        // above for the TinySPARQL function of that name.
        unsafe {
            Ok(Self {
                bus_new: symbol(handle, c"tracker_sparql_connection_bus_new")?,
                query_statement: symbol(handle, c"tracker_sparql_connection_query_statement")?,
                bind_string: symbol(handle, c"tracker_sparql_statement_bind_string")?,
                execute: symbol(handle, c"tracker_sparql_statement_execute")?,
                cursor_next: symbol(handle, c"tracker_sparql_cursor_next")?,
                cursor_get_string: symbol(handle, c"tracker_sparql_cursor_get_string")?,
            })
        }
    }
}

/// Looks up a function in a library opened with `dlopen`.
///
/// # Safety
///
/// `F` must be the `extern "C"` function pointer type matching the symbol.
unsafe fn symbol<F: Copy>(handle: *mut c_void, name: &CStr) -> Result<F, String> {
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    // SAFETY: `handle` is a live dlopen handle and `name` is NUL-terminated.
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if address.is_null() {
        Err(dl_error())
    } else {
        // SAFETY: the caller guarantees `F` is the symbol's function type,
        // and function pointers have the size of a data pointer here.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
    }
}

/// The last dynamic loader error.
fn dl_error() -> String {
    // SAFETY: dlerror returns NULL or a NUL-terminated message that stays
    // valid until the next dl* call on this thread; it is copied at once.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_owned()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Takes ownership of a GError set by a failed call.
///
/// # Safety
///
/// `error` must be NULL or a GError the caller owns.
unsafe fn take_error(error: *mut GError) -> glib::Error {
    if error.is_null() {
        glib::Error::new(gio::IOErrorEnum::Failed, "SPARQL call failed")
    } else {
        // SAFETY: an owned GError, freed by the wrapper.
        unsafe { from_glib_full(error) }
    }
}

/// An owned GObject reference.
struct Object(NonNull<c_void>);

impl Object {
    /// # Safety
    ///
    /// `pointer` must be NULL or a GObject reference the caller owns.
    unsafe fn take(pointer: *mut c_void, error: *mut GError) -> Result<Self, glib::Error> {
        match NonNull::new(pointer) {
            Some(pointer) => Ok(Self(pointer)),
            // SAFETY: a failed call hands its GError to the caller.
            None => Err(unsafe { take_error(error) }),
        }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: the reference is owned by this wrapper alone.
        unsafe { glib::gobject_ffi::g_object_unref(self.as_ptr().cast()) }
    }
}

/// A connection to a SPARQL endpoint on the session bus.
pub struct Connection {
    library: &'static Library,
    connection: Object,
}

// SAFETY: a Tracker connection is not tied to the thread that made it, and
// its reference count is atomic. ilia uses a connection from one thread at
// a time; cursors, which borrow it, stay on the thread that queried.
unsafe impl Send for Connection {}

impl Connection {
    /// Connects to `service_name` at its default object path.
    pub fn bus_new(service_name: &str) -> Result<Self, glib::Error> {
        let library = library()?;
        let service_name = c_string(service_name)?;
        let mut error = ptr::null_mut();
        // SAFETY: valid C strings; NULL selects the default object path and
        // the session bus. A non-NULL result is a full-transfer reference.
        let connection = unsafe {
            let pointer = (library.bus_new)(
                service_name.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                &mut error,
            );
            Object::take(pointer, error)?
        };
        Ok(Self {
            library,
            connection,
        })
    }

    /// Runs a query with `~name` parameters bound to strings; values never
    /// become part of the SPARQL text.
    pub fn query(
        &self,
        sparql: &str,
        bindings: &[(&str, &str)],
        cancellable: &gio::Cancellable,
    ) -> Result<Cursor<'_>, glib::Error> {
        let library = self.library;
        let sparql = c_string(sparql)?;
        let cancellable_ptr: *mut GCancellable = cancellable.to_glib_none().0;
        let mut error = ptr::null_mut();
        // SAFETY: the connection is live, the strings outlive the calls and
        // the cancellable is a live GCancellable. Results are full-transfer.
        unsafe {
            let pointer = (library.query_statement)(
                self.connection.as_ptr(),
                sparql.as_ptr(),
                cancellable_ptr,
                &mut error,
            );
            let statement = Object::take(pointer, error)?;
            for (name, value) in bindings {
                let (name, value) = (c_string(name)?, c_string(value)?);
                (library.bind_string)(statement.as_ptr(), name.as_ptr(), value.as_ptr());
            }
            let mut error = ptr::null_mut();
            let pointer = (library.execute)(statement.as_ptr(), cancellable_ptr, &mut error);
            let cursor = Object::take(pointer, error)?;
            Ok(Cursor {
                library,
                cursor,
                _statement: statement,
                cancellable: cancellable.clone(),
                _connection: PhantomData,
            })
        }
    }
}

/// The rows of a query, read in order.
pub struct Cursor<'a> {
    library: &'static Library,
    // Declared first, so it is released before its statement.
    cursor: Object,
    _statement: Object,
    cancellable: gio::Cancellable,
    _connection: PhantomData<&'a Connection>,
}

impl Cursor<'_> {
    /// Moves to the next row; `false` after the last one.
    pub fn next(&mut self) -> Result<bool, glib::Error> {
        let mut error = ptr::null_mut();
        // SAFETY: the cursor and the cancellable are live.
        let has_row = unsafe {
            (self.library.cursor_next)(
                self.cursor.as_ptr(),
                self.cancellable.to_glib_none().0,
                &mut error,
            )
        };
        if error.is_null() {
            Ok(has_row != glib::ffi::GFALSE)
        } else {
            // SAFETY: the call failed and handed its GError over.
            Err(unsafe { take_error(error) })
        }
    }

    /// A column of the current row, when it is bound and UTF-8.
    pub fn get_string(&self, column: i32) -> Option<&str> {
        let mut length: c_long = 0;
        // SAFETY: the cursor is live and `length` is a valid out pointer. A
        // non-NULL result holds `length` bytes valid until the cursor moves,
        // which needs `&mut self`.
        unsafe {
            let pointer =
                (self.library.cursor_get_string)(self.cursor.as_ptr(), column, &mut length);
            if pointer.is_null() || length < 0 {
                return None;
            }
            std::str::from_utf8(std::slice::from_raw_parts(
                pointer.cast::<u8>(),
                length as usize,
            ))
            .ok()
        }
    }
}

fn c_string(value: &str) -> Result<CString, glib::Error> {
    CString::new(value).map_err(|_| {
        glib::Error::new(
            gio::IOErrorEnum::InvalidArgument,
            "SPARQL argument contains a NUL byte",
        )
    })
}
