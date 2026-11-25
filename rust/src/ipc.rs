use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::iter::Peekable;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::str::Chars;
use std::time::Duration;

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_SIZE: usize = 14;

pub const RUN_COMMAND: u32 = 0;
pub const GET_WORKSPACES: u32 = 1;
pub const SUBSCRIBE: u32 = 2;
pub const GET_TREE: u32 = 4;
pub const GET_VERSION: u32 = 7;
pub const GET_CONFIG: u32 = 9;
pub const GET_BINDING_STATE: u32 = 12;

/// Events carry their type with the highest bit set; replies do not.
const EVENT_FLAG: u32 = 1 << 31;
/// The event sent when the window manager runs a binding.
pub const BINDING_EVENT: u32 = EVENT_FLAG | 5;

/// How long a request waits for the window manager, unless the caller says.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub(crate) fn object(&self) -> Result<&BTreeMap<String, Json>, String> {
        match self {
            Self::Object(value) => Ok(value),
            _ => Err("expected JSON object".to_owned()),
        }
    }

    pub(crate) fn array(&self) -> Result<&[Json], String> {
        match self {
            Self::Array(value) => Ok(value),
            _ => Err("expected JSON array".to_owned()),
        }
    }

    pub(crate) fn string(&self) -> Result<&str, String> {
        match self {
            Self::String(value) => Ok(value),
            _ => Err("expected JSON string".to_owned()),
        }
    }

    fn bool(&self) -> Result<bool, String> {
        match self {
            Self::Bool(value) => Ok(*value),
            _ => Err("expected JSON boolean".to_owned()),
        }
    }

    fn integer_string(&self) -> Result<String, String> {
        match self {
            Self::Number(value) if !value.contains(['.', 'e', 'E']) => Ok(value.clone()),
            Self::String(value) => Ok(value.clone()),
            _ => Err("expected JSON integer".to_owned()),
        }
    }
}

pub(crate) fn member<'a>(
    object: &'a BTreeMap<String, Json>,
    name: &str,
) -> Result<&'a Json, String> {
    object
        .get(name)
        .ok_or_else(|| format!("missing JSON member: {name}"))
}

pub(crate) struct JsonParser<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JsonParser<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn parse(mut self) -> Result<Json, String> {
        self.skip_space();
        let value = self.value()?;
        self.skip_space();
        if self.offset == self.bytes.len() {
            Ok(value)
        } else {
            Err(format!("unexpected JSON byte at {}", self.offset))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_space();
        match self.peek() {
            Some(b'{') => self.object().map(Json::Object),
            Some(b'[') => self.array().map(Json::Array),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Json::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number().map(Json::Number),
            Some(byte) => Err(format!("unexpected JSON byte {byte} at {}", self.offset)),
            None => Err("unexpected end of JSON".to_owned()),
        }
    }

    fn object(&mut self) -> Result<BTreeMap<String, Json>, String> {
        self.expect(b'{')?;
        self.skip_space();
        let mut result = BTreeMap::new();
        if self.take(b'}') {
            return Ok(result);
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            self.expect(b':')?;
            result.insert(key, self.value()?);
            self.skip_space();
            if self.take(b'}') {
                return Ok(result);
            }
            self.expect(b',')?;
        }
    }

    fn array(&mut self) -> Result<Vec<Json>, String> {
        self.expect(b'[')?;
        self.skip_space();
        let mut result = Vec::new();
        if self.take(b']') {
            return Ok(result);
        }
        loop {
            result.push(self.value()?);
            self.skip_space();
            if self.take(b']') {
                return Ok(result);
            }
            self.expect(b',')?;
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut result = String::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| "unterminated JSON string".to_owned())?;
            match byte {
                b'"' => return Ok(result),
                b'\\' => match self
                    .next()
                    .ok_or_else(|| "unterminated JSON escape".to_owned())?
                {
                    b'"' => result.push('"'),
                    b'\\' => result.push('\\'),
                    b'/' => result.push('/'),
                    b'b' => result.push('\u{0008}'),
                    b'f' => result.push('\u{000c}'),
                    b'n' => result.push('\n'),
                    b'r' => result.push('\r'),
                    b't' => result.push('\t'),
                    b'u' => result.push(self.unicode_escape()?),
                    other => return Err(format!("invalid JSON escape: {other}")),
                },
                0..=31 => return Err("control byte in JSON string".to_owned()),
                32..=127 => result.push(char::from(byte)),
                _ => {
                    let start = self.offset - 1;
                    let width = utf8_width(byte)?;
                    let end = start + width;
                    if end > self.bytes.len() {
                        return Err("truncated UTF-8 in JSON string".to_owned());
                    }
                    let text = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|error| error.to_string())?;
                    result.push_str(text);
                    self.offset = end;
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, String> {
        let first = self.hex_quad()?;
        let codepoint = if (0xd800..=0xdbff).contains(&first) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err("invalid JSON surrogate pair".to_owned());
            }
            0x10000 + (((first - 0xd800) as u32) << 10) + (second - 0xdc00) as u32
        } else {
            first as u32
        };
        char::from_u32(codepoint).ok_or_else(|| "invalid JSON unicode escape".to_owned())
    }

    fn hex_quad(&mut self) -> Result<u16, String> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self
                .next()
                .ok_or_else(|| "truncated JSON unicode escape".to_owned())?;
            let digit = hex_value(byte)? as u16;
            value = value
                .checked_mul(16)
                .and_then(|value| value.checked_add(digit))
                .ok_or_else(|| "invalid JSON unicode escape".to_owned())?;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<String, String> {
        let start = self.offset;
        if self.take(b'-') && !matches!(self.peek(), Some(b'0'..=b'9')) {
            return Err("invalid JSON number".to_owned());
        }
        if self.take(b'0') {
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("invalid leading zero in JSON number".to_owned());
            }
        } else {
            self.digits()?;
        }
        if self.take(b'.') {
            self.digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.offset += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            self.digits()?;
        }
        String::from_utf8(self.bytes[start..self.offset].to_vec())
            .map_err(|error| error.to_string())
    }

    fn digits(&mut self) -> Result<(), String> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        if self.offset == start {
            Err("expected JSON digit".to_owned())
        } else {
            Ok(())
        }
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), String> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err("invalid JSON literal".to_owned())
        }
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.offset += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.take(expected) {
            Ok(())
        } else {
            Err(format!("expected JSON byte {expected} at {}", self.offset))
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let result = self.peek();
        self.offset += usize::from(result.is_some());
        result
    }
}

fn utf8_width(first: u8) -> Result<usize, String> {
    match first {
        0xc2..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf4 => Ok(4),
        _ => Err("invalid UTF-8 in JSON string".to_owned()),
    }
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid hex digit in JSON escape".to_owned()),
    }
}

fn request(wm_name: &str, command: u32, payload: &str) -> Result<Json, String> {
    Connection::open(&wm_socket(wm_name)?, REQUEST_TIMEOUT)?.request(command, payload)
}

/// The socket the window manager is reached on, as the session names it.
pub fn wm_socket(wm_name: &str) -> Result<OsString, String> {
    socket_path(
        wm_name,
        std::env::var_os("I3SOCK"),
        std::env::var_os("SWAYSOCK"),
    )
}

/// One connection to the window manager, for any number of requests in
/// turn.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Connects to the socket at `path`. Every later read or write gives up
    /// after `timeout`, so a window manager that accepts the connection but
    /// never answers is an error, not a hang.
    pub fn open(path: &OsStr, timeout: Duration) -> Result<Self, String> {
        let stream = UnixStream::connect(path).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| error.to_string())?;
        Ok(Self { stream })
    }

    /// Sends one request and reads its reply.
    pub fn request(&mut self, command: u32, payload: &str) -> Result<Json, String> {
        self.stream
            .write_all(&encode_request(command, payload)?)
            .map_err(|error| error.to_string())?;
        let payload = read_reply(&mut self.stream)?;
        if payload.is_empty() {
            return Err("No Response".to_owned());
        }
        JsonParser::new(&payload).parse()
    }

    /// Subscribes the connection to `events` (such as `"binding"`); from
    /// then on it carries only events, read with
    /// [`next_event`](Self::next_event), which waits for as long as it takes.
    pub fn subscribe(mut self, events: &[&str]) -> Result<Self, String> {
        let reply = self.request(SUBSCRIBE, &subscribe_payload(events))?;
        if !subscribe_succeeded(&reply) {
            return Err("the subscription was refused".to_owned());
        }
        self.stream
            .set_read_timeout(None)
            .map_err(|error| error.to_string())?;
        Ok(self)
    }

    /// The next event on a subscribed connection: its type and its payload.
    pub fn next_event(&mut self) -> Result<(u32, Json), String> {
        loop {
            let (message_type, payload) = read_message(&mut self.stream)?;
            if message_type & EVENT_FLAG != 0 && !payload.is_empty() {
                return Ok((message_type, JsonParser::new(&payload).parse()?));
            }
        }
    }
}

/// The payload of a subscription: the event names as a JSON array. Event
/// names are plain words, so they need no escaping.
fn subscribe_payload(events: &[&str]) -> String {
    let names: Vec<String> = events.iter().map(|name| format!("\"{name}\"")).collect();
    format!("[{}]", names.join(","))
}

fn subscribe_succeeded(reply: &Json) -> bool {
    matches!(reply, Json::Object(object) if object.get("success") == Some(&Json::Bool(true)))
}

/// One request: the fixed header, with the payload's length in bytes, then
/// the payload as it is.
fn encode_request(command: u32, payload: &str) -> Result<Vec<u8>, String> {
    let length = u32::try_from(payload.len()).map_err(|_| "IPC payload too long".to_owned())?;
    let mut message = Vec::with_capacity(HEADER_SIZE + payload.len());
    message.extend_from_slice(IPC_MAGIC);
    message.extend_from_slice(&length.to_ne_bytes());
    message.extend_from_slice(&command.to_ne_bytes());
    message.extend_from_slice(payload.as_bytes());
    Ok(message)
}

/// The window manager's IPC socket: `SWAYSOCK` for sway, which is how sway
/// was detected, else `I3SOCK`. sway sets `I3SOCK` too, but only for the
/// programs it starts, so a stale `I3SOCK` must not win under sway.
fn socket_path(
    wm_name: &str,
    i3_socket: Option<OsString>,
    sway_socket: Option<OsString>,
) -> Result<OsString, String> {
    let socket = if wm_name == "sway" {
        sway_socket
    } else {
        i3_socket
    };
    socket.ok_or_else(|| format!("No IPC socket for window manager {wm_name}"))
}

/// Reads one reply: the fixed header, then exactly the payload length it
/// announces. Large replies arrive in several reads, and even the header may
/// be split.
fn read_reply(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    read_message(reader).map(|(_, payload)| payload)
}

