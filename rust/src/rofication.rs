use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

use crate::ipc::{member, Json, JsonParser};

// The Vala client hardcodes the daemon path; it is never configurable.
pub const SOCKET_PATH: &str = "/tmp/rofi_notification_daemon";
const BUFFER_SIZE: usize = 1024 * 8;

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
}

impl NotificationDesc {
    fn from_json(value: &Json) -> Result<Self, String> {
        let object = value.object()?;
        let string = |name: &str| -> Result<String, String> {
            Ok(member(object, name)?.string()?.to_owned())
        };
        Ok(Self {
            id: int_member(member(object, "id")?)?,
            summary: string("summary")?,
            body: string("body")?,
            application: string("application")?,
            icon: string("icon")?,
            urgency: int_member(member(object, "urgency")?)?,
        })
    }
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

pub fn parse_notifications(payload: &[u8]) -> Result<Vec<NotificationDesc>, String> {
    JsonParser::new(payload)
        .parse()?
        .array()?
        .iter()
        .map(NotificationDesc::from_json)
        .collect()
}

pub struct RoficationClient {
    socket_path: &'static str,
}

impl RoficationClient {
    pub fn new(socket_path: &'static str) -> Self {
        Self { socket_path }
    }

    // One blocking connection per command, mirroring the Vala client.
    fn open_socket(&self) -> Result<UnixStream, String> {
        UnixStream::connect(self.socket_path).map_err(|error| io_message(&error))
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
/// `(os error N)` suffix.
fn io_message(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(index) if text.ends_with(')') => text[..index].to_owned(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{delete_many_request, delete_request, list_request, parse_notifications};

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
}
