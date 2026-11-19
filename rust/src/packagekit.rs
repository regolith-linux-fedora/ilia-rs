//! A small client for the PackageKit service on the system bus, its 1.x
//! D-Bus interface: one transaction lists the packages, installs or removes
//! some, trial runs included, and reports what happens as [`Event`]s.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use glib::prelude::ToVariant;

const BUS_NAME: &str = "org.freedesktop.PackageKit";
const OBJECT_PATH: &str = "/org/freedesktop/PackageKit";
const INTERFACE: &str = "org.freedesktop.PackageKit";
const TRANSACTION_INTERFACE: &str = "org.freedesktop.PackageKit.Transaction";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
/// The calls only start work and return at once, so a call left without a
/// reply this long has no service behind it.
const CALL_TIMEOUT_MS: i32 = 25_000;

/// The `newest` filter: the latest version of each package only.
pub const FILTER_NEWEST: u64 = 1 << 16;
/// The `arch` filter: packages of the system's architecture only.
pub const FILTER_ARCH: u64 = 1 << 18;

/// The `only-trusted` transaction flag.
pub const FLAG_ONLY_TRUSTED: u64 = 1 << 1;
/// The `simulate` transaction flag: report what would change, change nothing.
pub const FLAG_SIMULATE: u64 = 1 << 2;

/// Package states of the `Package` signal.
pub const INFO_INSTALLED: u32 = 1;
pub const INFO_REMOVING: u32 = 13;
pub const INFO_OBSOLETING: u32 = 15;

/// Exit codes of the `Finished` signal.
pub const EXIT_SUCCESS: u32 = 1;
pub const EXIT_CANCELLED: u32 = 3;
pub const EXIT_CANCELLED_PRIORITY: u32 = 9;

/// The transaction status while the service waits for the user to
/// authorise it.
pub const STATUS_WAITING_FOR_AUTH: u32 = 31;

/// Error codes that only announce a cancellation.
const ERROR_TRANSACTION_CANCELLED: u32 = 17;
const ERROR_CANCELLED_PRIORITY: u32 = 65;

/// Asks the service to tell a package list with one signal instead of one
/// signal per package.
pub const HINT_PLURAL_SIGNALS: &str = "supports-plural-signals=true";
/// Lets the service ask the user to authorise the transaction.
pub const HINT_INTERACTIVE: &str = "interactive=true";

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A package the transaction lists, or would change.
    Package {
        info: u32,
        id: String,
        summary: String,
    },
    /// The transaction failed, or will, for this reason.
    Error {
        code: u32,
        details: String,
    },
    /// Progress, 101 when unknown.
    Percentage(u32),
    AllowCancel(bool),
    Status(u32),
    /// The transaction ended with this exit code; no event follows.
    Finished(u32),
    /// The service could not be reached or refused the request; no event
    /// follows.
    Failed(String),
}

struct Inner {
    handler: Box<dyn Fn(Event)>,
    connection: RefCell<Option<gio::DBusConnection>>,
    /// The unique bus name of the service and the transaction's object path.
    transaction: RefCell<Option<(String, String)>>,
    subscriptions: RefCell<Vec<gio::SignalSubscriptionId>>,
    ended: Cell<bool>,
}

/// One PackageKit transaction. The transaction runs to its end whether the
/// handle is kept or not; keep it to cancel it.
#[derive(Clone)]
pub struct Transaction {
    inner: Rc<Inner>,
}

impl Transaction {
    /// Creates a transaction with `hints` and starts it with `method` and
    /// its `parameters`. `handler` gets the transaction's events, ending
    /// with [`Event::Finished`] or [`Event::Failed`], always from the main
    /// loop, never from within this call.
    pub fn start(
        hints: &[&str],
        method: &'static str,
        parameters: glib::Variant,
        handler: impl Fn(Event) + 'static,
    ) -> Self {
        let transaction = Self {
            inner: Rc::new(Inner {
                handler: Box::new(handler),
                connection: RefCell::default(),
                transaction: RefCell::default(),
                subscriptions: RefCell::default(),
                ended: Cell::new(false),
            }),
        };
        let hints: Vec<String> = hints.iter().map(|hint| (*hint).to_owned()).collect();
        let inner = transaction.inner.clone();
        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(connection) => inner.create(connection, hints, method, parameters),
                Err(error) => inner.fail(&error),
            },
        );
        transaction
    }

    /// Asks the service to cancel the transaction.
    pub fn cancel(&self) {
        let inner = &self.inner;
        if inner.ended.get() {
            return;
        }
        let connection = inner.connection.borrow().clone();
        let transaction = inner.transaction.borrow().clone();
        if let (Some(connection), Some((owner, path))) = (connection, transaction) {
            connection.call(
                Some(&owner),
                &path,
                TRANSACTION_INTERFACE,
                "Cancel",
                None,
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                |result| {
                    if let Err(error) = result {
                        eprintln!(
                            "Cannot cancel the package transaction: {}",
                            error_text(&error)
                        );
                    }
                },
            );
        }
    }
}