/// Reads one message, a reply or an event: its type and its payload.
fn read_message(reader: &mut impl Read) -> Result<(u32, Vec<u8>), String> {
    let mut header = [0u8; HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .map_err(|error| format!("reading the reply header failed: {error}"))?;
    if &header[..IPC_MAGIC.len()] != IPC_MAGIC {
        return Err("invalid reply header".to_owned());
    }
    // The window manager writes the length and type in its native byte order.
    let length = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let message_type = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("reading the {length}-byte reply failed: {error}"))?;
    Ok((message_type, payload))
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct VersionReply {
    pub human_readable: String,
    pub loaded_config_file_name: String,
    pub minor: String,
    pub patch: String,
    pub major: String,
}

pub fn get_version(wm_name: &str) -> Result<VersionReply, String> {
    version_from_reply(request(wm_name, GET_VERSION, "")?)
}

fn version_from_reply(reply: Json) -> Result<VersionReply, String> {
    let object = reply.object()?;
    Ok(VersionReply {
        human_readable: member(object, "human_readable")?.string()?.to_owned(),
        loaded_config_file_name: member(object, "loaded_config_file_name")?
            .string()?
            .to_owned(),
        // i3 and sway send the parts as numbers.
        minor: member(object, "minor")?.integer_string()?,
        patch: member(object, "patch")?.integer_string()?,
        major: member(object, "major")?.integer_string()?,
    })
}

/// The configuration the window manager runs with, as it reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum Config {
    /// i3 reports the contents of every file it read, in its reading order,
    /// with its variables and X resources already applied. i3 4.20 and later
    /// also report each file as written, in `written`; it is `None` when the
    /// reply does not carry them.
    I3 {
        files: Vec<String>,
        written: Option<Vec<String>>,
    },
    /// sway reports its main file as written and nothing more: the files it
    /// includes are read from disk. `path` names the main file, when sway
    /// does.
    Sway { main: String, path: Option<PathBuf> },
}

pub fn get_config(wm_name: &str) -> Result<Config, String> {
    let reply = request(wm_name, GET_CONFIG, "")?;
    // Only the version reply names sway's main file, which relative includes
    // in it are read beside.
    let path = (wm_name == "sway")
        .then(|| get_version(wm_name).ok())
        .flatten()
        .map(|version| PathBuf::from(version.loaded_config_file_name))
        .filter(|path| !path.as_os_str().is_empty());
    config_from_reply(reply, wm_name, path)
}

/// The binding mode the window manager is in (`"default"` outside any
/// mode), from its binding-state reply.
pub fn binding_state_from_reply(reply: &Json) -> Result<String, String> {
    Ok(member(reply.object()?, "name")?.string()?.to_owned())
}

/// The command of the binding a binding event reports as run, as the window
/// manager holds it; `None` for an event about anything else.
pub fn command_from_binding_event(event: &Json) -> Result<Option<String>, String> {
    let object = event.object()?;
    if let Some(change) = object.get("change") {
        if change.string()? != "run" {
            return Ok(None);
        }
    }
    let binding = member(object, "binding")?.object()?;
    match binding.get("command") {
        Some(Json::String(command)) => Ok(Some(command.clone())),
        None | Some(Json::Null) => Ok(Some(String::new())),
        Some(_) => Err("expected JSON string for command".to_owned()),
    }
}

pub(crate) fn config_from_reply(
    reply: Json,
    wm_name: &str,
    path: Option<PathBuf>,
) -> Result<Config, String> {
    let object = reply.object()?;
    if wm_name == "sway" {
        let main = member(object, "config")?.string()?.to_owned();
        Ok(Config::Sway { main, path })
    } else {
        // Each file stays apart, so one without a final newline does not run
        // into the next.
        let mut files = Vec::new();
        let mut written = Some(Vec::new());
        for included in member(object, "included_configs")?.array()? {
            let included = included.object()?;
            let contents = member(included, "variable_replaced_contents")?;
            files.push(contents.string()?.to_owned());
            written = written.and_then(|mut written: Vec<String>| {
                let raw = included.get("raw_contents")?.string().ok()?;
                written.push(raw.to_owned());
                Some(written)
            });
        }
        Ok(Config::I3 { files, written })
    }
}

/// One word of an expanded `include` argument.
#[derive(Debug, PartialEq)]
struct ExpandedWord {
    text: String,
    /// Whether sway matches the word against file names. A word written in
    /// the text turns into a pattern at its first `*`, `?` or `[` written
    /// there unquoted and without a backslash. From then on, until the word
    /// written in the text ends, everything is pattern syntax as it stands,
    /// quoted text and expanded values included, and so is every field an
    /// expanded value splits off. Fields split off before that character,
    /// and words that never meet one, stand for themselves, wildcard
    /// characters and all.
    pattern: bool,
}

/// The words of `text` expanded the way sway expands an include, with
/// glibc's word expansion, short of running anything. A `~` that starts a
/// word (or follows a `=` in the first one) stands for `$HOME`. `$NAME` and
/// `${NAME}` stand for the environment's value, nothing when it is not set;
/// `${NAME-word}`, `${NAME=word}`, `${NAME+word}`, `${NAME?word}` (with or
/// without a colon after the name), `${#NAME}` and the `#`, `##`, `%` and
/// `%%` trims work as in the shell, except that `?` stands for nothing
/// instead of failing and that the value a word gives is never a pattern.
/// Quotes hold text together and go, a backslash keeps the character after
/// it as it is, and what an unquoted `$` brings in is split again at blanks.
///
/// `environment` holds what `${NAME=word}` and `${NAME:=word}` assigned. sway
/// sets those in its own environment, so they hold for the rest of its
/// reading, and they take precedence over the session's environment here.
///
/// `None` when glibc refuses the text, or when expanding it would mean
/// running something, which sway does and this never does: it holds,
/// unquoted and before the word has become a pattern, a character that
/// means something to the shell (`|`, `&`, `;`, `<`, `>`, `(`, `)`, `{` or
/// `}`, the braces of a `${...}` aside), a backquote or `$(` (command
/// substitution and arithmetic), a quote or a `${` that is not closed, a
/// backslash at its end, or a `${...}` that is not one of the forms above
/// (`${1}` included). A `$` that no name follows stands for itself, so `$1`
/// stays as written. `~user` is not looked up and stays as written, and so
/// does `~` when `HOME` is not set.
fn expand_shell_words(
    text: &str,
    environment: &mut HashMap<String, String>,
) -> Option<Vec<ExpandedWord>> {
    let mut words = WordExpansion::default();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => words.blank(),
            '\\' => words.push(chars.next()?),
            '\'' => {
                words.started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        // glibc ends a pattern at a blank even inside quotes;
                        // the closing quote then opens a new quotation.
                        ' ' | '\t' | '\n' if words.pattern => {
                            words.blank();
                            break;
                        }
                        c => words.word.push(c),
                    }
                }
            }
            '"' => {
                words.started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        ' ' | '\t' | '\n' if words.pattern => {
                            words.blank();
                            break;
                        }
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            words.word.extend(chars.next());
                        }
                        '$' => match parameter(&mut chars, environment)? {
                            Some(value) => words.word.push_str(&value),
                            None => words.word.push('$'),
                        },
                        '`' if !words.pattern => return None,
                        c => words.word.push(c),
                    }
                }
            }
            '$' => match parameter(&mut chars, environment)? {
                Some(value) => words.push_value(&value),
                None => words.push('$'),
            },
            '~' if words.tilde_expands() && ends_home(chars.peek()) => {
                words.word.push_str(&home_directory());
                words.started = true;
            }
            '*' | '?' | '[' => {
                words.pattern = true;
                words.push(c);
            }
            // Once the word is a pattern, glibc takes these as they are.
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '`' if words.pattern => {
                words.push(c)
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '`' => return None,
            c => words.push(c),
        }
    }
    words.end_field();
    Some(words.words)
}

/// The state of `expand_shell_words`.
#[derive(Default)]
struct WordExpansion {
    words: Vec<ExpandedWord>,
    word: String,
    /// Whether `word` is a word even while empty: quotes make one.
    started: bool,
    /// Whether the word written in the text has turned into a pattern.
    pattern: bool,
}

impl WordExpansion {
    fn push(&mut self, c: char) {
        self.word.push(c);
        self.started = true;
    }

    /// A blank in the text: it ends the word written there.
    fn blank(&mut self) {
        self.end_field();
        self.pattern = false;
    }

    /// Whether a `~` here may stand for the home directory, as glibc decides
    /// it: when the word has no text yet, or in the first word right after a
    /// `=`, or after a `:` once that word holds a `=`.
    fn tilde_expands(&self) -> bool {
        self.word.is_empty()
            || (self.words.is_empty()
                && (self.word.ends_with('=')
                    || (self.word.ends_with(':') && self.word.contains('='))))
    }

    fn end_field(&mut self) {
        if std::mem::take(&mut self.started) {
            self.words.push(ExpandedWord {
                text: std::mem::take(&mut self.word),
                pattern: self.pattern,
            });
        }
    }

    /// Adds an unquoted value, split at blanks; an empty one makes no word.
    fn push_value(&mut self, value: &str) {
        for c in value.chars() {
            match c {
                ' ' | '\t' | '\n' => self.end_field(),
                c => self.push(c),
            }
        }
    }
}

/// Whether a `~` followed by `next` stands for the home directory rather
/// than for the start of `~user`, which is not looked up.
fn ends_home(next: Option<&char>) -> bool {
    matches!(next, None | Some('/' | ':' | ' ' | '\t'))
}

fn home_directory() -> String {
    std::env::var_os("HOME").map_or_else(
        || "~".to_owned(),
        |home| home.to_string_lossy().into_owned(),
    )
}

/// What a `${NAME<operator>word}` does with its word.
#[derive(Clone, Copy)]
enum ParameterOperator {
    /// `-`: the word when the variable is unset.
    Default,
    /// `=`: the word when the variable is unset, which it is set to as well.
    Assign,
    /// `?`: nothing when the variable is unset.
    Error,
    /// `+`: the word when the variable is set, nothing otherwise.
    Alternative,
    /// `#` and `##`: the value without the shortest or longest start the word
    /// matches.
    TrimStart { longest: bool },
    /// `%` and `%%`: the value without the shortest or longest end the word
    /// matches.
    TrimEnd { longest: bool },
}

