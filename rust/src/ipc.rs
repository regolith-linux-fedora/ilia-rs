use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_SIZE: usize = 14;
const MAX_REPLY_SIZE: usize = 128 * 1024;

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

fn request(command: u32) -> Result<Json, String> {
    let socket_path = std::env::var("I3SOCK").map_err(|error| error.to_string())?;
    let mut stream = UnixStream::connect(&socket_path).map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(3));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| error.to_string())?;

    let mut request = [0u8; HEADER_SIZE];
    request[..6].copy_from_slice(IPC_MAGIC);
    request[6..10].copy_from_slice(&0u32.to_le_bytes());
    request[10..14].copy_from_slice(&command.to_le_bytes());
    stream
        .write_all(&request)
        .map_err(|error| error.to_string())?;

    let mut reply = vec![0; MAX_REPLY_SIZE];
    let received = stream.read(&mut reply).map_err(|error| error.to_string())?;
    if received <= HEADER_SIZE {
        return Err("No Response".to_owned());
    }
    JsonParser::new(&reply[HEADER_SIZE..received]).parse()
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

#[allow(dead_code)]
pub fn get_version() -> Result<VersionReply, String> {
    version_from_reply(request(GET_VERSION)?)
}

fn version_from_reply(reply: Json) -> Result<VersionReply, String> {
    let object = reply.object()?;
    Ok(VersionReply {
        human_readable: member(object, "human_readable")?.string()?.to_owned(),
        loaded_config_file_name: member(object, "loaded_config_file_name")?
            .string()?
            .to_owned(),
        minor: member(object, "minor")?.string()?.to_owned(),
        patch: member(object, "patch")?.string()?.to_owned(),
        major: member(object, "major")?.string()?.to_owned(),
    })
}

pub fn get_config(wm_name: &str) -> Result<String, String> {
    config_from_reply(request(GET_CONFIG)?, wm_name)
}

fn config_from_reply(reply: Json, wm_name: &str) -> Result<String, String> {
    let object = reply.object()?;
    if wm_name == "sway" {
        let base = member(object, "config")?.string()?.to_owned();
        Ok(format!("{base}{}", walk_included_configs(&base)))
    } else {
        let mut config = String::new();
        for included in member(object, "included_configs")?.array()? {
            config.push_str(member(included.object()?, "variable_replaced_contents")?.string()?);
        }
        Ok(config)
    }
}

pub fn expand_path_vars(path: &str) -> String {
    if !path.contains('$') {
        return path.to_owned();
    }
    // Vala-exact split algorithm, quirks replicated (ConfigReply in
    // I3Ipc.vala; the reference behavior is buggy but the port matches
    // it): every `$`-separated segment is parsed as a variable name up
    // to the next `/`, so a `$` after literal leading text corrupts that
    // segment (`/lit/$V/x` gains a spurious leading `$`), and `$$`
    // collapses to a literal `$` while what follows is still expanded
    // (`$$V` -> `$` + value-or-literal). Byte-wise like the reference
    // for ASCII paths; non-ASCII variable names are undefined there.
    let mut result = String::new();
    for (index, part) in path.split('$').enumerate() {
        if part.is_empty() {
            if index == 0 {
                continue;
            }
            result.push('$');
            continue;
        }
        let end = part.find('/').unwrap_or(part.len());
        match std::env::var(&part[..end]) {
            Ok(value) => {
                result.push_str(&value);
                result.push_str(&part[end..]);
            }
            Err(_) => {
                result.push('$');
                result.push_str(part);
            }
        }
    }
    result
}

fn include_paths(config: &str) -> Vec<String> {
    config
        .lines()
        .filter_map(|line| line.trim().strip_prefix("include "))
        .map(expand_path_vars)
        .collect()
}