impl Inner {
    fn create(
        self: Rc<Self>,
        connection: gio::DBusConnection,
        hints: Vec<String>,
        method: &'static str,
        parameters: glib::Variant,
    ) {
        self.connection.replace(Some(connection.clone()));
        let message = gio::DBusMessage::new_method_call(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "CreateTransaction",
        );
        // The reply tells the service's unique name: the transaction's
        // signals are taken from it alone.
        let connection_for_reply = connection.clone();
        connection.send_message_with_reply(
            &message,
            gio::DBusSendMessageFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |result| {
                let reply = match result.and_then(|reply| reply.to_gerror().map(|()| reply)) {
                    Ok(reply) => reply,
                    Err(error) => return self.fail(&error),
                };
                let path = reply
                    .body()
                    .and_then(|body| body.try_child_value(0))
                    .and_then(|path| path.str().map(str::to_owned));
                let Some(path) = path else {
                    return self.end(Event::Failed(
                        "The package service created no transaction".to_owned(),
                    ));
                };
                let owner = reply
                    .sender()
                    .map_or_else(|| BUS_NAME.to_owned(), |sender| sender.to_string());
                self.subscribe(&connection_for_reply, &owner, &path);
                self.transaction.replace(Some((owner, path)));
                self.run(hints, method, parameters);
            },
        );
    }

    /// Follows the transaction's signals and property changes. Subscribing
    /// before starting it misses none.
    fn subscribe(self: &Rc<Self>, connection: &gio::DBusConnection, owner: &str, path: &str) {
        let inner = self.clone();
        let signals = connection.signal_subscribe(
            Some(owner),
            Some(TRANSACTION_INTERFACE),
            None,
            Some(path),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, signal, parameters| inner.signal(signal, parameters),
        );
        let inner = self.clone();
        let properties = connection.signal_subscribe(
            Some(owner),
            Some(PROPERTIES_INTERFACE),
            Some("PropertiesChanged"),
            Some(path),
            Some(TRANSACTION_INTERFACE),
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| inner.properties_changed(parameters),
        );
        self.subscriptions
            .borrow_mut()
            .extend([signals, properties]);
    }

    /// Sets the hints, then calls the method that starts the transaction.
    fn run(self: Rc<Self>, hints: Vec<String>, method: &'static str, parameters: glib::Variant) {
        let inner = self.clone();
        self.call("SetHints", Some((hints,).to_variant()), move |result| {
            if let Err(error) = result {
                return inner.fail(&error);
            }
            let inner_for_method = inner.clone();
            inner.call(method, Some(parameters), move |result| {
                if let Err(error) = result {
                    inner_for_method.fail(&error);
                }
            });
        });
    }

    fn call(
        &self,
        method: &str,
        parameters: Option<glib::Variant>,
        callback: impl FnOnce(Result<glib::Variant, glib::Error>) + 'static,
    ) {
        let connection = self.connection.borrow().clone();
        let transaction = self.transaction.borrow().clone();
        let (Some(connection), Some((owner, path))) = (connection, transaction) else {
            return;
        };
        connection.call(
            Some(&owner),
            &path,
            TRANSACTION_INTERFACE,
            method,
            parameters.as_ref(),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            callback,
        );
    }

    fn signal(&self, signal: &str, parameters: &glib::Variant) {
        match signal {
            "Package" => {
                if let Some((info, id, summary)) = parameters.get::<(u32, String, String)>() {
                    self.emit(Event::Package { info, id, summary });
                }
            }
            "Packages" => {
                if let Some((packages,)) = parameters.get::<(Vec<(u32, String, String)>,)>() {
                    for (info, id, summary) in packages {
                        self.emit(Event::Package { info, id, summary });
                    }
                }
            }
            "ErrorCode" => {
                if let Some((code, details)) = parameters.get::<(u32, String)>() {
                    self.emit(Event::Error { code, details });
                }
            }
            "Finished" => {
                if let Some((exit, _runtime)) = parameters.get::<(u32, u32)>() {
                    self.end(Event::Finished(exit));
                }
            }
            _ => {}
        }
    }

