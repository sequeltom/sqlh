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

/// Parses one line. An error comes with the request's id when it could be read.
///
/// What must not be answered isn't: a response (anything with `result` or `error` and no
/// `method`, whatever its id, since sqlh sends no requests), and a notification (a string
/// `method` and no `id`), whose `params` are taken as none when they aren't an object.
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
    // An id is a string or a number, of at most MAX_ID_BYTES. One that is there but is
    // anything else (null included), or longer, can't be answered by, so the error leaves it
    // out.
    let id = match message.remove("id") {
        None => None,
        Some(id) if !(id.is_string() || id.is_number()) => {
            return Err((
                None,
                ProtocolError::new(
                    INVALID_REQUEST,
                    "Invalid request: id must be a string or a number",
                ),
            ));
        }
        Some(id) if id.to_string().len() > MAX_ID_BYTES => {
            return Err((
                None,
                ProtocolError::new(
                    INVALID_REQUEST,
                    format!("Invalid request: an id is at most {MAX_ID_BYTES} bytes"),
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
    let params = match message.remove("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(params)) => params,
        // A notification is never answered: what it can't use, it goes without.
        Some(_) if id.is_none() => Map::new(),
        Some(_) => {
            return Err((
                id,
                ProtocolError::invalid_params("Invalid params: params must be an object"),
            ));
        }
    };
    Ok(match id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    })
}

/// The era to serve a request in: modern when it carries the modern `_meta` (checked: the
/// version must be ours, and the client's capabilities are required), else the legacy version
/// `initialize` agreed, if any.
pub(crate) fn era_of(
    params: &Map<String, Value>,
    agreed: Option<&'static str>,
) -> Result<Era, ProtocolError> {
    let meta = params.get("_meta").and_then(Value::as_object);
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