fn walk_included_configs(base: &str) -> String {
    let mut output = String::from("\n");
    let mut visited = HashSet::new();
    let mut queue: VecDeque<String> = include_paths(base).into();
    while let Some(pattern) = queue.pop_front() {
        let mut matches = expand_glob(&pattern);
        matches.reverse();
        for path in matches {
            if !visited.insert(path.clone()) {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            output.push_str(&contents);
            queue.extend(include_paths(&contents));
        }
    }
    output
}

fn expand_glob(pattern: &str) -> Vec<String> {
    if !pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        return vec![pattern.to_owned()];
    }
    let absolute = pattern.starts_with('/');
    let mut candidates = vec![if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    }];
    for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
        let wildcard = segment
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'['));
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
                    if wildcard_matches(segment.as_bytes(), name.to_string_lossy().as_bytes()) {
                        next.push(candidate.join(name));
                    }
                }
            } else {
                next.push(candidate.join(segment));
            }
        }
        candidates = next;
    }
    if candidates.is_empty() {
        vec![pattern.to_owned()]
    } else {
        candidates
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

fn wildcard_matches(pattern: &[u8], text: &[u8]) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first().copied() {
            None => text.is_empty(),
            Some(b'*') => {
                matches(&pattern[1..], text) || (!text.is_empty() && matches(pattern, &text[1..]))
            }
            Some(b'?') => !text.is_empty() && matches(&pattern[1..], &text[1..]),
            Some(b'[') => {
                let Some(end) = pattern.iter().position(|byte| *byte == b']') else {
                    return !text.is_empty()
                        && text[0] == b'['
                        && matches(&pattern[1..], &text[1..]);
                };
                if text.is_empty() {
                    return false;
                }
                let class = &pattern[1..end];
                let negate = matches!(class.first(), Some(b'!' | b'^'));
                let class = if negate { &class[1..] } else { class };
                let mut hit = false;
                let mut index = 0;
                while index < class.len() {
                    if index + 2 < class.len() && class[index + 1] == b'-' {
                        hit |= (class[index]..=class[index + 2]).contains(&text[0]);
                        index += 3;
                    } else {
                        hit |= class[index] == text[0];
                        index += 1;
                    }
                }
                hit != negate && matches(&pattern[end + 1..], &text[1..])
            }
            Some(byte) => !text.is_empty() && byte == text[0] && matches(&pattern[1..], &text[1..]),
        }
    }
    matches(pattern, text)
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

pub fn get_tree() -> Result<TreeReply, String> {
    TreeReply::from_json(&request(GET_TREE)?)
}

#[cfg(test)]
mod tests {
    use super::{
        config_from_reply, expand_path_vars, include_paths, version_from_reply, wildcard_matches,
        JsonParser, TreeReply,
    };

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
    fn config_reply_concatenates_all_i3_partials() {
        let json = JsonParser::new(
            br#"{"included_configs":[{"variable_replaced_contents":"one"},{"variable_replaced_contents":"two"}]}"#,
        )
        .parse()
        .unwrap();
        assert_eq!(config_from_reply(json, "i3").unwrap(), "onetwo");
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
    fn expansion_and_glob_matching_cover_vala_shapes() {
        std::env::set_var("ILIA_TEST_ROOT", "/tmp/example");
        std::env::set_var("ILIA_TEST_DOLLAR", "x");
        std::env::remove_var("ILIA_TEST_UNSET_QQ");
        assert_eq!(
            expand_path_vars("$ILIA_TEST_ROOT/config"),
            "/tmp/example/config"
        );
        // Vala-exact quirks (replicated reference bugs): `$$` collapses
        // to `$` while the name after it is still expanded; a `$` after
        // literal leading text corrupts that segment with a spurious
        // `$`; a trailing `$` re-parses the leading literal as a name.
        assert_eq!(expand_path_vars("$$ILIA_TEST_DOLLAR"), "$x");
        assert_eq!(
            expand_path_vars("$$ILIA_TEST_UNSET_QQ"),
            "$$ILIA_TEST_UNSET_QQ"
        );
        assert_eq!(expand_path_vars("/lit/$ILIA_TEST_DOLLAR/x"), "$/lit/x/x");
        assert_eq!(expand_path_vars("zzlit$"), "$zzlit$");
        assert_eq!(expand_path_vars("$ILIA_MISSING/file"), "$ILIA_MISSING/file");
        assert!(wildcard_matches(b"[ab]*.conf", b"alpha.conf"));
        assert!(!wildcard_matches(b"[ab]*.conf", b"zeta.conf"));
        assert_eq!(
            include_paths("  include $ILIA_TEST_ROOT/*.conf\n# include ignored\n"),
            vec!["/tmp/example/*.conf"]
        );
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
}