/// The value of the parameter after a `$`, taken from `chars`:
/// `Some(Some(value))`, `Some(None)` when no name follows (the `$` stands
/// for itself) and `None` for a `${...}` that is not closed or not a form
/// `expand_shell_words` takes.
fn parameter(
    chars: &mut Peekable<Chars<'_>>,
    environment: &mut HashMap<String, String>,
) -> Option<Option<String>> {
    let braced = chars.next_if_eq(&'{').is_some();
    // Command substitution and arithmetic would run something.
    if !braced && chars.peek() == Some(&'(') {
        return None;
    }
    // A `$` that no name follows stands for itself, and what follows it is
    // read on its own.
    if !braced
        && !chars
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
    {
        return Some(None);
    }
    let length = braced && chars.next_if_eq(&'#').is_some();
    let mut name = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
        name.push(c);
    }
    if !is_environment_name(&name) {
        return None;
    }
    let value = environment
        .get(&name)
        .cloned()
        .or_else(|| std::env::var_os(&name).map(|value| value.to_string_lossy().into_owned()));
    if !braced {
        return Some(Some(value.unwrap_or_default()));
    }
    use ParameterOperator::*;
    let (operator, colon) = match chars.next()? {
        '}' if length => return Some(Some(value.map_or(0, |value| value.len()).to_string())),
        '}' => return Some(Some(value.unwrap_or_default())),
        _ if length => return None,
        ':' => (simple_operator(chars.next()?)?, true),
        '#' => (
            TrimStart {
                longest: chars.next_if_eq(&'#').is_some(),
            },
            false,
        ),
        '%' => (
            TrimEnd {
                longest: chars.next_if_eq(&'%').is_some(),
            },
            false,
        ),
        c => (simple_operator(c)?, false),
    };
    let word = parameter_word(chars)?;
    // With a colon, an empty value counts as unset.
    let set = value
        .as_deref()
        .is_some_and(|value| !(colon && value.is_empty()));
    let result = match operator {
        Default | Assign | Error if set => value.unwrap_or_default(),
        Default => expand_parameter_word(&word, environment)?,
        Assign => {
            let word = expand_parameter_word(&word, environment)?;
            environment.insert(name, word.clone());
            word
        }
        Error => {
            expand_parameter_word(&word, environment)?;
            String::new()
        }
        Alternative if set => expand_parameter_word(&word, environment)?,
        Alternative => String::new(),
        TrimStart { longest } | TrimEnd { longest } => {
            // The word is expanded even when there is nothing to trim, so
            // the assignments in it happen.
            let pattern = expand_parameter_word(&word, environment)?;
            match value {
                None => String::new(),
                Some(value) if pattern.is_empty() => value,
                Some(value) => trim_matching(
                    &value,
                    pattern.as_bytes(),
                    matches!(operator, TrimStart { .. }),
                    longest,
                ),
            }
        }
    };
    Some(Some(result))
}

fn simple_operator(c: char) -> Option<ParameterOperator> {
    match c {
        '-' => Some(ParameterOperator::Default),
        '=' => Some(ParameterOperator::Assign),
        '?' => Some(ParameterOperator::Error),
        '+' => Some(ParameterOperator::Alternative),
        _ => None,
    }
}

/// The word of a `${NAME<operator>word}` up to its closing brace, as written:
/// braces pair up, and a quoted or backslashed brace closes nothing.
fn parameter_word(chars: &mut Peekable<Chars<'_>>) -> Option<String> {
    let mut word = String::new();
    let mut depth = 0;
    let mut quote = None;
    loop {
        let c = chars.next()?;
        match c {
            '{' if quote.is_none() => depth += 1,
            '}' if quote.is_none() => {
                if depth == 0 {
                    return Some(word);
                }
                depth -= 1;
            }
            '\\' if quote.is_none() => {
                word.push(c);
                word.push(chars.next()?);
                continue;
            }
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '\'' | '"' if quote == Some(c) => quote = None,
            _ => {}
        }
        word.push(c);
    }
}

/// The word of a `${NAME<operator>word}` expanded the way glibc expands it:
/// quotes go but a quoted `*` or `?` keeps a backslash in front of it, every
/// `$` expands without splitting, a leading `~` is the home directory, and a
/// backslash stays, together with the character after it.
fn expand_parameter_word(word: &str, environment: &mut HashMap<String, String>) -> Option<String> {
    let mut expanded = String::new();
    let mut quote = None;
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '\'' | '"' if quote == Some(c) => quote = None,
            '*' | '?' if quote.is_some() => {
                expanded.push('\\');
                expanded.push(c);
            }
            '$' => match parameter(&mut chars, environment)? {
                Some(value) => expanded.push_str(&value),
                None => expanded.push('$'),
            },
            '~' if quote.is_none() && expanded.is_empty() && ends_home(chars.peek()) => {
                expanded.push_str(&home_directory());
            }
            '\\' => {
                expanded.push(c);
                expanded.extend(chars.next());
            }
            c => expanded.push(c),
        }
    }
    Some(expanded)
}

/// `value` without the shortest or `longest` start (`start`) or end that the
/// pattern matches, or all of it when the pattern matches none. The pattern
/// matches bytes, as in sway's C locale, but the value is only cut between
/// characters, so that it stays valid UTF-8.
fn trim_matching(value: &str, pattern: &[u8], start: bool, longest: bool) -> String {
    let mut cuts: Vec<usize> = (0..=value.len())
        .filter(|cut| value.is_char_boundary(*cut))
        .collect();
    // The shortest start ends at the first cut, the shortest end begins at
    // the last.
    if start == longest {
        cuts.reverse();
    }
    for cut in cuts {
        let (head, tail) = value.split_at(cut);
        let part = if start { head } else { tail };
        if wildcard_matches(pattern, part.as_bytes()) {
            return if start { tail } else { head }.to_owned();
        }
    }
    value.to_owned()
}

fn is_environment_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The words of an `include` line's argument as sway hands them on: split at
/// spaces and tabs that are neither quoted nor escaped by a backslash, a word
/// that starts with a quote without its quotes, joined by single spaces.
fn include_argument(text: &str) -> String {
    let mut words = Vec::new();
    let mut start = None;
    let (mut single, mut double, mut escaped) = (false, false, false);
    for (index, c) in text.char_indices() {
        if !(single || double || escaped) && matches!(c, ' ' | '\t') {
            if let Some(start) = start.take() {
                words.push(&text[start..index]);
            }
            continue;
        }
        start.get_or_insert(index);
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' if !single => double = !double,
            '\'' if !double => single = !single,
            _ => {}
        }
    }
    if let Some(start) = start {
        words.push(&text[start..]);
    }
    words
        .into_iter()
        .map(|word| match word.starts_with(['"', '\'']) {
            true => unescape(&strip_quotes(word)),
            false => unescape(word),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` with the escapes sway's `unescape_string` replaces in every
/// argument of a configuration command: `\\`, `\'`, `\"`, `\?`, the C
/// control escapes `\a`, `\b`, `\f`, `\n`, `\r`, `\t` and `\v`, and `\x`
/// with up to two decimal digits read in base 16 (sway's own reading). `\0`
/// ends the word. A backslash before anything else stays, with the character
/// after it, for the word expansion to read.
fn unescape(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        let Some(escaped) = chars.next() else {
            result.push(c);
            break;
        };
        match escaped {
            '0' => break,
            'a' => result.push('\x07'),
            'b' => result.push('\x08'),
            'f' => result.push('\x0c'),
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'v' => result.push('\x0b'),
            '\\' | '\'' | '"' | '?' => result.push(escaped),
            'x' => {
                let mut value = 0u32;
                for _ in 0..2 {
                    match chars.next_if(char::is_ascii_digit) {
                        Some(digit) => value = value * 16 + digit.to_digit(10).unwrap_or(0),
                        None => break,
                    }
                }
                // sway drops the NUL a `\x` without digits gives; a byte
                // above ASCII would not be UTF-8.
                match value {
                    0 => {}
                    1..=0x7f => result.extend(char::from_u32(value)),
                    _ => result.push(char::REPLACEMENT_CHARACTER),
                }
            }
            other => {
                result.push(c);
                result.push(other);
            }
        }
    }
    result
}

/// `word` without the quotes that are not escaped by a backslash or inside
/// quotes of the other kind; backslashes stay.
fn strip_quotes(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let (mut single, mut double, mut escaped) = (false, false, false);
    for c in word.chars() {
        match c {
            '\'' if !(double || escaped) => single = !single,
            '"' if !(single || escaped) => double = !double,
            _ => {
                escaped = c == '\\' && !escaped;
                result.push(c);
            }
        }
    }
    result
}

/// The files an `include` reads, in the order sway reads them, by their
/// resolved real paths: the words of `argument` (its variables already
/// replaced) expanded with the session environment and `environment` over
/// it, each taken from `directory` (the one holding the file that names it)
/// when relative. A word that is a pattern stands for its matches, sorted;
/// any other word for the file it names. Names that are not files, or are not
/// there, read nothing; an argument with shell syntax reads nothing at all.
fn include_paths(
    argument: &str,
    directory: Option<&Path>,
    environment: &mut HashMap<String, String>,
) -> Vec<PathBuf> {
    let Some(words) = expand_shell_words(&include_argument(argument), environment) else {
        return Vec::new();
    };
    words
        .into_iter()
        .flat_map(|word| match word.pattern {
            true => expand_glob(&word.text, directory),
            false => vec![match directory {
                Some(directory) => directory.join(&word.text),
                None => PathBuf::from(&word.text),
            }],
        })
        .filter_map(|path| fs::canonicalize(path).ok())
        .filter(|path| path.is_file())
        .collect()
}

/// The lines of `text` as sway reads them: a line that ends in a backslash
/// goes on with the next line, the backslash and the line break dropped,
/// unless it starts with `#`.
fn continued_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let mut joined = pending.take().unwrap_or_default();
        joined.push_str(line);
        match joined.strip_suffix("\\\n") {
            Some(start) if !joined.starts_with('#') => pending = Some(start.to_owned()),
            _ => lines.push(joined.strip_suffix('\n').unwrap_or(&joined).to_owned()),
        }
    }
    // A backslash on the last line has nothing to go on with.
    lines.extend(pending.map(|start| start + "\\"));
    lines
}

/// sway's configuration as sway itself reads it, since sway reports only its
/// main file, as written: the lines in sway's reading order, lines continued
/// with a trailing backslash joined, each `include` read in place, the files
/// a pattern matches in alphabetical order and each file once, by its real
/// path. Comments stay as written; every other line has its variables
/// (`set`, and `set_from_resource` with `resources`) replaced by the value
/// they have where they are used, and its words joined by single spaces with
/// quoted words kept whole. The lines that define variables or include files
/// stay in place as written, after they have taken effect.
///
/// The lines come in runs that each hold the lines of one file: a run ends
/// where an included file starts or ends.
///
/// `main` is the text of the main file and `main_path` the file, when known;
/// relative includes in `main` are read beside `main_path` as it is given,
/// those in an included file beside the file's real path.
pub fn read_config_files(
    main: &str,
    main_path: Option<&Path>,
    resources: &HashMap<String, String>,
) -> Vec<Vec<String>> {
    let walk = ConfigWalk::new(resources).walk(main, main_path);
    let mut lines = walk.lines.into_iter();
    let mut files = Vec::new();
    let mut start = 0;
    for end in walk.breaks.into_iter().chain([usize::MAX]) {
        let run: Vec<String> = lines.by_ref().take(end.saturating_sub(start)).collect();
        if !run.is_empty() {
            files.push(run);
        }
        start = end;
    }
    files
}

struct ConfigWalk<'a> {
    resources: &'a HashMap<String, String>,
    /// The variables defined so far, by name without the `$`.
    variables: HashMap<String, String>,
    /// The files read so far, by canonical path.
    visited: HashSet<PathBuf>,
    lines: Vec<String>,
    /// Where in `lines` the text of one file starts or ends.
    breaks: Vec<usize>,
    /// The environment variables that includes assigned with
    /// `${NAME:=word}`, as sway keeps them in its own environment.
    environment: HashMap<String, String>,
}

