//! JSON-RPC 2.0 messages and MCP's two eras (docs/mcp-mode.md §2; the 2026-07-28 revision's
//! versioning, discovery and stdio pages, and 2025-11-25's lifecycle). One message per line,
//! no batches. A request carrying the modern `_meta` is served statelessly by 2026-07-28; one
//! without it is legacy, served by the version `initialize` agreed, or refused when nothing
//! was agreed.

use serde_json::{Map, Value, json};

/// The modern revision sqlh serves.
pub(crate) const MODERN: &str = "2026-07-28";

/// The legacy revisions sqlh serves after `initialize`, newest first. 2025-03-26 isn't among
/// them: it obliges a server to take JSON-RPC batches, which sqlh doesn't.
pub(crate) const LEGACY: &[&str] = &["2025-11-25", "2025-06-18"];

pub(crate) const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub(crate) const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";
pub(crate) const META_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";
pub(crate) const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
/// 2026-07-28's `RequestMetaObject` declares it (deprecated, SEP-2577); sqlh sends no log
/// notifications, so it is only checked.
pub(crate) const META_LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";

/// The schema's `LoggingLevel`, the values a `_meta` log level takes.
const LOGGING_LEVELS: &[&str] = &[
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const UNSUPPORTED_VERSION: i64 = -32022;

/// How long a client may cache `server/discover` and `tools/list`: they depend only on the
/// launch, so an hour, and privately, since they name the local profiles.
pub(crate) const TTL_MS: u64 = 3_600_000;

/// One message from the client.
#[derive(Debug, PartialEq)]
pub(crate) enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Map<String, Value>,
    },
    Notification {
        method: String,
        params: Map<String, Value>,
    },
    /// A response to a request: sqlh sends none, so there is nothing to match it with.
    Response,
}

/// An error to answer with (a JSON-RPC `error`).
#[derive(Debug, PartialEq)]
pub(crate) struct ProtocolError {
    pub(crate) code: i64,
    pub(crate) message: String,
    pub(crate) data: Option<Value>,
}

impl ProtocolError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> ProtocolError {
        ProtocolError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(INVALID_PARAMS, message)
    }
}

/// The era a request is served in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Era {
    Modern,
    /// After `initialize`, with the version it agreed.
    Legacy(&'static str),
}

/// The longest id sqlh takes, as JSON text: a request id is held while the request is open
/// and echoed in every answer, so it is bounded on its own, far below the message size.
pub(crate) const MAX_ID_BYTES: usize = 1024;

/// What an id is known by, for every rule that compares ids (the calls in flight, the
/// requests held back, the open subscriptions and the cancels that name them): its JSON text
/// as serde_json writes it again. `parse` takes only strings and integers within 64 bits, a
/// number written another way (`1.0`, `1e2`, `-0`) as the integer it is (`exact_integer`), so two
/// ids share a key only when they are the same id: `1`, `1.0` and `1e0` one, `"1"` another.
/// A cancel's `requestId` is taken the same way; one that isn't such an id has a key no open
/// request has.
pub(crate) fn key(id: &Value) -> String {
    id.to_string()
}

