use std::cmp::Ordering;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::ipc::{member, Json, JsonParser};

// The Vala client hardcodes the daemon path; it is never configurable.
pub const SOCKET_PATH: &str = "/tmp/rofi_notification_daemon";
// How long to wait on the daemon: one that accepts the connection but never
// answers must not hang ilia.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const BUFFER_SIZE: usize = 1024 * 8;
// The freedesktop urgency levels are low (0), normal (1) and critical (2).
const NORMAL_URGENCY: i64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct NotificationDesc {
    pub id: i64,
    pub summary: String,
    pub body: String,
    pub application: String,
    #[allow(dead_code)]
    pub icon: String,
    #[allow(dead_code)]
    pub urgency: i64,
    /// When the notification last arrived, in seconds since the epoch; zero
    /// when the daemon did not say.
    pub timestamp: f64,
}

impl NotificationDesc {
    // rofication leaves out the members it has no value for, so only the id,
    // which deleting a notification needs, is required.
    fn from_json(value: &Json) -> Result<Self, String> {
        let object = value.object()?;
        let string = |name: &str| -> Result<String, String> {
            match object.get(name) {
                None | Some(Json::Null) => Ok(String::new()),
                Some(value) => Ok(value.string()?.to_owned()),
            }
        };
        Ok(Self {
            id: int_member(member(object, "id")?)?,
            summary: string("summary")?,
            body: string("body")?,
            application: string("application")?,
            icon: string("icon")?,
            urgency: match object.get("urgency") {
                None | Some(Json::Null) => NORMAL_URGENCY,
                Some(value) => int_member(value)?,
            },
            // A float from current daemons; older ones send none or "".
            timestamp: match object.get("timestamp") {
                Some(Json::Number(text)) => text.parse().unwrap_or_default(),
                _ => 0.0,
            },
        })
    }
}

/// Newest first. The daemon lists notifications in the order they were first
/// added, and a replaced notification keeps its id and its place but gets a
/// new timestamp, so order by timestamp, then by id. Notifications without a
/// timestamp come last.
pub fn newest_first(a: &NotificationDesc, b: &NotificationDesc) -> Ordering {
    b.timestamp
        .total_cmp(&a.timestamp)
        .then_with(|| b.id.cmp(&a.id))
}

// json-glib's get_int_member truncates doubles toward zero.
fn int_member(value: &Json) -> Result<i64, String> {
    match value {
        Json::Number(text) if !text.contains(['.', 'e', 'E']) => {
            text.parse::<i64>().map_err(|error| error.to_string())
        }
        Json::Number(text) => text
            .parse::<f64>()
            .map(|number| number as i64)
            .map_err(|error| error.to_string()),
        _ => Err("expected JSON integer".to_owned()),
    }
}

pub fn list_request() -> &'static [u8] {
    b"list\n"
}

pub fn delete_request(id: i64) -> String {
    format!("del:{id}\n")
}

// Null entries are skipped when joining, so an all-null (or empty) id set
// still yields the bare `delm:\n` command.
pub fn delete_many_request(ids: &[Option<String>]) -> String {
    let joined: Vec<&str> = ids.iter().flatten().map(String::as_str).collect();
    format!("delm:{}\n", joined.join(","))
}

/// Decodes the daemon's list. A notification that cannot be decoded is
/// reported and left out; it does not take the others with it.
pub fn parse_notifications(payload: &[u8]) -> Result<Vec<NotificationDesc>, String> {
    Ok(JsonParser::new(payload)
        .parse()?
        .array()?
        .iter()
        .filter_map(|value| match NotificationDesc::from_json(value) {
            Ok(notification) => Some(notification),
            Err(error) => {
                eprintln!("Error: skipping a notification: {error}");
                None
            }
        })
        .collect())
}

#[derive(Clone, Copy)]
pub struct RoficationClient {
    socket_path: &'static str,
}

impl RoficationClient {
    pub fn new(socket_path: &'static str) -> Self {
        Self { socket_path }
    }

    // One blocking connection per command, mirroring the Vala client.
    fn open_socket(&self) -> Result<UnixStream, String> {
        let socket = UnixStream::connect(self.socket_path).map_err(|error| io_message(&error))?;
        socket
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .and_then(|()| socket.set_write_timeout(Some(SOCKET_TIMEOUT)))
            .map_err(|error| io_message(&error))?;
        Ok(socket)
    }

    pub fn get_notifications(&self) -> Result<Vec<NotificationDesc>, String> {
        let mut socket = self.open_socket()?;
        socket
            .write_all(list_request())
            .map_err(|error| io_message(&error))?;

        // Read until EOF, concatenating every 8 KiB chunk.
        let mut payload = Vec::new();
        let mut buffer = [0u8; BUFFER_SIZE];
        loop {
            match socket.read(&mut buffer) {
                Ok(0) => break,
                Ok(len) => payload.extend_from_slice(&buffer[..len]),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_message(&error)),
            }
        }
        drop(socket);