impl<'a> ConfigWalk<'a> {
    fn new(resources: &'a HashMap<String, String>) -> Self {
        Self {
            resources,
            variables: HashMap::new(),
            visited: HashSet::new(),
            lines: Vec::new(),
            breaks: Vec::new(),
            environment: HashMap::new(),
        }
    }

    fn walk(mut self, main: &str, main_path: Option<&Path>) -> Self {
        // An include that names the main file does not read it again.
        if let Some(path) = main_path {
            self.visited
                .insert(fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        }
        // sway reads the main file's relative includes beside the path it was
        // given, not beside the file a link there points to.
        self.read_text(main, main_path.and_then(Path::parent));
        self
    }

    /// Reads the text of one file; `directory` holds the file.
    fn read_text(&mut self, text: &str, directory: Option<&Path>) {
        self.file_break();
        for line in continued_lines(text) {
            self.read_line(line.trim(), directory);
        }
        self.file_break();
    }

    fn file_break(&mut self) {
        if self.breaks.last() != Some(&self.lines.len()) {
            self.breaks.push(self.lines.len());
        }
    }

    /// Keeps a line that defines a variable or includes files as written.
    fn keep(&mut self, line: &str) {
        self.lines.push(line.to_owned());
    }

    fn read_line(&mut self, line: &str, directory: Option<&Path>) {
        if line.is_empty() || line.starts_with('#') {
            self.lines.push(line.to_owned());
            return;
        }
        // sway reads its keywords without regard to case.
        match split_words(line).as_slice() {
            [keyword, ..] if keyword.eq_ignore_ascii_case("include") => {
                self.keep(line);
                // The line is trimmed, so the keyword starts it.
                let argument = self.replace(&line[keyword.len()..]);
                for path in include_paths(&argument, directory, &mut self.environment) {
                    if !self.visited.insert(path.clone()) {
                        continue;
                    }
                    // sway reads the bytes, whatever their encoding.
                    match fs::read(&path) {
                        Ok(text) => self.read_text(&String::from_utf8_lossy(&text), path.parent()),
                        Err(error) => {
                            // sway forgets a file it could not read.
                            self.visited.remove(&path);
                            eprintln!("Cannot read {}: {error}", path.display());
                        }
                    }
                }
            }
            [keyword, name, value @ ..]
                if keyword.eq_ignore_ascii_case("set")
                    && name.starts_with('$')
                    && !value.is_empty() =>
            {
                self.keep(line);
                let value = join_words(&self.replace(&value.join(" ")));
                self.variables.insert(name[1..].to_owned(), value);
            }
            [keyword, name, key, default @ ..]
                if keyword.eq_ignore_ascii_case("set_from_resource") && name.starts_with('$') =>
            {
                self.keep(line);
                let resource = self
                    .resources
                    .get(&self.replace(key))
                    .filter(|value| !value.is_empty())
                    .cloned();
                // With neither a value nor a default the variable is left as
                // it was, defined or not.
                let value = resource.or_else(|| {
                    (!default.is_empty()).then(|| join_words(&self.replace(&default.join(" "))))
                });
                if let Some(value) = value {
                    self.variables.insert(name[1..].to_owned(), value);
                }
            }
            words => {
                let line = join_words(&self.replace(&words.join(" ")));
                self.lines.push(line);
            }
        }
    }

    /// `text` with each `$name` replaced by the value the variable has now.
    /// Of the defined names that start at one `$`, the longest wins; a name
    /// that is not defined stays as written. As in sway, `$$` becomes `$`,
    /// which is not replaced then, and a `$` behind a single backslash is
    /// left alone.
    fn replace(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(dollar) = rest.find('$') {
            result.push_str(&rest[..dollar]);
            let after = &rest[dollar + 1..];
            if result.ends_with('\\') && !result.ends_with("\\\\") {
                result.push('$');
                rest = after;
                continue;
            }
            if let Some(after) = after.strip_prefix('$') {
                result.push('$');
                rest = after;
                continue;
            }
            let run = after
                .find(|c: char| !is_name_char(c))
                .unwrap_or(after.len());
            // The run is ASCII, so every prefix ends on a character boundary.
            let defined = (1..=run)
                .rev()
                .map(|end| &after[..end])
                .find_map(|name| self.variables.get_key_value(name));
            match defined {
                Some((name, value)) => {
                    result.push_str(value);
                    rest = &after[name.len()..];
                }
                None => {
                    result.push('$');
                    result.push_str(&after[..run]);
                    rest = &after[run..];
                }
            }
        }
        result.push_str(rest);
        result
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// The words of a line as sway splits it: at spaces and tabs, but not inside
/// quotes or inside a criteria block (`[...]` at the start of a word). A
/// backslash escapes the character after it. Words keep their quotes and
/// backslashes.
pub(crate) fn split_words(line: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let (mut single, mut double, mut criteria, mut escaped) = (false, false, false, false);
    for (index, c) in line.char_indices() {
        if !(single || double || criteria || escaped) && matches!(c, ' ' | '\t') {
            if let Some(start) = start.take() {
                words.push(&line[start..index]);
            }
            continue;
        }
        let word_start = start.is_none();
        start.get_or_insert(index);
        if escaped {
            escaped = false;
        } else if single {
            single = c != '\'';
        } else if double {
            match c {
                '\\' => escaped = true,
                '"' => double = false,
                _ => {}
            }
        } else if c == '\\' {
            escaped = true;
        } else if criteria {
            match c {
                '"' => double = true,
                '\'' => single = true,
                ']' => criteria = false,
                _ => {}
            }
        } else {
            match c {
                '\'' => single = true,
                '"' => double = true,
                '[' if word_start => criteria = true,
                _ => {}
            }
        }
    }
    if let Some(start) = start {
        words.push(&line[start..]);
    }
    words
}

/// The words of `text` joined by single spaces.
fn join_words(text: &str) -> String {
    split_words(text).join(" ")
}

/// The files of an i3 configuration, given as written and in i3's reading
/// order, with i3's variables applied the way i3 4.20 and later apply them
/// when no X resource is set: every `set_from_resource` takes its default.
///
/// Within each file, lines continued with a trailing backslash are joined
/// (not in comments), all of the file's `set` and `set_from_resource` lines
/// are taken first and then the file's whole text is substituted once. Names
/// are matched without regard to ASCII case wherever they occur, the longest
/// name wins where several start at one `$`, a name set again takes its last
/// value, and values are inserted as written, not substituted again. The
/// variables a file sets stay set for the files read after it.
pub fn replace_i3_variables(files: &[String]) -> Vec<String> {
    let mut variables: Vec<(String, String)> = Vec::new();
    files
        .iter()
        .map(|text| {
            let text = join_i3_continuations(text);
            for line in text.lines() {
                if let Some((name, value)) = i3_variable(line) {
                    match variables
                        .iter_mut()
                        .find(|(known, _)| known.eq_ignore_ascii_case(&name))
                    {
                        Some(variable) => *variable = (name, value),
                        None => variables.push((name, value)),
                    }
                }
            }
            replace_i3_names(&text, &variables)
        })
        .collect()
}

/// `text` with each line that ends in a backslash joined to the next, the
/// backslash and line break dropped, unless the line is a comment.
fn join_i3_continuations(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        joined.push_str(line);
        let comment = joined[line_start..].trim_start().starts_with('#');
        if !comment && joined.ends_with("\\\n") {
            joined.truncate(joined.len() - 2);
        } else {
            line_start = joined.len();
        }
    }
    joined
}

/// The variable a line of an i3 file defines with no X resources set, as
/// `($name, value)`: `set $name value`, or `set_from_resource $name resource
/// default` with its default, empty when there is none.
fn i3_variable(line: &str) -> Option<(String, String)> {
    let (keyword, rest) = split_i3_word(line)?;
    let (name, value) = if keyword.eq_ignore_ascii_case("set") {
        split_i3_word(rest)?
    } else if keyword.eq_ignore_ascii_case("set_from_resource") {
        let (name, rest) = split_i3_word(rest)?;
        let default = split_i3_word(rest).map_or("", |(_resource, default)| default);
        (name, default)
    } else {
        return None;
    };
    name.starts_with('$')
        .then(|| (name.to_owned(), value.to_owned()))
}

/// The first blank-separated word of `text` and the text after the blanks
/// that follow it.
fn split_i3_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((&text[..end], text[end..].trim_start()))
}

/// `text` with every occurrence of a variable name replaced by its value, in
/// one pass from the start: the longest of the names that start at a `$`
/// wins, and inserted values are not searched again.
fn replace_i3_names(text: &str, variables: &[(String, String)]) -> String {
    let mut by_length: Vec<&(String, String)> = variables.iter().collect();
    by_length.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        result.push_str(&rest[..dollar]);
        rest = &rest[dollar..];
        let found = by_length.iter().find(|(name, _)| {
            rest.get(..name.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(name))
        });
        match found {
            Some((name, value)) => {
                result.push_str(value);
                rest = &rest[name.len()..];
            }
            None => {
                result.push('$');
                rest = &rest[1..];
            }
        }
    }
    result.push_str(rest);
    result
}

/// The paths that the pattern `pattern` matches, sorted by their bytes, the
/// way glibc's `glob` finds them for sway (which runs in the C locale); a
/// relative pattern starts at `directory`. A part of the pattern with
/// wildcards (a `*`, `?` or `[` without a backslash in front) matches the
/// names `wildcard_matches` accepts, except that a name that starts with a
/// dot is only matched by a part that starts with one too. Any other part
/// stands for itself, without its backslashes. A pattern that ends in `/`
/// matches directories only. A pattern with wildcards that matches no
/// existing path stands for itself, as written.
fn expand_glob(pattern: &str, directory: Option<&Path>) -> Vec<PathBuf> {
    let written = match directory {
        Some(directory) => directory.join(pattern),
        None => PathBuf::from(pattern),
    };
    let mut candidates = vec![match (pattern.starts_with('/'), directory) {
        (true, _) => PathBuf::from("/"),
        (false, Some(directory)) => directory.to_owned(),
        (false, None) => PathBuf::new(),
    }];
    let mut wildcards = false;
    for part in pattern.split('/').filter(|part| !part.is_empty()) {
        let part = part.as_bytes();
        if !has_wildcards(part) {
            let name = OsString::from_vec(without_backslashes(part));
            candidates
                .iter_mut()
                .for_each(|candidate| candidate.push(&name));
            continue;
        }
        wildcards = true;
        let leading_dot = part.starts_with(b".") || part.starts_with(b"\\.");
        let mut next = Vec::new();
        for candidate in candidates {
            let directory = if candidate.as_os_str().is_empty() {
                Path::new(".")
            } else {
                candidate.as_path()
            };
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            // `glob` sees `.` and `..` as well, which `read_dir` leaves out.
            let names = [OsString::from("."), OsString::from("..")]
                .into_iter()
                .chain(entries.flatten().map(|entry| entry.file_name()));
            for name in names {
                let text = name.as_bytes();
                let hidden = text.starts_with(b".") && !leading_dot;
                if !hidden && wildcard_matches(part, text) {
                    next.push(candidate.join(name));
                }
            }
        }
        candidates = next;
    }
    if wildcards {
        if pattern.ends_with('/') {
            candidates.retain(|path| path.is_dir());
        } else {
            candidates.retain(|path| fs::symlink_metadata(path).is_ok());
        }
        if candidates.is_empty() {
            return vec![written];
        }
    }
    candidates.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    candidates
}