/// Parses one line. An error comes with the request's id when it could be read.
///
/// What must not be answered isn't: a response (anything with `result` or `error` and no
/// `method`, whatever its id, since sqlh sends no requests), and a notification (a string
/// `method` and no `id`), whose `params` are taken as none when they aren't an object. A
/// request's `params` that is there must be an object (`null` is not one).
pub(crate) fn parse(line: &str) -> Result<Incoming, (Option<Value>, ProtocolError)> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| (None, ProtocolError::new(PARSE_ERROR, "Parse error")))?;
    let Value::Object(mut message) = value else {
        let what = if value.is_array() {
            "Invalid request: JSON-RPC batches aren't supported; send one message per line"
        } else {
            "Invalid request: a message is a JSON object"
        };
        return Err((None, ProtocolError::new(INVALID_REQUEST, what)));
    };
    if !message.contains_key("method")
        && (message.contains_key("result") || message.contains_key("error"))
    {
        return Ok(Incoming::Response);
    }
    // An id is a string or an integer, of at most MAX_ID_BYTES. One that is there but is
    // anything else (null included), or longer, can't be answered by, so the error leaves it
    // out. An integer is what it is however it is written: serde_json reads `1` as an i64 but
    // `1.0`, `1e2` and `-0` as floats, and past 64 bits a whole number too, so those are
    // decided from the number as the line writes it (`exact_integer`, R40), never from the float,
    // which would round a fraction or a larger number onto an id (R38).
    let id = match message.remove("id") {
        None => None,
        Some(id) if id.is_string() || id.is_i64() || id.is_u64() => Some(id),
        Some(other) => match (&other, raw_member(line, &["id"]).and_then(exact_integer)) {
            (Value::Number(_), Some(exact)) => Some(exact),
            _ => {
                return Err((
                    None,
                    ProtocolError::new(
                        INVALID_REQUEST,
                        "Invalid request: id must be a string or an integer within 64 bits",
                    ),
                ));
            }
        },
    };
    let id = match id {
        None => None,
        Some(id) if key(&id).len() > MAX_ID_BYTES => {
            return Err((
                None,
                ProtocolError::new(
                    INVALID_REQUEST,
                    format!("Invalid request: an id is at most {MAX_ID_BYTES} bytes as JSON"),
                ),
            ));
        }
        Some(id) => Some(id),
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((
            id,
            ProtocolError::new(INVALID_REQUEST, "Invalid request: jsonrpc must be \"2.0\""),
        ));
    }
    let Some(method) = message.get("method") else {
        // Not a request, a notification or (above) a response.
        return Err((
            id,
            ProtocolError::new(INVALID_REQUEST, "Invalid request: a message needs a method"),
        ));
    };
    let Some(method) = method.as_str().map(str::to_owned) else {
        return Err((
            id,
            ProtocolError::new(INVALID_REQUEST, "Invalid request: method must be a string"),
        ));
    };
    // Only `params` left out is none: present, JSON-RPC 2.0 §4.2 has it a structured value, and
    // MCP an object, so a request's `null` is refused as any other non-object is.
    let mut params = match message.remove("params") {
        None => Map::new(),
        Some(Value::Object(params)) => params,
        // A notification is never answered: what it can't use, `null` included, it goes
        // without.
        Some(_) if id.is_none() => Map::new(),
        Some(_) => {
            return Err((
                id,
                ProtocolError::invalid_params("Invalid params: params must be an object"),
            ));
        }
    };
    // A cancel names its request by the same rule: `1e2` names the request whose id is 100.
    // A number that is no integer id names none, so the cancel is ignored, as an invalid one
    // should be.
    if id.is_none()
        && method == "notifications/cancelled"
        && let Some(Value::Number(n)) = params.get("requestId")
        && !(n.is_i64() || n.is_u64())
    {
        match raw_member(line, &["params", "requestId"]).and_then(exact_integer) {
            Some(exact) => params.insert("requestId".into(), exact),
            None => params.remove("requestId"),
        };
    }
    // A tool's numeric arguments (`max_rows`, `max_cell`, `timeout`) are integers, decided by
    // the same rule (R41): one serde_json read as a float becomes the integer its text is
    // (`30.0`, `3e1`), or stays a float, which the tool refuses as no whole number
    // (`10.0000000000000001`, which the float would make 10; `-1e-9999`, which it would make
    // 0; `1e30`).
    if method == "tools/call"
        && let Some(Value::Object(arguments)) = params.get_mut("arguments")
    {
        let floats: Vec<String> = arguments
            .iter()
            .filter(|(_, value)| matches!(value, Value::Number(n) if !(n.is_i64() || n.is_u64())))
            .map(|(name, _)| name.clone())
            .collect();
        for name in floats {
            if let Some(exact) =
                raw_member(line, &["params", "arguments", &name]).and_then(exact_integer)
            {
                arguments.insert(name, exact);
            }
        }
    }
    Ok(match id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    })
}

