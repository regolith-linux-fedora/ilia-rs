use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_SIZE: usize = 14;

pub const RUN_COMMAND: u32 = 0;
pub const GET_TREE: u32 = 4;
pub const GET_VERSION: u32 = 7;
pub const GET_CONFIG: u32 = 9;

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
    let socket_path = socket_path(
        wm_name,
        std::env::var_os("I3SOCK"),
        std::env::var_os("SWAYSOCK"),
    )?;
    let mut stream = UnixStream::connect(&socket_path).map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(3));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;

    stream
        .write_all(&encode_request(command, payload)?)
        .map_err(|error| error.to_string())?;

    let payload = read_reply(&mut stream)?;
    if payload.is_empty() {
        return Err("No Response".to_owned());
    }
    JsonParser::new(&payload).parse()
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
    let mut header = [0u8; HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .map_err(|error| format!("reading the reply header failed: {error}"))?;
    if &header[..IPC_MAGIC.len()] != IPC_MAGIC {
        return Err("invalid reply header".to_owned());
    }
    // The window manager writes the length in its native byte order.
    let length = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("reading the {length}-byte reply failed: {error}"))?;
    Ok(payload)
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

fn config_from_reply(reply: Json, wm_name: &str, path: Option<PathBuf>) -> Result<Config, String> {
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

/// `path` with `$NAME` and `${NAME}` replaced by the session environment's
/// value, and a leading `~` by the home directory. A variable that is not
/// set stays as written.
pub fn expand_path_vars(path: &str) -> String {
    let path = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("$HOME{rest}"),
        _ => path.to_owned(),
    };
    let mut result = String::with_capacity(path.len());
    let mut rest = path.as_str();
    while let Some(dollar) = rest.find('$') {
        result.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let (name, written) = match after.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            },
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], end)
            }
        };
        let value = is_environment_name(name)
            .then(|| std::env::var(name).ok())
            .flatten();
        match value {
            Some(value) => {
                result.push_str(&value);
                rest = &after[written..];
            }
            None => {
                result.push('$');
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

fn is_environment_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The files an `include` reads, in the order sway reads them: `pattern`
/// with the session environment applied, taken from `directory` (the one
/// holding the file that names it) when relative, and its matches sorted.
fn include_paths(pattern: &str, directory: Option<&Path>) -> Vec<PathBuf> {
    let pattern = PathBuf::from(expand_path_vars(pattern));
    let pattern = match directory {
        Some(directory) if pattern.is_relative() => directory.join(pattern),
        _ => pattern,
    };
    expand_glob(&pattern)
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

/// sway's configuration as sway itself reads it, since sway reports only its
/// main file, as written: the lines in sway's reading order, each `include`
/// read in place, the files a pattern matches in alphabetical order and each
/// file once. Comments stay as written; every other line has its variables
/// (`set`, and `set_from_resource` with `resources`) replaced by the value
/// they have where they are used, and its words joined by single spaces with
/// quoted words kept whole. The lines that define variables or include files
/// stay in place as written, after they have taken effect.
///
/// The lines come in runs that each hold the lines of one file: a run ends
/// where an included file starts or ends.
///
/// `main` is the text of the main file and `main_path` the file, when known;
/// relative includes in `main` are read beside it.
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
}

impl<'a> ConfigWalk<'a> {
    fn new(resources: &'a HashMap<String, String>) -> Self {
        Self {
            resources,
            variables: HashMap::new(),
            visited: HashSet::new(),
            lines: Vec::new(),
            breaks: Vec::new(),
        }
    }

    fn walk(mut self, main: &str, main_path: Option<&Path>) -> Self {
        let directory = main_path.and_then(|path| {
            let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            let directory = path.parent().map(Path::to_owned);
            // An include that names the main file does not read it again.
            self.visited.insert(path);
            directory
        });
        self.read_text(main, directory.as_deref());
        self
    }

    /// Reads the text of one file; `directory` holds the file.
    fn read_text(&mut self, text: &str, directory: Option<&Path>) {
        self.file_break();
        for line in text.lines() {
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
        match split_words(line).as_slice() {
            ["include", pattern @ ..] if !pattern.is_empty() => {
                self.keep(line);
                let pattern = self.replace(&pattern.join(" "));
                for path in include_paths(&pattern, directory) {
                    if !self.visited.insert(path.clone()) {
                        continue;
                    }
                    match fs::read_to_string(&path) {
                        Ok(text) => self.read_text(&text, path.parent()),
                        Err(error) => eprintln!("Cannot read {}: {error}", path.display()),
                    }
                }
            }
            ["set", name, value @ ..] if name.starts_with('$') && !value.is_empty() => {
                self.keep(line);
                let value = join_words(&self.replace(&value.join(" ")));
                self.variables.insert(name[1..].to_owned(), value);
            }
            ["set_from_resource", name, key, default @ ..] if name.starts_with('$') => {
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
    /// that is not defined stays as written.
    fn replace(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(dollar) = rest.find('$') {
            result.push_str(&rest[..dollar]);
            let after = &rest[dollar + 1..];
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

/// The paths that `pattern` matches, sorted. A part of the pattern with
/// wildcards matches the names `wildcard_matches` accepts, except that a name
/// that starts with a dot is only matched by a part that starts with a dot
/// too. A pattern without wildcards stands for itself, whether or not it
/// exists.
fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::new()];
    for component in pattern.components() {
        let segment = component.as_os_str().to_string_lossy();
        let wildcard =
            matches!(component, Component::Normal(_)) && segment.contains(['*', '?', '[']);
        let segment: Vec<char> = segment.chars().collect();
        let mut next = Vec::new();
        for candidate in candidates {
            if wildcard {
                let directory = if candidate.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    candidate.as_path()
                };
                let Ok(entries) = fs::read_dir(directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let text: Vec<char> = name.to_string_lossy().chars().collect();
                    let hidden = text.first() == Some(&'.') && segment[0] != '.';
                    if !hidden && wildcard_matches(&segment, &text) {
                        next.push(candidate.join(name));
                    }
                }
            } else {
                next.push(candidate.join(component));
            }
        }
        candidates = next;
    }
    candidates.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    candidates
}

/// Whether `text` fits the glob `pattern`: `*` stands for any run of
/// characters, `?` for one character and `[...]` for one character of a set,
/// in which `!` or `^` first negates the set, a `]` first is a member and
/// `a-z` is a range. An unclosed `[` stands for itself.
fn wildcard_matches(pattern: &[char], text: &[char]) -> bool {
    fn matches(pattern: &[char], text: &[char]) -> bool {
        match pattern.first().copied() {
            None => text.is_empty(),
            Some('*') => {
                matches(&pattern[1..], text) || (!text.is_empty() && matches(pattern, &text[1..]))
            }
            Some('?') => !text.is_empty() && matches(&pattern[1..], &text[1..]),
            Some('[') => {
                let negate = matches!(pattern.get(1), Some('!' | '^'));
                let start = if negate { 2 } else { 1 };
                // A `]` right at the start of the set is a member, not its end.
                let Some(end) = pattern
                    .iter()
                    .skip(start + 1)
                    .position(|c| *c == ']')
                    .map(|position| start + 1 + position)
                else {
                    return !text.is_empty()
                        && text[0] == '['
                        && matches(&pattern[1..], &text[1..]);
                };
                if text.is_empty() {
                    return false;
                }
                let class = &pattern[start..end];
                let mut hit = false;
                let mut index = 0;
                while index < class.len() {
                    if index + 2 < class.len() && class[index + 1] == '-' {
                        hit |= (class[index]..=class[index + 2]).contains(&text[0]);
                        index += 3;
                    } else {
                        hit |= class[index] == text[0];
                        index += 1;
                    }
                }
                hit != negate && matches(&pattern[end + 1..], &text[1..])
            }
            Some(c) => !text.is_empty() && c == text[0] && matches(&pattern[1..], &text[1..]),
        }
    }
    matches(pattern, text)
}

/// Runs `command` in the window manager, in a message of its own: the text
/// reaches it unchanged, with no shell or command line in between. A command
/// the window manager reports as failed is an error carrying its reason.
pub fn run_command(wm_name: &str, command: &str) -> Result<(), String> {
    command_outcome(&request(wm_name, RUN_COMMAND, command)?)
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
        command_outcome, config_from_reply, encode_request, expand_glob, expand_path_vars,
        include_paths, read_config_files, read_reply, replace_i3_variables, socket_path,
        split_words, version_from_reply, wildcard_matches, Config, JsonParser, TreeReply,
        IPC_MAGIC, RUN_COMMAND,
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
        std::env::remove_var("ILIA_TEST_UNSET");
        assert_eq!(
            expand_path_vars("$ILIA_TEST_ROOT/a/${ILIA_TEST_ROOT}"),
            "/root dir/a//root dir"
        );
        // A name that is not set, or is no name at all, stays as written.
        assert_eq!(
            expand_path_vars("/a/$ILIA_TEST_UNSET/${ILIA_TEST_UNSET}/$/${x"),
            "/a/$ILIA_TEST_UNSET/${ILIA_TEST_UNSET}/$/${x"
        );
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_path_vars("~/x"), format!("{home}/x"));
        }
        assert_eq!(expand_path_vars("~x/y"), "~x/y");
        let fits = |pattern: &str, text: &str| {
            let chars = |text: &str| text.chars().collect::<Vec<_>>();
            wildcard_matches(&chars(pattern), &chars(text))
        };
        assert!(fits("[ab]*.conf", "alpha.conf"));
        assert!(!fits("[ab]*.conf", "zeta.conf"));
        assert!(fits("[0-4]?_*", "10_base"));
        assert!(!fits("[!0-4]?_*", "10_base"));
        assert!(fits("[]x]", "]"));
        assert!(fits("a[b", "a[b"));
        assert!(fits("é?", "éü"));
        assert!(!fits("a*b", "acd"));
        assert_eq!(
            expand_glob(Path::new("/nonexistent/x")),
            [PathBuf::from("/nonexistent/x")]
        );
        assert!(expand_glob(Path::new("/nonexistent/*")).is_empty());
        // An include reads existing files only, sorted, and not hidden ones.
        let directory = scratch("paths");
        fs::create_dir(directory.join("sub")).unwrap();
        for name in ["b", "a", ".hidden"] {
            fs::write(directory.join(name), "").unwrap();
        }
        let canonical = fs::canonicalize(&directory).unwrap();
        assert_eq!(
            include_paths("*", Some(&directory)),
            [canonical.join("a"), canonical.join("b")]
        );
        assert!(include_paths("missing", Some(&directory)).is_empty());
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
                !matches!(
                    split_words(line).first(),
                    Some(&("set" | "set_from_resource" | "include"))
                )
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
}