/// Whether a part of a pattern has a wildcard without a backslash in front.
fn has_wildcards(part: &[u8]) -> bool {
    let mut bytes = part.iter();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                bytes.next();
            }
            b'*' | b'?' | b'[' => return true,
            _ => {}
        }
    }
    false
}

/// `part` with each backslash dropped and the byte after it kept.
fn without_backslashes(part: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(part.len());
    let mut bytes = part.iter();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => name.push(*bytes.next().unwrap_or(byte)),
            byte => name.push(*byte),
        }
    }
    name
}

/// Whether `text` fits the glob `pattern`, byte by byte as `fnmatch` matches
/// in the C locale sway runs in: `*` stands for any run of bytes, `?` for
/// one byte and `[...]` for one byte of a set, in which `!` or `^` first
/// negates the set, a `]` first is a member and `a-z` is a range. A
/// backslash makes the byte after it stand for itself, inside a set too. An
/// unclosed `[` stands for itself. So `caf?` does not match `café`, whose
/// `é` is two bytes.
fn wildcard_matches(pattern: &[u8], text: &[u8]) -> bool {
    let tokens = glob_tokens(pattern);
    // Every token but `*` stands for exactly one byte, so on a mismatch only
    // the last `*` seen has to take one byte more: the match takes time
    // proportional to the lengths' product, however many `*` there are.
    let (mut token, mut position) = (0, 0);
    // The last `*` seen and the text position its run ends at so far.
    let mut star: Option<(usize, usize)> = None;
    while position < text.len() {
        match tokens.get(token) {
            Some(GlobToken::Star) => {
                star = Some((token, position));
                token += 1;
            }
            Some(one) if one.matches(text[position]) => {
                token += 1;
                position += 1;
            }
            _ => match star {
                Some((star_token, end)) => {
                    star = Some((star_token, end + 1));
                    token = star_token + 1;
                    position = end + 1;
                }
                None => return false,
            },
        }
    }
    tokens[token..]
        .iter()
        .all(|token| matches!(token, GlobToken::Star))
}