/// The integer a JSON number is, from its text (`number`, as the line writes it), when it is
/// one within i64 or u64, for an id (R40) and a tool's numeric argument (R41): `1.0` and `1e0` are 1, `1e2` is 100, `-0` is 0. Not a fraction
/// (`1.5`, `1.0000000000000001`, `5e-3`), nor a larger magnitude (`1e20`, `18446744073709551616`),
/// which sqlh refuses as an implementation limit (MCP asks only for an integer). Exact: the
/// digits are shifted by the exponent as text, never through a float, so nothing rounds.
pub(crate) fn exact_integer(number: &str) -> Option<Value> {
    let (negative, unsigned) = match number.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, number),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        // Zero, however written (`-0`, `0.0`, `0e9`).
        return Some(json!(0));
    }
    // A huge exponent can't make a nonzero integer within 64 bits: past i64, refuse.
    let exponent: i64 = match exponent {
        None => 0,
        Some(e) => e.strip_prefix('+').unwrap_or(e).parse().ok()?,
    };
    let fraction_digits = i64::try_from(fraction.len()).ok()?;
    let scale = exponent.checked_sub(fraction_digits)?;
    let integer = if scale >= 0 {
        // u64::MAX has 20 digits.
        let zeros = usize::try_from(scale).ok()?;
        if significant.len().checked_add(zeros)? > 20 {
            return None;
        }
        format!("{significant}{}", "0".repeat(zeros))
    } else {
        let cut = usize::try_from(scale.checked_neg()?).ok()?;
        let at = significant.len().checked_sub(cut)?;
        if at == 0 || !significant[at..].bytes().all(|b| b == b'0') {
            return None;
        }
        significant[..at].to_owned()
    };
    if integer.len() > 20 {
        return None;
    }
    let magnitude: u128 = integer.parse().ok()?;
    if negative {
        let value = i128::try_from(magnitude).ok()?.checked_neg()?;
        i64::try_from(value).ok().map(Value::from)
    } else {
        u64::try_from(magnitude).ok().map(Value::from)
    }
}

/// The text of the value at `path` (member names, from the top object down), as the line
/// writes it: the last member of that name, as serde_json keeps the last of a duplicate. For
/// a line serde_json has already read whole, so it only needs to find its way, decoding the
/// names it compares (`"\u0069d"` is `id`).
fn raw_member<'a>(line: &'a str, path: &[&str]) -> Option<&'a str> {
    let mut scan = Scan {
        bytes: line.as_bytes(),
        at: 0,
    };
    let (start, end) = scan.find(path)?;
    line.get(start..end)
}

/// A reader of a JSON text already known to be valid (`raw_member`).
struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    /// The span of the value at `path` in the object that starts here.
    fn find(&mut self, path: &[&str]) -> Option<(usize, usize)> {
        let (name, rest) = path.split_first()?;
        self.space();
        if self.bytes.get(self.at) != Some(&b'{') {
            return None;
        }
        self.at += 1;
        let mut found = None;
        loop {
            self.space();
            match self.bytes.get(self.at)? {
                b'}' => break,
                b',' => {
                    self.at += 1;
                    continue;
                }
                _ => {}
            }
            let key = self.string()?;
            self.space();
            if self.bytes.get(self.at) != Some(&b':') {
                return None;
            }
            self.at += 1;
            self.space();
            let start = self.at;
            if key == *name {
                found = if rest.is_empty() {
                    self.value()?;
                    Some((start, self.at))
                } else {
                    // Found in the last one of the name, as serde_json keeps it.
                    let inner = self.find(rest);
                    self.at = start;
                    self.value()?;
                    inner
                };
            } else {
                self.value()?;
            }
        }
        found
    }

    /// A string, decoded; `at` is left past its closing quote.
    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        self.at += 1;
        let mut units: Vec<u16> = Vec::new();
        loop {
            let b = *self.bytes.get(self.at)?;
            self.at += 1;
            match b {
                b'"' => return Some(String::from_utf16_lossy(&units)),
                b'\\' => {
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    let unit = match escape {
                        b'"' => u16::from(b'"'),
                        b'\\' => u16::from(b'\\'),
                        b'/' => u16::from(b'/'),
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => u16::from(b'\n'),
                        b'r' => u16::from(b'\r'),
                        b't' => u16::from(b'\t'),
                        b'u' => {
                            let hex =
                                std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
                            self.at += 4;
                            u16::from_str_radix(hex, 16).ok()?
                        }
                        _ => return None,
                    };
                    units.push(unit);
                }
                _ => {
                    // The rest of a UTF-8 character, kept as it is.
                    let start = self.at - 1;
                    let mut end = self.at;
                    while self
                        .bytes
                        .get(end)
                        .is_some_and(|b| !matches!(b, b'"' | b'\\'))
                    {
                        end += 1;
                    }
                    let text = std::str::from_utf8(self.bytes.get(start..end)?).ok()?;
                    units.extend(text.encode_utf16());
                    self.at = end;
                }
            }
        }
    }

    /// Steps over one value of any kind.
    fn value(&mut self) -> Option<()> {
        match *self.bytes.get(self.at)? {
            b'"' => self.string().map(|_| ()),
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                self.at += 1;
                loop {
                    self.space();
                    match *self.bytes.get(self.at)? {
                        b if b == close => {
                            self.at += 1;
                            return Some(());
                        }
                        b',' | b':' => self.at += 1,
                        _ => self.value()?,
                    }
                }
            }
            _ => {
                // A number, true, false or null.
                let start = self.at;
                while self
                    .bytes
                    .get(self.at)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.'))
                {
                    self.at += 1;
                }
                (self.at > start).then_some(())
            }
        }
    }
}