    fn properties_changed(&self, parameters: &glib::Variant) {
        let Some((_, changed, _)) =
            parameters.get::<(String, HashMap<String, glib::Variant>, Vec<String>)>()
        else {
            return;
        };
        // In a fixed order: a reply can change several at once.
        if let Some(allow) = changed
            .get("AllowCancel")
            .and_then(|value| value.get::<bool>())
        {
            self.emit(Event::AllowCancel(allow));
        }
        if let Some(status) = changed.get("Status").and_then(|value| value.get::<u32>()) {
            self.emit(Event::Status(status));
        }
        if let Some(percentage) = changed
            .get("Percentage")
            .and_then(|value| value.get::<u32>())
        {
            self.emit(Event::Percentage(percentage));
        }
    }

    fn emit(&self, event: Event) {
        if !self.ended.get() {
            (self.handler)(event);
        }
    }

    fn fail(&self, error: &glib::Error) {
        self.end(Event::Failed(error_text(error)));
    }

    /// Reports the last event and stops following the transaction.
    fn end(&self, event: Event) {
        if self.ended.replace(true) {
            return;
        }
        let subscriptions: Vec<_> = self.subscriptions.borrow_mut().drain(..).collect();
        if let Some(connection) = self.connection.borrow().as_ref() {
            for subscription in subscriptions {
                connection.signal_unsubscribe(subscription);
            }
        }
        (self.handler)(event);
    }
}

/// Whether an error code only announces that the transaction was cancelled.
pub fn is_cancellation_error(code: u32) -> bool {
    matches!(code, ERROR_TRANSACTION_CANCELLED | ERROR_CANCELLED_PRIORITY)
}

/// Whether an exit code means the transaction was cancelled.
pub fn is_cancelled_exit(exit: u32) -> bool {
    matches!(exit, EXIT_CANCELLED | EXIT_CANCELLED_PRIORITY)
}

/// What an unsuccessful exit code means, for a transaction that ended
/// without telling why.
pub fn exit_text(exit: u32) -> &'static str {
    match exit {
        2 => "failed",
        3 => "cancelled",
        4 => "key required",
        5 => "licence agreement required",
        6 => "killed",
        7 => "media change required",
        8 => "untrusted packages needed",
        9 => "cancelled by a more important transaction",
        10 => "skipped",
        11 => "repair required",
        _ => "unknown",
    }
}

/// The text of an error, without the remote error name GDBus puts in front
/// of the message a service sent.
fn error_text(error: &glib::Error) -> String {
    strip_remote_error(error.message()).to_owned()
}

fn strip_remote_error(message: &str) -> &str {
    message
        .strip_prefix("GDBus.Error:")
        .and_then(|rest| rest.split_once(": "))
        .map_or(message, |(_, text)| text)
}

#[cfg(test)]
mod tests {
    use super::{exit_text, is_cancellation_error, is_cancelled_exit, strip_remote_error};

    #[test]
    fn remote_error_names_are_stripped() {
        assert_eq!(
            strip_remote_error(
                "GDBus.Error:org.freedesktop.PackageKit.Failed: Package request refused: try later"
            ),
            "Package request refused: try later"
        );
        assert_eq!(
            strip_remote_error("Could not connect: No such file or directory"),
            "Could not connect: No such file or directory"
        );
        assert_eq!(
            strip_remote_error("GDBus.Error:org.freedesktop.DBus.Error.NoReply"),
            "GDBus.Error:org.freedesktop.DBus.Error.NoReply"
        );
    }

    #[test]
    fn cancellations_are_recognised() {
        assert!(is_cancellation_error(17));
        assert!(is_cancellation_error(65));
        assert!(!is_cancellation_error(4));
        assert!(is_cancelled_exit(3));
        assert!(is_cancelled_exit(9));
        assert!(!is_cancelled_exit(2));
        assert_eq!(exit_text(2), "failed");
        assert_eq!(exit_text(99), "unknown");
    }
}