/// One element of a glob pattern.
#[derive(Debug, PartialEq)]
enum GlobToken {
    /// `*`: any run of bytes.
    Star,
    /// `?`: any one byte.
    Any,
    /// A byte that stands for itself.
    Byte(u8),
    /// `[...]`: one byte of the ranges (single bytes are ranges of one), or
    /// of none of them when `negated`.
    Set {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

impl GlobToken {
    /// Whether the token, other than `*`, stands for `byte`.
    fn matches(&self, byte: u8) -> bool {
        match self {
            GlobToken::Star | GlobToken::Any => true,
            GlobToken::Byte(expected) => *expected == byte,
            GlobToken::Set { negated, ranges } => {
                ranges
                    .iter()
                    .any(|(low, high)| (*low..=*high).contains(&byte))
                    != *negated
            }
        }
    }
}

/// The tokens of a glob pattern, as `wildcard_matches` reads it.
fn glob_tokens(pattern: &[u8]) -> Vec<GlobToken> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < pattern.len() {
        let (token, next) = match pattern[index] {
            b'*' => (GlobToken::Star, index + 1),
            b'?' => (GlobToken::Any, index + 1),
            b'[' => glob_set(pattern, index).unwrap_or((GlobToken::Byte(b'['), index + 1)),
            b'\\' if index + 1 < pattern.len() => (GlobToken::Byte(pattern[index + 1]), index + 2),
            byte => (GlobToken::Byte(byte), index + 1),
        };
        tokens.push(token);
        index = next;
    }
    tokens
}

/// The set that starts with the `[` at `open`, and the index after its `]`;
/// `None` when no `]` closes it.
fn glob_set(pattern: &[u8], open: usize) -> Option<(GlobToken, usize)> {
    let negated = matches!(pattern.get(open + 1), Some(b'!' | b'^'));
    let start = open + 1 + usize::from(negated);
    // A `]` right at the start of the set is a member, not its end.
    let mut index = start + usize::from(pattern.get(start) == Some(&b']'));
    let end = loop {
        match pattern.get(index)? {
            b'\\' => index += 2,
            b']' => break index,
            _ => index += 1,
        }
    };
    let class = &pattern[start..end];
    // One member of the set at `index`: its byte and where the next member
    // starts.
    let member = |index: usize| match class[index] {
        b'\\' if index + 1 < class.len() => (class[index + 1], index + 2),
        byte => (byte, index + 1),
    };
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < class.len() {
        let (low, next) = member(index);
        if next + 1 < class.len() && class[next] == b'-' {
            let (high, after) = member(next + 1);
            ranges.push((low, high));
            index = after;
        } else {
            ranges.push((low, low));
            index = next;
        }
    }
    Some((GlobToken::Set { negated, ranges }, end + 1))
}

/// Runs `command` in the window manager, in a message of its own: the text
/// reaches it unchanged, with no shell or command line in between. A command
/// the window manager reports as failed is an error carrying its reason.
pub fn run_command(wm_name: &str, command: &str) -> Result<(), String> {
    command_outcome(&request(wm_name, RUN_COMMAND, command)?)
}

/// The names of the workspaces the window manager knows about, on every
/// output, empty ones included.
pub fn get_workspace_names(wm_name: &str) -> Result<Vec<String>, String> {
    workspace_names_from_reply(&request(wm_name, GET_WORKSPACES, "")?)
}

fn workspace_names_from_reply(reply: &Json) -> Result<Vec<String>, String> {
    reply
        .array()?
        .iter()
        .map(|workspace| Ok(member(workspace.object()?, "name")?.string()?.to_owned()))
        .collect()
}

/// The outcome of a run request. The reply holds one result per command
/// run, and a failed one says why.
fn command_outcome(reply: &Json) -> Result<(), String> {
    for result in reply.array()? {
        let result = result.object()?;
        if !member(result, "success")?.bool()? {
            return Err(match result.get("error") {
                Some(Json::String(error)) => error.clone(),
                _ => "unknown error".to_owned(),
            });
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowProperties {
    pub class: Option<String>,
    pub instance: Option<String>,
    pub machine: Option<String>,
    pub title: Option<String>,
    pub window_role: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeReply {
    pub id: String,
    pub node_type: String,
    pub urgent: bool,
    pub name: String,
    pub window_type: Option<String>,
    pub output: Option<String>,
    pub app_id: Option<String>,
    pub layout: Option<String>,
    pub window_properties: Option<WindowProperties>,
    pub nodes: Vec<TreeReply>,
    pub floating_nodes: Vec<TreeReply>,
}

impl TreeReply {
    fn from_json(value: &Json) -> Result<Self, String> {
        let object = value.object()?;
        let optional_string = |name: &str| -> Result<Option<String>, String> {
            match object.get(name) {
                None | Some(Json::Null) => Ok(None),
                Some(value) => Ok(Some(value.string()?.to_owned())),
            }
        };
        let children = |name: &str| -> Result<Vec<Self>, String> {
            match object.get(name) {
                None | Some(Json::Null) => Ok(Vec::new()),
                Some(value) => value.array()?.iter().map(Self::from_json).collect(),
            }
        };
        let window_properties = match object.get("window_properties") {
            None | Some(Json::Null) => None,
            Some(value) => {
                let properties = value.object()?;
                let property = |name: &str| -> Result<Option<String>, String> {
                    match properties.get(name) {
                        None | Some(Json::Null) => Ok(None),
                        Some(value) => Ok(Some(value.string()?.to_owned())),
                    }
                };
                Some(WindowProperties {
                    class: property("class")?,
                    instance: property("instance")?,
                    machine: property("machine")?,
                    title: property("title")?,
                    window_role: property("window_role")?,
                })
            }
        };
        Ok(Self {
            id: member(object, "id")?.integer_string()?,
            node_type: member(object, "type")?.string()?.to_owned(),
            urgent: member(object, "urgent")?.bool()?,
            name: match object.get("name") {
                Some(Json::String(value)) => value.clone(),
                None | Some(Json::Null) => String::new(),
                Some(_) => return Err("expected JSON string for name".to_owned()),
            },
            window_type: optional_string("window_type")?,
            output: optional_string("output")?,
            app_id: optional_string("app_id")?,
            layout: optional_string("layout")?,
            window_properties,
            nodes: children("nodes")?,
            floating_nodes: children("floating_nodes")?,
        })
    }
}

pub fn get_tree(wm_name: &str) -> Result<TreeReply, String> {
    TreeReply::from_json(&request(wm_name, GET_TREE, "")?)
}

#[cfg(test)]
mod tests {
    use super::{
        command_outcome, config_from_reply, continued_lines, encode_request, expand_glob,
        expand_shell_words, include_argument, include_paths, read_config_files, read_reply,
        replace_i3_variables, socket_path, split_words, version_from_reply, wildcard_matches,
        workspace_names_from_reply, Config, JsonParser, TreeReply, IPC_MAGIC, RUN_COMMAND,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::io::Read;
    use std::path::{Path, PathBuf};

    /// Hands out at most `chunk` bytes per read, like a socket under load.
    struct Fragmented<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl Read for Fragmented<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.chunk.min(buffer.len()).min(self.data.len());
            buffer[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }

    fn message(payload: &[u8]) -> Vec<u8> {
        let mut message = IPC_MAGIC.to_vec();
        message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        message.extend_from_slice(&9u32.to_ne_bytes());
        message.extend_from_slice(payload);
        message
    }

    #[test]
    fn reply_is_read_whole_however_it_is_fragmented() {
        let payload = vec![b'x'; 300 * 1024];
        let message = message(&payload);
        for chunk in [3, 1000, 64 * 1024, message.len()] {
            let mut reader = Fragmented {
                data: &message,
                chunk,
            };
            assert_eq!(read_reply(&mut reader).unwrap(), payload);
        }
    }

    #[test]
    fn socket_comes_from_the_detected_window_manager() {
        let i3 = || Some("/run/i3.sock".into());
        let sway = || Some("/run/sway.sock".into());
        // Under sway a stale I3SOCK does not win over SWAYSOCK.
        assert_eq!(socket_path("sway", i3(), sway()), Ok(sway().unwrap()));
        assert_eq!(socket_path("sway", None, sway()), Ok(sway().unwrap()));
        assert_eq!(socket_path("i3", i3(), sway()), Ok(i3().unwrap()));
        assert_eq!(socket_path("i3", i3(), None), Ok(i3().unwrap()));
        // No socket for the window manager is an error, not a guess.
        assert_eq!(
            socket_path("sway", i3(), None),
            Err("No IPC socket for window manager sway".to_owned())
        );
        assert_eq!(
            socket_path("Unknown", None, None),
            Err("No IPC socket for window manager Unknown".to_owned())
        );
    }

    #[test]
    fn short_or_foreign_replies_are_errors() {
        let message = message(b"{}");
        let mut truncated = Fragmented {
            data: &message[..message.len() - 1],
            chunk: 4,
        };
        assert!(read_reply(&mut truncated).is_err());
        let mut header_only = Fragmented {
            data: &message[..10],
            chunk: 4,
        };
        assert!(read_reply(&mut header_only).is_err());
        let mut foreign = Fragmented {
            data: b"not-ipc-header-at-all",
            chunk: 64,
        };
        assert!(read_reply(&mut foreign).is_err());
    }

    #[test]
    fn json_parser_handles_nested_ipc_values() {
        let json = JsonParser::new(br#"{"id":12,"ok":true,"name":"a\n\u263a","xs":[null]}"#)
            .parse()
            .unwrap();
        let object = json.object().unwrap();
        assert_eq!(object["id"].integer_string().unwrap(), "12");
        assert_eq!(object["name"].string().unwrap(), "a\n☺");
    }

    #[test]
    fn config_reply_keeps_each_i3_file_apart() {
        // A file without a final newline must not run into the next one.
        let json = JsonParser::new(
            br#"{"included_configs":[{"variable_replaced_contents":"one"},{"variable_replaced_contents":"two\n"}]}"#,
        )
        .parse()
        .unwrap();
        assert_eq!(
            config_from_reply(json, "i3", None).unwrap(),
            Config::I3 {
                files: vec!["one".to_owned(), "two\n".to_owned()],
                written: None,
            }
        );
        // i3 4.20 and later also send each file as written.
        let json = JsonParser::new(
            br#"{"included_configs":[{"raw_contents":"set $a b\n$a","variable_replaced_contents":"set b b\nb"}]}"#,
        )
        .parse()
        .unwrap();
        assert_eq!(
            config_from_reply(json, "i3", None).unwrap(),
            Config::I3 {
                files: vec!["set b b\nb".to_owned()],
                written: Some(vec!["set $a b\n$a".to_owned()]),
            }
        );
    }

    #[test]
    fn i3_variables_apply_as_i3_applies_them() {
        let replaced = replace_i3_variables(&[
            concat!(
                "bindsym $MOD+$key.long exec $term\n",
                "set $mod Mod4\n",
                "set $key F1\n",
                "set $key.long F2\n",
                "set_from_resource $term wm.term x-terminal\n",
                "set_from_resource $none wm.none\n",
                "set $alias $term\n",
                "bindsym $mod+Shift+\\\n",
                "  $key exec [$none] $alias\n",
                "# a comment ending in \\\n",
                "set $key F3\n",
            )
            .to_owned(),
            "bindsym $mod+$key exec $later\nset $later x\n".to_owned(),
        ]);
        assert_eq!(
            replaced[0],
            concat!(
                "bindsym Mod4+F2 exec x-terminal\n",
                "set Mod4 Mod4\n",
                "set F3 F1\n",
                "set F2 F2\n",
                "set_from_resource x-terminal wm.term x-terminal\n",
                "set_from_resource  wm.none\n",
                "set $term x-terminal\n",
                "bindsym Mod4+Shift+  F3 exec [] $term\n",
                "# a comment ending in \\\n",
                "set F3 F3\n",
            )
        );
        // Variables stay set for later files; values are not substituted
        // again.
        assert_eq!(replaced[1], "bindsym Mod4+F3 exec x\nset x x\n");
        let replaced = replace_i3_variables(&["set $b $a\nset $a 1\n$b".to_owned()]);
        assert_eq!(replaced[0], "set $a 1\nset 1 1\n$a");
    }

    #[test]
    fn sway_config_reply_is_the_main_file_as_written() {
        let json = JsonParser::new(br#"{"config":"set $a b\ninclude ~/x/*\n"}"#)
            .parse()
            .unwrap();
        let path = Some(PathBuf::from("/etc/sway/config"));
        assert_eq!(
            config_from_reply(json, "sway", path.clone()).unwrap(),
            Config::Sway {
                main: "set $a b\ninclude ~/x/*\n".to_owned(),
                path
            }
        );
    }

    #[test]
    fn run_request_carries_the_command_as_is() {
        let command = "exec notify-send \"caf\u{e9}  \\\"x\\\"\"";
        let request = encode_request(RUN_COMMAND, command).unwrap();
        assert_eq!(&request[..6], IPC_MAGIC);
        // The length counts bytes, not characters.
        let length = u32::from_ne_bytes(request[6..10].try_into().unwrap());
        assert_eq!(length as usize, command.len());
        assert_eq!(u32::from_ne_bytes(request[10..14].try_into().unwrap()), 0);
        assert_eq!(&request[14..], command.as_bytes());
        assert_eq!(encode_request(9, "").unwrap().len(), 14);
    }

    #[test]
    fn a_failed_command_is_an_error_with_its_reason() {
        let outcome = |reply: &[u8]| command_outcome(&JsonParser::new(reply).parse().unwrap());
        assert_eq!(outcome(br#"[{"success":true}]"#), Ok(()));
        assert_eq!(outcome(br#"[]"#), Ok(()));
        assert_eq!(
            outcome(br#"[{"success":true},{"success":false,"error":"caf\u00e9 \"x\""}]"#),
            Err("caf\u{e9} \"x\"".to_owned())
        );
        assert_eq!(
            outcome(br#"[{"success":false}]"#),
            Err("unknown error".to_owned())
        );
        assert!(outcome(br#"{"success":true}"#).is_err());
    }

    #[test]
    fn workspace_names_come_from_every_workspace_reported() {
        let names =
            |reply: &[u8]| workspace_names_from_reply(&JsonParser::new(reply).parse().unwrap());
        assert_eq!(
            names(
                br#"[{"num":1,"name":"1","focused":false},
                     {"num":2,"name":"2:web","focused":true,"output":"HDMI-1"},
                     {"num":-1,"name":"music","focused":false}]"#
            ),
            Ok(vec!["1".to_owned(), "2:web".to_owned(), "music".to_owned()])
        );
        assert_eq!(names(b"[]"), Ok(Vec::new()));
        assert!(names(br#"[{"num":1}]"#).is_err());
        assert!(names(br#"{"name":"1"}"#).is_err());
    }

    #[test]
    fn version_reply_preserves_all_five_strings() {
        let json = JsonParser::new(
            br#"{"human_readable":"4.22","loaded_config_file_name":"/tmp/i3","minor":"22","patch":"0","major":"4"}"#,
        )
        .parse()
        .unwrap();
        let version = version_from_reply(json).unwrap();
        assert_eq!(version.human_readable, "4.22");
        assert_eq!(version.loaded_config_file_name, "/tmp/i3");
        assert_eq!(version.minor, "22");
        assert_eq!(version.patch, "0");
        assert_eq!(version.major, "4");
    }

    #[test]
    fn version_reply_takes_numbers_as_i3_and_sway_send_them() {
        let json = JsonParser::new(
            br#"{"human_readable":"1.11","loaded_config_file_name":"/etc/sway/config","major":1,"minor":11,"patch":0}"#,
        )
        .parse()
        .unwrap();
        let version = version_from_reply(json).unwrap();
        assert_eq!(version.loaded_config_file_name, "/etc/sway/config");
        assert_eq!(version.major, "1");
        assert_eq!(version.minor, "11");
        assert_eq!(version.patch, "0");
    }

    #[test]
    fn expansion_and_glob_matching_follow_sway() {
        std::env::set_var("ILIA_TEST_ROOT", "/root dir");
        std::env::set_var("ILIA_TEST_TWO", " /a\t/b ");
        std::env::remove_var("ILIA_TEST_UNSET");
        let words = |text: &str| {
            expand_shell_words(text, &mut HashMap::new()).map(|words| {
                let words: Vec<_> = words.into_iter().map(|word| word.text).collect();
                words.join("|")
            })
        };
        // Values are split at blanks again; quotes keep text together.
        assert_eq!(
            words("$ILIA_TEST_ROOT/a/\"${ILIA_TEST_ROOT}\""),
            Some("/root|dir/a//root dir".to_owned())
        );
        assert_eq!(words("x$ILIA_TEST_TWO"), Some("x|/a|/b".to_owned()));
        // A name that is not set stands for nothing; a `$` without a name
        // for itself.
        assert_eq!(
            words("/a/$ILIA_TEST_UNSET ${ILIA_TEST_UNSET}/b $ILIA_TEST_UNSET $/"),
            Some("/a/|/b|$/".to_owned())
        );
        assert_eq!(
            words("'a  b' c\\ d \"e\"'f'"),
            Some("a  b|c d|ef".to_owned())
        );
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(words("~/x ~ a~"), Some(format!("{home}/x|{home}|a~")));
        }
        assert_eq!(words("~x/y"), Some("~x/y".to_owned()));
        // Shell syntax, unquoted, makes the whole text unusable.
        for syntax in [
            "a |", "a&", "a;b", "<a", "a>", "(a)", "{a,b}", "a}", "'a", "a\\", "${A", "${A:b}",
            "${A/b}", "${1}", "${#A:-b}", "${A:-b",
        ] {
            assert_eq!(words(syntax), None, "{syntax}");
        }
        assert_eq!(words("'(a)' \"{a;b}\" \\|"), Some("(a)|{a;b}||".to_owned()));
        let fits =
            |pattern: &str, text: &str| wildcard_matches(pattern.as_bytes(), text.as_bytes());
        assert!(fits("[ab]*.conf", "alpha.conf"));
        assert!(!fits("[ab]*.conf", "zeta.conf"));
        assert!(fits("[0-4]?_*", "10_base"));
        assert!(!fits("[!0-4]?_*", "10_base"));
        assert!(fits("[]x]", "]"));
        assert!(fits("a[b", "a[b"));
        // Bytes, as in sway's C locale: `é` is two of them.
        assert!(!fits("caf?", "café"));
        assert!(fits("caf??", "café"));
        assert!(!fits("caf[é]", "café"));
        assert!(fits("é*", "éü"));
        assert!(!fits("a*b", "acd"));
        assert!(fits("a\\*", "a*"));
        assert!(!fits("a\\*", "ab"));
        assert!(fits("[\\]a]", "]"));
        assert!(fits("[\\]a]", "a"));
        assert!(fits("*a*b*", "xaybz"));
        assert!(fits("a*", "a"));
        assert!(!fits("*a", "b"));
        assert!(fits("*?[!x]", "ab"));
        assert!(!fits("[!a-c]", "b"));
        // Many stars against a long name that almost fits: answered at once,
        // not after trying every way to share out the text.
        let long = "a".repeat(250);
        assert!(!fits(&"*a".repeat(12).replace("*a*a", "*a*a*b"), &long));
        assert!(fits(&"*a".repeat(12), &long));
        assert_eq!(
            expand_glob("/nonexistent/x", None),
            [PathBuf::from("/nonexistent/x")]
        );
        // A pattern that matches nothing stands for itself.
        assert_eq!(
            expand_glob("/nonexistent/*", None),
            [PathBuf::from("/nonexistent/*")]
        );
        // An include reads existing files only, sorted, and not hidden ones.
        let directory = scratch("paths");
        fs::create_dir(directory.join("sub")).unwrap();
        for name in ["b", "a", ".hidden"] {
            fs::write(directory.join(name), "").unwrap();
        }
        let canonical = fs::canonicalize(&directory).unwrap();
        assert_eq!(
            include_paths("*", Some(&directory), &mut HashMap::new()),
            [canonical.join("a"), canonical.join("b")]
        );
        assert!(include_paths("missing", Some(&directory), &mut HashMap::new()).is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tree_parsing_and_navigability_inputs_are_preserved() {
        let json = JsonParser::new(
            br#"{"id":1,"type":"root","urgent":false,"name":"root","nodes":[{"id":2,"type":"con","urgent":true,"name":"Editor","window_type":"normal","window_properties":{"class":"Editor","instance":"editor"}}]}"#,
        )
        .parse()
        .unwrap();
        let tree = TreeReply::from_json(&json).unwrap();
        assert_eq!(tree.nodes[0].id, "2");
        assert_eq!(
            tree.nodes[0]
                .window_properties
                .as_ref()
                .unwrap()
                .class
                .as_deref(),
            Some("Editor")
        );
    }

    fn lines(main: &str, resources: &[(&str, &str)]) -> Vec<String> {
        let resources: HashMap<_, _> = resources
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        walk_included_configs(main, None, &resources)
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// A fresh directory for one test's files.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("ilia-sway-config-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// The lines `read_config_files` reads, in one run and without the
    /// lines that define variables or include files.
    fn walk_included_configs(
        main: &str,
        main_path: Option<&Path>,
        resources: &HashMap<String, String>,
    ) -> Vec<String> {
        read_config_files(main, main_path, resources)
            .into_iter()
            .flatten()
            .filter(|line| {
                !split_words(line).first().is_some_and(|keyword| {
                    ["set", "set_from_resource", "include"]
                        .iter()
                        .any(|directive| keyword.eq_ignore_ascii_case(directive))
                })
            })
            .collect()
    }

    #[test]
    fn words_split_at_blanks_outside_quotes_and_criteria() {
        assert_eq!(
            split_words("  bindsym\t$mod+q   exec  a "),
            ["bindsym", "$mod+q", "exec", "a"]
        );
        assert_eq!(
            split_words(r#"exec notify-send "Two  Words" 'it''s  x'"#),
            ["exec", "notify-send", r#""Two  Words""#, "'it''s  x'"]
        );
        assert_eq!(
            split_words(r#"[title="a ] b" class='c  d'] focus"#),
            [r#"[title="a ] b" class='c  d']"#, "focus"]
        );
        // Only a bracket that starts a word opens criteria.
        assert_eq!(split_words("exec a[b c]"), ["exec", "a[b", "c]"]);
        assert_eq!(
            split_words(r#"exec "say \"x  y\"" a\ b"#),
            ["exec", r#""say \"x  y\"""#, r"a\ b"]
        );
        assert_eq!(split_words("exec café  ü"), ["exec", "café", "ü"]);
        assert!(split_words(" \t ").is_empty());
    }

    #[test]
    fn variables_have_their_value_where_they_are_used() {
        assert_eq!(
            lines(
                "set $a first\nbindsym x exec $a\nset $a second\nbindsym y exec $a $b\nset $b later",
                &[]
            ),
            ["bindsym x exec first", "bindsym y exec second $b"]
        );
    }

    #[test]
    fn the_longest_defined_name_wins() {
        assert_eq!(
            lines(
                "set $t.s short\nset $t.s.long long\nbindsym x exec $t.s.long $t.s $t.s.other $t",
                &[]
            ),
            ["bindsym x exec long short short.other $t"]
        );
    }

    #[test]
    fn values_are_joined_as_sway_joins_words() {
        assert_eq!(
            lines(
                "set $w  a    b \"Two  Words\"\nbindsym x exec    $w\t[class=\"x  y\"]  'q  r'",
                &[]
            ),
            [r#"bindsym x exec a b "Two  Words" [class="x  y"] 'q  r'"#]
        );
        // A value is split again where it is used.
        assert_eq!(
            lines(
                "set_from_resource $r key\nbindsym x exec $r",
                &[("key", "a   b")]
            ),
            ["bindsym x exec a b"]
        );
    }

    #[test]
    fn comments_and_names_in_them_stay_as_written() {
        assert_eq!(
            lines(
                "set $a value\n## Cat // $a  label // <> x ##\n# set $b nope\nbindsym x exec $b",
                &[]
            ),
            [
                "## Cat // $a  label // <> x ##",
                "# set $b nope",
                "bindsym x exec $b"
            ]
        );
    }

    #[test]
    fn resources_win_over_defaults_unless_empty() {
        let resources = [("set.key", "from resource"), ("empty.key", "")];
        assert_eq!(
            lines(
                "set $k set.key\n\
                 set_from_resource $a $k default\n\
                 set_from_resource $b empty.key \"Two  Words\"   x\n\
                 set_from_resource $c missing.key\n\
                 set $d kept\n\
                 set_from_resource $d missing.key\n\
                 set_from_resource $e missing.key \"\"\n\
                 bindsym x exec $a|$b|$c|$d|$e",
                &resources
            ),
            [r#"bindsym x exec from resource|"Two  Words" x|$c|kept|"""#]
        );
    }

    #[test]
    fn files_are_read_in_runs_with_their_directives() {
        let directory = scratch("runs");
        fs::write(directory.join("a"), "## A // B // c ##\nbindsym a exec a").unwrap();
        let main = format!(
            "set $x y\n## X // Y // z ##\ninclude {}/a\nbindsym $x exec x\n",
            directory.display()
        );
        let files = read_config_files(&main, None, &HashMap::new());
        assert_eq!(
            files,
            [
                vec![
                    "set $x y".to_owned(),
                    "## X // Y // z ##".to_owned(),
                    format!("include {}/a", directory.display()),
                ],
                vec![
                    "## A // B // c ##".to_owned(),
                    "bindsym a exec a".to_owned()
                ],
                vec!["bindsym y exec x".to_owned()],
            ]
        );
        // The plain reading is the same lines without the directives.
        assert_eq!(
            walk_included_configs(&main, None, &HashMap::new()),
            [
                "## X // Y // z ##",
                "## A // B // c ##",
                "bindsym a exec a",
                "bindsym y exec x"
            ]
        );
    }

    #[test]
    fn includes_are_read_in_place_sorted_and_once() {
        let directory = scratch("includes");
        let parts = directory.join("parts");
        fs::create_dir(&parts).unwrap();
        fs::write(parts.join("20_b"), "bindsym b exec $v\nset $v from-b").unwrap();
        fs::write(parts.join("10_a"), "set $v from-a\nbindsym a exec $v\n").unwrap();
        fs::write(parts.join(".hidden"), "bindsym h exec hidden\n").unwrap();
        fs::write(
            directory.join("nested"),
            "include parts/10_a\nbindsym n exec $v\n",
        )
        .unwrap();
        let main = directory.join("config");
        let text = "set $v main\n\
                    include parts/*\n\
                    bindsym m exec $v\n\
                    include nested\n\
                    include config\n\
                    include ./parts/../parts/20_b\n";
        fs::write(&main, text).unwrap();
        let read = walk_included_configs(text, Some(&main), &HashMap::new());
        let read: Vec<_> = read.iter().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            read,
            [
                "bindsym a exec from-a",
                "bindsym b exec from-a",
                "bindsym m exec from-b",
                "bindsym n exec from-b",
            ]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn include_patterns_take_variables_and_the_environment() {
        let directory = scratch("patterns");
        fs::write(directory.join("first"), "bindsym f exec first\n").unwrap();
        fs::write(directory.join("second"), "bindsym s exec second\n").unwrap();
        std::env::set_var("ILIA_TEST_PARTS", &directory);
        let text = "set $file first\n\
                    include $ILIA_TEST_PARTS/$file\n\
                    set $file second\n\
                    include ${ILIA_TEST_PARTS}/$file\n";
        assert_eq!(
            lines(text, &[]),
            ["bindsym f exec first", "bindsym s exec second"]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lines_ending_in_a_backslash_go_on_with_the_next() {
        assert_eq!(
            continued_lines("a \\\n  b\\\n\tc\nd\n# e \\\nf\n  # g \\\nh\ni \\ \nj\\"),
            ["a   b\tc", "d", "# e \\", "f", "  # g h", "i \\ ", "j\\"]
        );
        assert!(continued_lines("").is_empty());
    }

    #[test]
    fn include_arguments_lose_the_quotes_of_words_starting_with_one() {
        assert_eq!(
            include_argument("  \"/a b\"\t'/c'  /d/'e f'  /g\\ h \"i\"j'k'"),
            "/a b /c /d/'e f' /g\\ h ijk"
        );
        assert_eq!(include_argument(" \t "), "");
    }

    #[test]
    fn include_lines_are_read_as_sway_reads_them() {
        let directory = scratch("lines");
        for name in ["a", "b", "c d", "e", "f(g)"] {
            fs::write(directory.join(name), format!("bindsym {name} exec x\n")).unwrap();
        }
        // A file that is not UTF-8 is read all the same.
        fs::write(directory.join("e"), b"bindsym e exec x\n# caf\xe9\n").unwrap();
        std::env::set_var("ILIA_TEST_PAIR", "b\tc\\ d");
        std::env::remove_var("ILIA_TEST_NONE");
        let main = directory.join("config");
        let text = "SET $e e\n\
                    set $pair a \\\n  $e\n\
                    INCLUDE $pair\n\
                    include \"c d\" f(g)\n\
                    Include $ILIA_TEST_NONE'f(g)'\n\
                    include $ILIA_TEST_PAIR\n\
                    include c\\ d\n";
        fs::write(&main, text).unwrap();
        let read = walk_included_configs(text, Some(&main), &HashMap::new());
        let read: Vec<_> = read.iter().filter(|line| !line.is_empty()).collect();
        // The environment's value is split once more: `c\ d` is two words.
        assert_eq!(
            read,
            [
                "bindsym a exec x",
                "bindsym e exec x",
                "# caf\u{fffd}",
                "bindsym f(g) exec x",
                "bindsym b exec x",
                "bindsym c d exec x",
            ]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_word_becomes_a_pattern_at_a_wildcard_written_unquoted() {
        std::env::set_var("ILIA_TEST_STAR", "*.conf");
        std::env::set_var("ILIA_TEST_FIELDS", "/a/* /b/c");
        let words = |text: &str| {
            expand_shell_words(text, &mut HashMap::new()).map(|words| {
                let words: Vec<_> = words
                    .into_iter()
                    .map(|word| format!("{}{}", word.text, if word.pattern { "*" } else { "" }))
                    .collect();
                words.join("|")
            })
        };
        // Quoted, backslashed or expanded wildcards stand for themselves...
        assert_eq!(
            words("d/'*'.conf d/\"?\" 'x[1]' \\* d/$ILIA_TEST_STAR ${ILIA_TEST_STAR}"),
            Some("d/*.conf|d/?|x[1]|*|d/*.conf|*.conf".to_owned())
        );
        // ...until a wildcard written unquoted makes the whole word a pattern.
        assert_eq!(
            words("d/'*'* d/*'?' d/\\** $ILIA_TEST_STAR* \"d/\"*"),
            Some("d/***|d/*?*|d/***|*.conf**|d/**".to_owned())
        );
        // Fields a value splits off before that point stay as they are; the
        // ones after it are patterns.
        assert_eq!(
            words("${ILIA_TEST_FIELDS}x* d*$ILIA_TEST_FIELDS"),
            Some("/a/*|/b/cx**|d*/a/**|/b/c*".to_owned())
        );
        std::env::remove_var("ILIA_TEST_STAR");
        std::env::remove_var("ILIA_TEST_FIELDS");
    }

    #[test]
    fn parameter_forms_expand_as_sway_expands_them() {
        std::env::set_var("ILIA_TEST_SET", "/s");
        std::env::set_var("ILIA_TEST_EMPTY", "");
        std::env::set_var("ILIA_TEST_LONG", "/d/x.a.b");
        std::env::remove_var("ILIA_TEST_UNSET");
        std::env::remove_var("ILIA_TEST_ASSIGNED");
        let mut environment = HashMap::new();
        let mut words = |text: &str| {
            expand_shell_words(text, &mut environment).map(|words| {
                let words: Vec<_> = words
                    .into_iter()
                    .map(|word| format!("{}{}", word.text, if word.pattern { "*" } else { "" }))
                    .collect();
                words.join("|")
            })
        };
        assert_eq!(
            words("${ILIA_TEST_UNSET:-/a} ${ILIA_TEST_UNSET-/b} ${ILIA_TEST_EMPTY:-/c}"),
            Some("/a|/b|/c".to_owned())
        );
        assert_eq!(
            words("x${ILIA_TEST_EMPTY-/d} ${ILIA_TEST_SET:-/e} ${ILIA_TEST_SET-/f}"),
            Some("x|/s|/s".to_owned())
        );
        assert_eq!(
            words("${ILIA_TEST_SET:+/g} x${ILIA_TEST_EMPTY:+/h} ${ILIA_TEST_EMPTY+/i}"),
            Some("/g|x|/i".to_owned())
        );
        assert_eq!(
            words("x${ILIA_TEST_UNSET+/j} ${ILIA_TEST_SET:?no} x${ILIA_TEST_UNSET:?no}"),
            Some("x|/s|x".to_owned())
        );
        // The word is expanded: quotes go, values come in, it is split, and
        // its wildcards stand for themselves.
        assert_eq!(
            words("${ILIA_TEST_UNSET:-'/k l' $ILIA_TEST_SET/*.conf ${ILIA_TEST_UNSET:-{m}}}"),
            Some("/k|l|/s/*.conf|{m}".to_owned())
        );
        // A backslash in the word stays.
        assert_eq!(words("${ILIA_TEST_UNSET:-a\\.b}"), Some("a\\.b".to_owned()));
        assert_eq!(words("*${ILIA_TEST_UNSET:-'?'}"), Some("*\\?*".to_owned()));
        assert_eq!(
            words("${#ILIA_TEST_LONG} ${#ILIA_TEST_UNSET} ${ILIA_TEST_LONG%.*}"),
            Some("8|0|/d/x.a".to_owned())
        );
        assert_eq!(
            words("${ILIA_TEST_LONG%%.*} ${ILIA_TEST_LONG#*.} ${ILIA_TEST_LONG##*.}"),
            Some("/d/x|a.b|b".to_owned())
        );
        assert_eq!(words("${ILIA_TEST_LONG#nope}"), Some("/d/x.a.b".to_owned()));
        // A word that is not used is not expanded: it assigns nothing.
        assert_eq!(
            words("${ILIA_TEST_SET:-${ILIA_TEST_ASSIGNED:=/n}} x$ILIA_TEST_ASSIGNED"),
            Some("/s|x".to_owned())
        );
        // An assignment holds for what comes after it.
        assert_eq!(
            words("${ILIA_TEST_ASSIGNED:=/o /p} ${ILIA_TEST_EMPTY:=/q}"),
            Some("/o|/p|/q".to_owned())
        );
        assert_eq!(
            words("$ILIA_TEST_ASSIGNED ${ILIA_TEST_EMPTY}"),
            Some("/o|/p|/q".to_owned())
        );
        assert_eq!(std::env::var_os("ILIA_TEST_ASSIGNED"), None);
        for name in ["ILIA_TEST_SET", "ILIA_TEST_EMPTY", "ILIA_TEST_LONG"] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn includes_read_wildcards_as_sway_does() {
        let directory = scratch("wildcards");
        for name in [
            "a.conf",
            "*.conf",
            "x[1].conf",
            "x1.conf",
            "[ab]",
            "q?.conf",
            "qy.conf",
        ] {
            fs::write(directory.join(name), "").unwrap();
        }
        let canonical = fs::canonicalize(&directory).unwrap();
        std::env::set_var("ILIA_TEST_PATTERN", "x[1].conf");
        let paths = |argument: &str| {
            include_paths(argument, Some(&directory), &mut HashMap::new())
                .into_iter()
                .map(|path| path.strip_prefix(&canonical).unwrap().display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths("./'*'.conf"), ["*.conf"]);
        assert_eq!(paths("./\"x[1]\".conf"), ["x[1].conf"]);
        assert_eq!(paths("$ILIA_TEST_PATTERN"), ["x[1].conf"]);
        assert_eq!(paths("x[1].conf"), ["x1.conf"]);
        // A pattern without matches names itself.
        assert_eq!(paths("[ab]"), ["[ab]"]);
        // Once a pattern, quoted wildcards match too; an expanded word's
        // quoted ones do not.
        assert_eq!(paths("q'?'*"), ["q?.conf", "qy.conf"]);
        assert_eq!(paths("q*${ILIA_TEST_UNSET:-'?'}.conf"), ["q?.conf"]);
        assert_eq!(paths("${ILIA_TEST_UNSET:-*.conf}"), ["*.conf"]);
        std::env::remove_var("ILIA_TEST_PATTERN");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn patterns_take_shell_characters_and_end_at_quoted_blanks_as_glibc_does() {
        let words = |text: &str| {
            expand_shell_words(text, &mut HashMap::new()).map(|words| {
                let words: Vec<_> = words
                    .into_iter()
                    .map(|word| format!("{}{}", word.text, if word.pattern { "*" } else { "" }))
                    .collect();
                words.join("|")
            })
        };
        // Once the word is a pattern, shell characters are part of it.
        assert_eq!(
            words("a*(b) c*{d}|e;f x"),
            Some("a*(b)*|c*{d}|e;f*|x".to_owned())
        );
        assert_eq!(words("x(* y"), None);
        // A quoted blank ends the pattern, and the closing quote then opens a
        // quotation that is never closed.
        assert_eq!(words("m*\"y file\" b"), None);
        assert_eq!(words("m*'y' b"), Some("m*y*|b".to_owned()));
        // Nothing is ever run: backquotes and `$(` leave the include empty,
        // also inside double quotes, but a pattern takes a backquote as is.
        for text in ["`x`", "\"`x`\"", "$(x)", "a*$(x)", "$((1))"] {
            assert_eq!(words(text), None, "{text}");
        }
        assert_eq!(words("a*`"), Some("a*`*".to_owned()));
        std::env::remove_var("ILIA_TEST_UNSET");
        if let Ok(home) = std::env::var("HOME") {
            // `~` with no text before it in its word, or after the first
            // word's `=`.
            assert_eq!(
                words("''~/x $ILIA_TEST_UNSET~/y ~:z a=~/w"),
                Some(format!("{home}/x|{home}/y|{home}:z|a=~/w"))
            );
            assert_eq!(words("a=~/w"), Some(format!("a={home}/w")));
            assert_eq!(words("a=b:~/w"), Some(format!("a=b:{home}/w")));
            assert_eq!(words("b:~/w"), Some("b:~/w".to_owned()));
        }
        // A trim's word is expanded, and assigns, even with nothing to trim.
        assert_eq!(
            words("${ILIA_TEST_UNSET#${ILIA_TEST_TRIM:=/q}} $ILIA_TEST_TRIM"),
            Some("/q".to_owned())
        );
    }

    #[test]
    fn include_arguments_are_unescaped_as_sway_unescapes_them() {
        assert_eq!(
            include_argument(r#"a\\b x\ty "q\"r" \x41\x c\ d \0z"#),
            "a\\b x\ty q\"r A c\\ d "
        );
        // `\"` reaches the expansion as a quote that is never closed.
        assert_eq!(
            expand_shell_words(&include_argument(r#"q\"r"#), &mut HashMap::new()),
            None
        );
    }

    #[test]
    fn variables_skip_escaped_and_doubled_dollars() {
        assert_eq!(
            lines(
                "set $h h.conf\nbindsym a exec echo $$h \\$h \\\\$h $h $$$h\n",
                &[]
            ),
            ["bindsym a exec echo $h \\$h \\\\h.conf h.conf $h.conf"]
        );
    }

    #[test]
    fn includes_skip_cycles_dangling_links_and_files_under_a_final_slash() {
        let directory = scratch("edges");
        for part in ["sub", "other", "cyc"] {
            fs::create_dir(directory.join(part)).unwrap();
        }
        fs::write(directory.join("base.conf"), "").unwrap();
        fs::write(directory.join("sub/config"), "").unwrap();
        std::os::unix::fs::symlink(directory.join("missing"), directory.join("link")).unwrap();
        fs::write(directory.join("cyc/a"), "bindsym a exec a\ninclude b\n").unwrap();
        fs::write(directory.join("cyc/b"), "bindsym b exec b\ninclude a\n").unwrap();
        let canonical = fs::canonicalize(&directory).unwrap();
        let paths = |argument: &str| include_paths(argument, Some(&directory), &mut HashMap::new());
        // A final `/` matches directories only, which are not read.
        assert!(paths("base.con*/").is_empty());
        assert!(paths("su*/").is_empty());
        // A fixed last part must exist under each match.
        assert_eq!(paths("*/config"), [canonical.join("sub/config")]);
        // A link to nothing is not read.
        assert!(paths("lin*").is_empty());
        // Files that include each other are each read once.
        let main = directory.join("config");
        assert_eq!(
            walk_included_configs("include cyc/a\n", Some(&main), &HashMap::new()),
            ["bindsym a exec a", "bindsym b exec b"]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_linked_main_file_reads_its_includes_beside_the_link() {
        let directory = scratch("link");
        for part in ["real", "links"] {
            fs::create_dir(directory.join(part)).unwrap();
            fs::write(
                directory.join(part).join("part"),
                format!("bindsym {part} exec x\ninclude config\n"),
            )
            .unwrap();
        }
        let text = "include part\n";
        fs::write(directory.join("real/config"), text).unwrap();
        std::os::unix::fs::symlink(
            directory.join("real/config"),
            directory.join("links/config"),
        )
        .unwrap();
        let main = directory.join("links/config");
        // The main file, reached through the link again, is not read twice.
        assert_eq!(
            walk_included_configs(text, Some(&main), &HashMap::new()),
            ["bindsym links exec x"]
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