/// The era to serve a request in: modern when it carries the modern `_meta` (checked: the
/// version must be ours, and the client's capabilities are required), else the legacy version
/// `initialize` agreed, if any. A `_meta` that isn't an object, or whose declared members are
/// mistyped, is `-32602` in either era (`check_meta`).
pub(crate) fn era_of(
    params: &Map<String, Value>,
    agreed: Option<&'static str>,
) -> Result<Era, ProtocolError> {
    let meta = check_meta(params)?;
    match meta.and_then(|meta| meta.get(META_VERSION)) {
        Some(Value::String(version)) if version == MODERN => {
            if meta
                .and_then(|meta| meta.get(META_CAPABILITIES))
                .is_some_and(Value::is_object)
            {
                Ok(Era::Modern)
            } else {
                Err(ProtocolError::invalid_params(format!(
                    "Invalid params: _meta needs {META_CAPABILITIES}"
                )))
            }
        }
        Some(Value::String(version)) => Err(unsupported(version)),
        Some(_) => Err(ProtocolError::invalid_params(format!(
            "Invalid params: _meta {META_VERSION} must be a string"
        ))),
        None => match agreed {
            Some(version) => Ok(Era::Legacy(version)),
            None => Err(ProtocolError::invalid_params(format!(
                "Invalid params: _meta needs {META_VERSION} (or start with initialize)"
            ))),
        },
    }
}

/// A request's `_meta`, when it has one, with the members both eras declare typed (R39): an
/// object (2026-07-28's `RequestMetaObject`, the legacy revisions' optional `_meta`), whose
/// `progressToken` is a string or a number, whose modern client info is an `Implementation`
/// and whose log level is a `LoggingLevel`. The version and the capabilities are `era_of`'s;
/// any other member, an extension's, is let be.
pub(crate) fn check_meta(
    params: &Map<String, Value>,
) -> Result<Option<&Map<String, Value>>, ProtocolError> {
    let meta = match params.get("_meta") {
        None => return Ok(None),
        Some(Value::Object(meta)) => meta,
        Some(_) => return Err(must_be("", "_meta", "an object")),
    };
    member(meta, "_meta ", "progressToken", false, Kind::Token)?;
    member(
        meta,
        "_meta ",
        META_CLIENT_INFO,
        false,
        Kind::Implementation,
    )?;
    member(meta, "_meta ", META_LOG_LEVEL, false, Kind::LogLevel)?;
    Ok(Some(meta))
}

/// What a declared member of a request's params must be (`member`).
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Object,
    String,
    Bool,
    /// `string[]`.
    Strings,
    /// The schema's `Implementation` (a client's info): an object whose `name` and `version`
    /// are strings.
    Implementation,
    /// A `ProgressToken`: a string or a number.
    Token,
    /// A `LoggingLevel`: one of its eight strings.
    LogLevel,
}

impl Kind {
    fn takes(self, value: &Value) -> bool {
        match self {
            Kind::Object => value.is_object(),
            Kind::String => value.is_string(),
            Kind::Bool => value.is_boolean(),
            Kind::Strings => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            Kind::Implementation => {
                value.get("name").is_some_and(Value::is_string)
                    && value.get("version").is_some_and(Value::is_string)
            }
            Kind::Token => value.is_string() || value.is_number(),
            Kind::LogLevel => value
                .as_str()
                .is_some_and(|level| LOGGING_LEVELS.contains(&level)),
        }
    }