        parse_notifications(&payload)
    }

    pub fn delete_notification_by_id(&self, id: i64) -> Result<(), String> {
        self.send(delete_request(id).as_bytes())
    }

    pub fn delete_notifications_by_ids(&self, ids: &[Option<String>]) -> Result<(), String> {
        let command = delete_many_request(ids);
        self.send(command.as_bytes())
    }

    // Send a command and close without reading a reply.
    fn send(&self, command: &[u8]) -> Result<(), String> {
        let mut socket = self.open_socket()?;
        socket
            .write_all(command)
            .map_err(|error| io_message(&error))
    }
}

/// The OS error text the way GIO reports it, without Rust's
/// `(os error N)` suffix. A daemon that does not answer in time is
/// reported in GIO's words, not as EAGAIN.
fn io_message(error: &std::io::Error) -> String {
    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        return "Socket I/O timed out".to_owned();
    }
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(index) if text.ends_with(')') => text[..index].to_owned(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delete_many_request, delete_request, io_message, list_request, newest_first,
        parse_notifications,
    };
    use std::io::{Error, ErrorKind};

    #[test]
    fn io_errors_read_like_gio() {
        let missing = Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(io_message(&missing), "No such file or directory");
        // A read timeout on a blocking socket is EAGAIN.
        let silent = Error::from_raw_os_error(libc::EAGAIN);
        assert_eq!(io_message(&silent), "Socket I/O timed out");
        assert_eq!(
            io_message(&Error::from(ErrorKind::TimedOut)),
            "Socket I/O timed out"
        );
    }

    #[test]
    fn command_framing_matches_vala_bytes() {
        assert_eq!(list_request(), b"list\n");
        assert_eq!(delete_request(11).as_bytes(), b"del:11\n");
        assert_eq!(delete_request(-3), "del:-3\n");
    }

    #[test]
    fn delete_many_joins_ids_and_skips_nulls() {
        let mut ids: Vec<Option<String>> = vec![None; 10];
        ids.extend(["22", "11", "7"].map(|id| Some(id.to_owned())));
        assert_eq!(delete_many_request(&ids), "delm:22,11,7\n");
        assert_eq!(
            delete_many_request(&[Some("5".to_owned())]).as_bytes(),
            b"delm:5\n"
        );
    }

    #[test]
    fn delete_many_with_empty_visible_set_sends_bare_command() {
        assert_eq!(delete_many_request(&vec![None; 10]).as_bytes(), b"delm:\n");
        assert_eq!(delete_many_request(&[]), "delm:\n");
    }

    #[test]
    fn notification_payload_parses_all_fields() {
        let notifications = parse_notifications(
            br#"[{"id": 2, "summary": "Hello! January", "body": "", "application": "notify-send", "icon": "face-wink", "urgency": 1, "actions": [], "hints": {"urgency": 1}}]"#,
        )
        .unwrap();
        assert_eq!(notifications.len(), 1);
        let notification = &notifications[0];
        assert_eq!(notification.id, 2);
        assert_eq!(notification.summary, "Hello! January");
        assert_eq!(notification.body, "");
        assert_eq!(notification.application, "notify-send");
        assert_eq!(notification.icon, "face-wink");
        assert_eq!(notification.urgency, 1);
        assert!(parse_notifications(b"[]").unwrap().is_empty());
        assert!(parse_notifications(b"{}").is_err());
    }

    #[test]
    fn notifications_sort_newest_first_by_timestamp_then_id() {
        let mut notifications = parse_notifications(
            br#"[{"id": 1, "timestamp": 100.5}, {"id": 2, "timestamp": 300.25},
                {"id": 3, "timestamp": 200}, {"id": 4}, {"id": 5, "timestamp": ""},
                {"id": 6, "timestamp": 200}]"#,
        )
        .unwrap();
        notifications.sort_by(newest_first);
        let ids: Vec<i64> = notifications.iter().map(|n| n.id).collect();
        assert_eq!(ids, [2, 6, 3, 1, 5, 4]);
    }

    #[test]
    fn missing_members_leave_the_other_notifications_listed() {
        let notifications = parse_notifications(
            br#"[{"id": 1, "summary": "one", "body": "b", "application": "a", "icon": "i", "urgency": 2},
                {"id": 2, "summary": "no icon or body", "application": "a"},
                {"summary": "no id"},
                {"id": 4, "summary": 4},
                {"id": 5}]"#,
        )
        .unwrap();
        let ids: Vec<i64> = notifications.iter().map(|n| n.id).collect();
        assert_eq!(ids, [1, 2, 5]);
        let partial = &notifications[1];
        assert_eq!(partial.summary, "no icon or body");
        assert_eq!(partial.body, "");
        assert_eq!(partial.icon, "");
        assert_eq!(partial.urgency, 1);
        assert_eq!(notifications[2].application, "");
    }
}