    fn what(self) -> &'static str {
        match self {
            Kind::Object => "an object",
            Kind::String => "a string",
            Kind::Bool => "true or false",
            Kind::Strings => "an array of strings",
            Kind::Implementation => "an object with name and version strings",
            Kind::Token => "a string or a number",
            Kind::LogLevel => {
                "one of debug, info, notice, warning, error, critical, alert, emergency"
            }
        }
    }
}

/// Checks one declared member of a request's params (`within` says where it is, for the
/// message): left out, it is refused only when `required`; present, it must be of its `kind`,
/// or the request is `-32602`, `Invalid params: <within><name> must be <what the kind is>`.
/// Members the schema doesn't declare are never looked at, so an extension's pass.
pub(crate) fn member(
    params: &Map<String, Value>,
    within: &str,
    name: &str,
    required: bool,
    kind: Kind,
) -> Result<(), ProtocolError> {
    match params.get(name) {
        None if !required => Ok(()),
        Some(value) if kind.takes(value) => Ok(()),
        _ => Err(must_be(within, name, kind.what())),
    }
}

fn must_be(within: &str, name: &str, what: &str) -> ProtocolError {
    ProtocolError::invalid_params(format!("Invalid params: {within}{name} must be {what}"))
}

/// `UnsupportedProtocolVersionError`: what sqlh supports, and what was asked for.
fn unsupported(requested: &str) -> ProtocolError {
    let mut supported = vec![MODERN];
    supported.extend_from_slice(LEGACY);
    ProtocolError {
        code: UNSUPPORTED_VERSION,
        message: "Unsupported protocol version".into(),
        data: Some(json!({"supported": supported, "requested": requested})),
    }
}

/// The legacy version to agree to for `initialize`: the client's when sqlh serves it, else
/// sqlh's newest (the client then decides whether to go on).
pub(crate) fn negotiate(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|requested| LEGACY.iter().find(|v| **v == requested))
        .copied()
        .unwrap_or(LEGACY[0])
}

/// The server's name and version, as results carry it.
pub(crate) fn server_info() -> Value {
    json!({"name": "sqlh", "version": crate::app::VERSION})
}

/// The versions `server/discover` lists.
pub(crate) fn supported_versions() -> Vec<&'static str> {
    let mut versions = vec![MODERN];
    versions.extend_from_slice(LEGACY);
    versions
}

/// A result line for request `id`. A modern result carries `resultType` and the server's
/// info in `_meta`, which every 2026-07-28 result must or should.
pub(crate) fn result_line(id: &Value, era: Era, mut result: Map<String, Value>) -> String {
    if era == Era::Modern {
        result.insert("resultType".into(), json!("complete"));
        let meta = result
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(meta) = meta {
            meta.insert(META_SERVER.into(), server_info());
        }
    }
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// A notification line (a message with no id, which gets no answer).
pub(crate) fn notification_line(method: &str, params: Map<String, Value>) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// An error line, for request `id` when it is known (the MCP schema leaves `id` out when it
/// couldn't be read).
pub(crate) fn error_line(id: Option<&Value>, error: &ProtocolError) -> String {
    let mut body = Map::new();
    body.insert("code".into(), json!(error.code));
    body.insert("message".into(), json!(error.message));
    if let Some(data) = &error.data {
        body.insert("data".into(), data.clone());
    }
    let mut line = Map::new();
    line.insert("jsonrpc".into(), json!("2.0"));
    if let Some(id) = id {
        line.insert("id".into(), id.clone());
    }
    line.insert("error".into(), Value::Object(body));
    Value::Object(line).to_string()
}

/// A JSON object from a `json!` literal that is one.
pub(crate) fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    // R40: a number is the integer it is, however it is written, decided from its text so
    // that nothing rounds (R38); a fraction or a magnitude past 64 bits is no id.
    #[test]
    fn an_integral_number_is_its_integer_however_it_is_written() {
        for (text, wanted) in [
            ("1", Some(json!(1))),
            ("1.0", Some(json!(1))),
            ("1e0", Some(json!(1))),
            ("1E+0", Some(json!(1))),
            ("10e-1", Some(json!(1))),
            ("1e2", Some(json!(100))),
            ("100e-2", Some(json!(1))),
            ("-0", Some(json!(0))),
            ("-0.0", Some(json!(0))),
            ("0e999999999999999999999", Some(json!(0))),
            ("1e19", Some(json!(10_000_000_000_000_000_000u64))),
            ("18446744073709551615.0", Some(json!(u64::MAX))),
            ("1.8446744073709551615e19", Some(json!(u64::MAX))),
            ("-9223372036854775808.0", Some(json!(i64::MIN))),
            (
                "-9.223372036854775807e18",
                Some(json!(-9_223_372_036_854_775_807i64)),
            ),
            ("1.5", None),
            ("1.0000000000000001", None),
            ("5e-3", None),
            ("1e20", None),
            ("18446744073709551616", None),
            ("18446744073709551616.0", None),
            ("-9223372036854775809", None),
            ("1e999999999999999999999", None),
            ("1e-999999999999999999999", None),
        ] {
            assert_eq!(exact_integer(text), wanted, "{text}");
        }
    }

    // R40: the text of a member, found as serde_json would: names decoded, the last of a
    // duplicate, strings and nesting stepped over.
    // R41: a tool call's numeric arguments are read as R40 reads ids: an integer written
    // another way is that integer, and anything else stays a float the tool refuses.
    #[test]
    fn a_tool_calls_numeric_arguments_are_integers_from_their_text() {
        let arguments = |raw: &str| {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"query","arguments":{raw}}}}}"#
            );
            match parse(&line) {
                Ok(Incoming::Request { params, .. }) => params["arguments"].clone(),
                other => panic!("{raw}: {other:?}"),
            }
        };
        let read = arguments(r#"{"sql":"x","max_rows":30.0,"max_cell":3e1,"timeout":-0}"#);
        assert_eq!(read["max_rows"], json!(30));
        assert_eq!(read["max_cell"], json!(30));
        assert_eq!(read["timeout"], json!(0));
        assert!(read["max_rows"].is_u64() && read["max_cell"].is_u64());
        let read =
            arguments(r#"{"max_rows":10.0000000000000001,"max_cell":-1e-9999,"timeout":1e30}"#);
        for name in ["max_rows", "max_cell", "timeout"] {
            assert!(read[name].is_f64(), "{name}: {}", read[name]);
        }
        // Only a tool call's arguments are read so; a ping's params are left as they are.
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"x":1.0}}"#;
        match parse(line) {
            Ok(Incoming::Request { params, .. }) => assert!(params["x"].is_f64()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_members_text_is_found_as_the_line_writes_it() {
        let line =
            r#"{"params":{"x":"}{\"id\":7","requestId":1e2},"id":1.0,"id":[1,{"id":2}],"id":2.0}"#;
        assert_eq!(raw_member(line, &["id"]), Some("2.0"));
        assert_eq!(raw_member(line, &["params", "requestId"]), Some("1e2"));
        assert_eq!(raw_member(line, &["params", "id"]), None);
        // An escaped name is the name it decodes to, and the last of the name still wins.
        // (The escape is built from its backslash, U+005C, so the source shows it plainly.)
        let escaped = format!(
            r#"{{"id":1.5,"{}u0069d":2.0,"i\"d":3.0}}"#,
            char::from(0x5c)
        );
        assert!(escaped.contains(r#",""#) && !escaped.contains(r#""id":2.0"#));
        assert_eq!(raw_member(&escaped, &["id"]), Some("2.0"));
        assert_eq!(raw_member(r#"{"id": -0 }"#, &["id"]), Some("-0"));
        assert_eq!(raw_member(r#"{"é":"ü","id":1E2}"#, &["id"]), Some("1E2"));
        assert_eq!(raw_member("[1]", &["id"]), None);
    }

    use super::*;

    fn modern(extra: Value) -> Map<String, Value> {
        let mut params = object(json!({
            "_meta": {META_VERSION: MODERN, META_CAPABILITIES: {}}
        }));
        if let Value::Object(extra) = extra {
            params.extend(extra);
        }
        params
    }

    // One message per line: requests, notifications and responses; the errors sqlh answers
    // with when a line isn't one, with the id when it could be read.
    #[test]
    fn lines_are_requests_notifications_or_errors() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"a":1}}"#),
            Ok(Incoming::Request {
                id: json!(7),
                method: "tools/list".into(),
                params: object(json!({"a": 1})),
            })
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Ok(Incoming::Notification {
                method: "notifications/initialized".into(),
                params: Map::new(),
            })
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"x","result":{}}"#),
            Ok(Incoming::Response)
        );
        let code = |line: &str| parse(line).err().map(|(id, e)| (id, e.code));
        assert_eq!(code("{nope"), Some((None, PARSE_ERROR)));
        assert_eq!(code("[]"), Some((None, INVALID_REQUEST)));
        assert_eq!(code("3"), Some((None, INVALID_REQUEST)));
        assert_eq!(
            code(r#"{"id":1,"method":"ping"}"#),
            Some((Some(json!(1)), INVALID_REQUEST))
        );
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":"a","method":5}"#),
            Some((Some(json!("a")), INVALID_REQUEST))
        );
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":[1]}"#),
            Some((Some(json!(2)), INVALID_PARAMS))
        );
        // Only params left out is none: a request's null is no object either, and is refused
        // with its id; a notification's goes without, unanswered.
        for method in ["ping", "tools/list", "tools/call", "server/discover"] {
            let line = format!(r#"{{"jsonrpc":"2.0","id":3,"method":"{method}","params":null}}"#);
            let (id, error) = parse(&line).unwrap_err();
            assert_eq!(id, Some(json!(3)), "{method}");
            assert_eq!(
                (error.code, error.message.as_str()),
                (INVALID_PARAMS, "Invalid params: params must be an object"),
                "{method}"
            );
        }
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":null}"#),
            Ok(Incoming::Notification {
                method: "notifications/cancelled".into(),
                params: Map::new(),
            })
        );
        // An id that is neither a string nor a number can't be answered by it.
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#),
            Some((None, INVALID_REQUEST))
        );
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#),
            Some((None, INVALID_REQUEST))
        );
        // Neither a request nor a response.
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":4}"#),
            Some((Some(json!(4)), INVALID_REQUEST))
        );
        // Never answered: a response, whatever its id, and a notification, whatever its params.
        for line in [
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":[1],"result":{}}"#,
            r#"{"error":{"code":1,"message":"x"}}"#,
        ] {
            assert_eq!(parse(line), Ok(Incoming::Response), "{line}");
        }
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":[3]}"#),
            Ok(Incoming::Notification {
                method: "notifications/cancelled".into(),
                params: Map::new(),
            })
        );
        // An id is held and echoed, so it has a bound of its own.
        let long = format!(
            r#"{{"jsonrpc":"2.0","id":"{}","method":"ping"}}"#,
            "x".repeat(MAX_ID_BYTES)
        );
        assert_eq!(code(&long), Some((None, INVALID_REQUEST)));
        let fits = format!(
            r#"{{"jsonrpc":"2.0","id":"{}","method":"ping"}}"#,
            "x".repeat(MAX_ID_BYTES - 2)
        );
        assert!(parse(&fits).is_ok());
        // R38: an id is a string or an integer serde_json reads exactly, an i64 or a u64, and
        // it is known by its text as written; any other number is refused without it, so two
        // ids never become one.
        let ping = |id: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#);
        for id in [
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "0",
            r#""1""#,
        ] {
            match parse(&ping(id)) {
                Ok(Incoming::Request { id: read, .. }) => assert_eq!(key(&read), id),
                other => panic!("{id}: {other:?}"),
            }
        }
        // R40: an integer written another way is that integer.
        for (written, id) in [
            ("1e2", json!(100)),
            ("1.0", json!(1)),
            ("-0", json!(0)),
            ("18446744073709551615.0", json!(u64::MAX)),
        ] {
            match parse(&ping(written)) {
                Ok(Incoming::Request { id: read, .. }) => assert_eq!(read, id, "{written}"),
                other => panic!("{written}: {other:?}"),
            }
        }
        for id in [
            "18446744073709551616",
            "18446744073709551617",
            "-9223372036854775809",
            "1.5",
            "1.0000000000000001",
            "1e20",
        ] {
            let refused = parse(&ping(id)).unwrap_err();
            assert_eq!(refused.0, None, "{id}");
            assert_eq!(
                refused.1.message,
                "Invalid request: id must be a string or an integer within 64 bits",
                "{id}"
            );
        }
        assert_ne!(key(&json!(1)), key(&json!("1")));
    }

    // docs/mcp-mode.md §2: modern requests carry the version and the client's capabilities;
    // a wrong version is -32022 with what sqlh supports; no _meta is legacy after initialize
    // and -32602 before it.
    #[test]
    fn the_era_follows_the_request_and_initialize() {
        assert_eq!(era_of(&modern(json!({})), None), Ok(Era::Modern));
        assert_eq!(
            era_of(&modern(json!({})), Some("2025-11-25")),
            Ok(Era::Modern)
        );
        assert_eq!(
            era_of(&Map::new(), Some("2025-06-18")),
            Ok(Era::Legacy("2025-06-18"))
        );
        let no_meta = era_of(&Map::new(), None).unwrap_err();
        assert_eq!(no_meta.code, INVALID_PARAMS);
        let no_capabilities = object(json!({"_meta": {META_VERSION: MODERN}}));
        assert_eq!(
            era_of(&no_capabilities, None).unwrap_err().code,
            INVALID_PARAMS
        );
        let old = object(json!({"_meta": {META_VERSION: "1900-01-01", META_CAPABILITIES: {}}}));
        let error = era_of(&old, None).unwrap_err();
        assert_eq!(error.code, UNSUPPORTED_VERSION);
        assert_eq!(
            error.data,
            Some(json!({
                "supported": ["2026-07-28", "2025-11-25", "2025-06-18"],
                "requested": "1900-01-01"
            }))
        );
        // R39: a `_meta` that isn't an object, or whose declared members are mistyped, is
        // -32602 in either era; an extension's members pass.
        let legacy = Some("2025-11-25");
        for meta in [
            json!(5),
            json!(null),
            json!({"progressToken": {}}),
            json!({META_CLIENT_INFO: {"name": "x"}}),
            json!({META_LOG_LEVEL: 5}),
            json!({META_LOG_LEVEL: null}),
            json!({META_LOG_LEVEL: "verbose"}),
            json!({META_LOG_LEVEL: "INFO"}),
        ] {
            let error = era_of(&object(json!({"_meta": meta})), legacy).unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS, "{meta}");
        }
        // The declared log level is a `LoggingLevel`, and the refusal names the eight.
        let meta = json!({META_VERSION: MODERN, META_CAPABILITIES: {}, META_LOG_LEVEL: "verbose"});
        let error = era_of(&object(json!({"_meta": meta})), None);
        let levels = LOGGING_LEVELS.join(", ");
        assert_eq!(
            error.unwrap_err().message,
            format!("Invalid params: _meta {META_LOG_LEVEL} must be one of {levels}")
        );
        for level in LOGGING_LEVELS {
            let meta = json!({META_VERSION: MODERN, META_CAPABILITIES: {}, META_LOG_LEVEL: level});
            assert_eq!(
                era_of(&object(json!({"_meta": meta})), None),
                Ok(Era::Modern),
                "{level}"
            );
        }
        let extended = object(json!({"_meta": {
            "progressToken": 7,
            META_CLIENT_INFO: {"name": "x", "version": "1", "title": "X"},
            META_LOG_LEVEL: "info",
            "com.example/x": [1],
        }}));
        assert_eq!(era_of(&extended, legacy), Ok(Era::Legacy("2025-11-25")));
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate(Some("2025-03-26")), "2025-11-25");
        assert_eq!(negotiate(None), "2025-11-25");
    }

    #[test]
    fn modern_results_carry_their_type_and_the_server() {
        let line = result_line(&json!(3), Era::Modern, object(json!({"tools": []})));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 3);
        assert_eq!(value["result"]["resultType"], "complete");
        assert_eq!(value["result"]["_meta"][META_SERVER]["name"], "sqlh");
        let legacy = result_line(&json!("x"), Era::Legacy("2025-11-25"), Map::new());
        assert_eq!(legacy, r#"{"id":"x","jsonrpc":"2.0","result":{}}"#);
        let error = error_line(None, &ProtocolError::new(PARSE_ERROR, "Parse error"));
        assert_eq!(
            error,
            r#"{"error":{"code":-32700,"message":"Parse error"},"jsonrpc":"2.0"}"#
        );
        assert!(!line.contains('\n'), "one line");
    }
}
