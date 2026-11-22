//! The tools (docs/mcp-mode.md §4): their definitions, the checks on a call's arguments, and a
//! call run through the engine into an output held in memory. Every tool is a read (owner
//! decision 16): the user's SQL goes through the guard and the rolled-back wrapper, and the
//! schema commands are sqlh's own SQL, read-only too.

use std::time::Instant;

use serde_json::{Map, Value, json};

use super::protocol::{Kind, ProtocolError, member};
use crate::cli::SchemaCommand;
use crate::db::Cancel;
use crate::engine::{Call, Engine};
use crate::error::{Exit, Failure};
use crate::output::{Advice, Collected, Format, Limits};
use crate::run::{self, Input, QueryOrigin, Report, Request, RunOutcome};

/// The most rows a call may ask to see per result set.
const MAX_ROWS_ARG: u64 = 10_000;

/// The most characters per value a call may ask for (0, all of a value, is bounded by the
/// output's size anyway).
const MAX_CELL_ARG: u64 = 1_000_000;

/// The longest SQL a call takes: 1 MB, as text.
const MAX_SQL_BYTES: usize = 1 << 20;

/// What the server knows at launch that the tools need: the profiles (in launch order, as
/// the engine numbers them) with their descriptions, and which limits the flags set, which a
/// schema command's own defaults then leave alone (schema-commands.md §4).
pub(crate) struct Launch {
    pub(crate) profiles: Vec<Profile>,
    /// `--max-rows` and `--max-cell` at launch, if given: they are every call's default then.
    pub(crate) max_rows: Option<u64>,
    pub(crate) max_cell: Option<usize>,
}

pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
}

/// A tool's arguments, by kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    Sql,
    Object,
    Pattern,
    OptionalPattern,
    InDefinitions,
    ExactCount,
}

struct Tool {
    name: &'static str,
    description: &'static str,
    args: &'static [Arg],
    /// Whether `max_rows` means anything for it: `info` returns one row. `max_cell` always
    /// does, since a profile's or the launch's small limit cuts `info`'s values too (R49).
    rows: bool,
}

/// The tools, in `tools/list` order.
const TOOLS: &[Tool] = &[
    Tool {
        name: "query",
        description: "Run read-only T-SQL on SQL Server. A read-only guard refuses writes, and \
            everything runs in a transaction that is rolled back. Several batches separated by \
            GO are allowed; temp tables last only within one call. The result is sqlh's compact \
            text: `-- ` lines are metadata, the header is name:type, rows are tab-separated, \
            NULL is NULL, and the last line is the status: `-- ok:`, `-- partial:` or \
            `-- failed`. Values are escaped: `\\\\` is one backslash, `\\t` a tab, `\\n` a \
            newline, and `…[+N]` ends a value cut N characters short. Limits cut long results \
            and say so; raise max_rows only when you need more. `≥N` rows is a lower bound: \
            the hidden rows were counted only briefly (exact_count counts them all). A capped \
            result with more statements after it in its batch is counted for up to 5 s so they \
            still run; `-- partial:` means some didn't, so put each query in its own GO batch. \
            format json gives the same as one JSON document.",
        args: &[Arg::Sql, Arg::ExactCount],
        rows: true,
    },
    Tool {
        name: "check",
        description: "Compile T-SQL without running it (SET NOEXEC ON): syntax and name \
            binding errors only. A missing table is reported only when a query runs, because \
            SQL Server defers its name resolution.",
        args: &[Arg::Sql],
        rows: true,
    },
    Tool {
        name: "info",
        description: "The server, its version and edition, the database, the login and \
            user, the transport, the auth scheme, the server's time, and server_permissions: \
            sysadmin, or the server-wide permissions this login holds that decide which server \
            reads work (VIEW SERVER STATE, VIEW ANY ERROR LOG and the like), or none.",
        args: &[],
        rows: false,
    },
    Tool {
        name: "databases",
        description: "The databases on the server, with their state, whether this login \
            can use them, compatibility level, recovery model, read-only flag and collation.",
        args: &[],
        rows: true,
    },
    Tool {
        name: "tables",
        description: "Tables and views with their estimated rows. pattern is a substring, \
            or a glob with * and ? (case-insensitive), matched against the name or \
            schema.name.",
        args: &[Arg::OptionalPattern],
        rows: true,
    },
    Tool {
        name: "describe",
        description: "Everything needed to query a table or view, or call a procedure or \
            function: columns and types, keys, indexes, foreign keys both ways, checks, \
            triggers, parameters. Works for system views too (sys.dm_exec_requests). Use it \
            before writing queries against unfamiliar tables.",
        args: &[Arg::Object],
        rows: true,
    },
    Tool {
        name: "find",
        description: "Objects and columns whose names match a pattern (a substring, or a glob \
            with * and ?); with in_definitions, also the module bodies that contain it, with \
            the first matching line.",
        args: &[Arg::Pattern, Arg::InDefinitions],
        rows: true,
    },
    Tool {
        name: "definition",
        description: "The source of a view, procedure, function or trigger, as a block framed \
            by its line count.",
        args: &[Arg::Object],
        rows: true,
    },
    Tool {
        name: "deps",
        description: "What an object uses and what uses it, one level each way: code \
            references, foreign keys and synonyms, with the column or constraint that holds \
            each reference.",
        args: &[Arg::Object],
        rows: true,
    },
];

/// The `max_rows` and `max_cell` descriptions for `tool`, each with the call's default: the
/// launch's `--max-rows` or `--max-cell` if given, else the schema command's own
/// (`schema::Command::limits`), else the profile's or sqlh's.
fn limit_descriptions(tool: &str, launch: &Launch) -> (String, String) {
    let rows = match (launch.max_rows, tool) {
        (Some(n), _) => format!("default {n}"),
        (None, "describe" | "deps" | "definition") => "default 2000".into(),
        (None, _) => "default 100, or the profile's max_rows".into(),
    };
    let cell = match (launch.max_cell, tool) {
        (Some(0), _) | (None, "definition") => "default: no limit".into(),
        (Some(n), _) => format!("default {n}"),
        (None, "describe" | "deps") => "default 4000".into(),
        (None, _) => "default 500, or the profile's max_cell".into(),
    };
    (
        format!("Rows shown per result set ({rows}); 0 shows all (within the size limit)."),
        format!("Characters shown per value ({cell}); 0 shows all (within the size limit)."),
    )
}

/// The `tools/list` array, for these profiles.
pub(crate) fn definitions(launch: &Launch) -> Value {
    let names: Vec<&str> = launch.profiles.iter().map(|p| p.name.as_str()).collect();
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|tool| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for arg in tool.args {
                let (name, schema, needed) = match arg {
                    Arg::Sql => (
                        "sql",
                        json!({"type": "string", "minLength": 1, "description":
                        if tool.name == "check" {
                            "The T-SQL to compile (not run)."
                        } else {
                            "The T-SQL to run."
                        }}),
                        true,
                    ),
                    Arg::ExactCount => (
                        "exact_count",
                        json!({"type": "boolean", "description":
                            "Count every hidden row of a capped result instead of stopping \
                             early (default false), so later statements in the batch run \
                             unless the timeout ends the count."}),
                        false,
                    ),
                    Arg::Object => (
                        "object",
                        json!({"type": "string", "minLength": 1, "description":
                            "The object's name: Orders, dbo.Orders, [My Table], db..t"}),
                        true,
                    ),
                    Arg::Pattern => (
                        "pattern",
                        json!({"type": "string", "minLength": 1,
                            "description": "A substring or a * ? glob."}),
                        true,
                    ),
                    Arg::OptionalPattern => (
                        "pattern",
                        json!({"type": "string", "description": "A substring or a * ? glob."}),
                        false,
                    ),
                    Arg::InDefinitions => (
                        "in_definitions",
                        json!({"type": "boolean", "description":
                            "Also search module bodies (views, procedures, functions)."}),
                        false,
                    ),
                };
                properties.insert(name.into(), schema);
                if needed {
                    required.push(name);
                }
            }
            let profile_description = if names.len() == 1 {
                "The server to use; the only one is the default."
            } else {
                "The server to use: one of the profiles named in the server's instructions."
            };
            properties.insert(
                "profile".into(),
                json!({"type": "string", "enum": names, "description": profile_description}),
            );
            properties.insert(
                "database".into(),
                json!({"type": "string", "minLength": 1, "maxLength": 128, "description":
                    "A database to run in, instead of the profile's."}),
            );
            let (rows, cell) = limit_descriptions(tool.name, launch);
            if tool.rows {
                properties.insert(
                    "max_rows".into(),
                    json!({"type": "integer", "minimum": 0, "maximum": MAX_ROWS_ARG,
                        "description": rows}),
                );
            }
            properties.insert(
                "max_cell".into(),
                json!({"type": "integer", "minimum": 0, "maximum": MAX_CELL_ARG,
                    "description": cell}),
            );
            properties.insert(
                "timeout".into(),
                json!({"type": "integer", "minimum": 1,
                    "description": "Seconds a batch may run, up to the server's own limit."}),
            );
            properties.insert(
                "format".into(),
                json!({"type": "string", "enum": ["text", "json"],
                    "description": "text (the default, compact) or json."}),
            );
            let mut schema = json!({
                "type": "object",
                "properties": properties,
                "additionalProperties": false,
            });
            // With several profiles a call must name one, as `plan` enforces.
            if names.len() > 1 {
                required.push("profile");
            }
            if !required.is_empty() {
                schema["required"] = json!(required);
            }
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": schema,
                "annotations": {"readOnlyHint": true, "openWorldHint": false},
            })
        })
        .collect();
    Value::Array(tools)
}

/// The server's instructions (docs/mcp-mode.md §6): what it is, the profiles it can reach and
/// how its output reads.
pub(crate) fn instructions(launch: &Launch) -> String {
    let profiles: Vec<String> = launch
        .profiles
        .iter()
        .map(|p| match &p.description {
            Some(description) => format!("{} ({})", p.name, description),
            None => p.name.clone(),
        })
        .collect();
    let choose = if launch.profiles.len() > 1 {
        " Pass `profile` to choose one."
    } else {
        ""
    };
    format!(
        "sqlh runs read-only T-SQL against SQL Server. Profiles: {}.{choose} Use `describe`, \
         `find` or `deps` before writing queries against unfamiliar tables. Results come in a \
         compact text format: `-- ` lines are metadata, the header is `name:type`, rows are \
         tab-separated, NULL is `NULL`, `\\\\` is one backslash, and the last line is the \
         status: `-- ok:`, `-- partial:` (a stopped row count kept later statements of its batch \
         from running; put each query in its own GO batch) or `-- failed`. Results are cut at \
         row and size limits, which the output says; raise `max_rows` only when you need \
         more. Every call is read-only (writes are refused and rolled back) and starts fresh: \
         temp tables last only within one call.",
        profiles.join("; ")
    )
}

/// What a call runs.
enum What {
    Sql { sql: String, check: bool },
    Command(SchemaCommand),
}

/// A call, checked and ready to run.
pub(crate) struct Planned {
    pub(crate) tool: &'static str,
    pub(crate) profile: usize,
    what: What,
    database: Option<String>,
    max_rows: Option<u64>,
    max_cell: Option<usize>,
    timeout: Option<u32>,
    /// `exact_count`: count every hidden row (decision 45; `query` only).
    exact_count: bool,
    pub(crate) format: Format,
}

/// Why a `tools/call` was refused before it ran (docs/mcp-mode.md §4, Errors). A request that
/// doesn't fit `CallToolRequest` (no tool name, an unknown tool, `arguments` present but not
/// an object, `null` included, an `inputResponses` that isn't an object or a `requestState`
/// that isn't a string) is a protocol error, -32602. Arguments the tool can't take (an unknown one, a wrong
/// type, a value out of range, a missing one, a profile the server doesn't serve) are a tool
/// execution error, which the spec puts in the result (`isError`) so the model can correct
/// the call; `format` is the call's own, where it named a valid one.
#[derive(Debug)]
pub(crate) enum Refused {
    Protocol(ProtocolError),
    Arguments { message: String, format: Format },
}

/// What a refusal shows of a name the client sent (a tool, an argument, a profile): enough to
/// recognize it, so a huge one can't make the answer huge.
pub(crate) fn echo(name: &str) -> String {
    const SHOWN: usize = 100;
    match name.char_indices().nth(SHOWN) {
        Some((at, _)) => format!("{}…", &name[..at]),
        None => name.to_owned(),
    }
}

/// The arguments that are whole numbers, which `protocol::parse` reads from their text so that
/// none is rounded (R41), and only these, so a call costs at most this many rereadings of its
/// line however many other arguments it holds (R42). Every tool's schema declares exactly these
/// as `integer` (`the_numeric_arguments_are_the_schemas_integers`).
pub(crate) const NUMERIC_ARGS: [&str; 3] = ["max_rows", "max_cell", "timeout"];

/// The arguments `tool` takes: its own, then those every tool takes, `max_rows` only where it
/// applies (all but `info`, R49).
fn takes(tool: &Tool) -> Vec<&'static str> {
    let rows: &[&str] = if tool.rows { &["max_rows"] } else { &[] };
    tool.args
        .iter()
        .map(|arg| arg_name(*arg))
        .chain(["profile", "database"])
        .chain(rows.iter().copied())
        .chain(["max_cell", "timeout", "format"])
        .collect()
}

/// Checks a `tools/call`'s name and arguments against the tool's schema (`Refused`); anything
/// the run itself finds wrong (a bad object name, a refused write, a SQL error) is a tool error
/// too, in the run's own output.
pub(crate) fn plan(params: &Map<String, Value>, launch: &Launch) -> Result<Planned, Refused> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(Refused::Protocol(ProtocolError::invalid_params(
            "Invalid params: tools/call needs a tool name",
        )));
    };
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return Err(Refused::Protocol(ProtocolError::invalid_params(format!(
            "Unknown tool: {}",
            echo(name)
        ))));
    };
    // The rest of `CallToolRequestParams` (R39): `arguments` left out is none, but present it is
    // an object (`null` included, it isn't one); the multi-round-trip members, which sqlh never
    // asks for, are typed all the same.
    let empty = Map::new();
    let args = match params.get("arguments") {
        None => &empty,
        Some(Value::Object(args)) => args,
        Some(_) => {
            return Err(Refused::Protocol(ProtocolError::invalid_params(format!(
                "Invalid params: the arguments of tool {} must be an object",
                tool.name
            ))));
        }
    };
    member(params, "", "inputResponses", false, Kind::Object).map_err(Refused::Protocol)?;
    member(params, "", "requestState", false, Kind::String).map_err(Refused::Protocol)?;
    // The format first, so a refusal of the other arguments comes in the format asked for.
    let format = match args.get("format") {
        None => Format::Text,
        Some(Value::String(f)) if f == "text" => Format::Text,
        Some(Value::String(f)) if f == "json" => Format::Json,
        Some(_) => {
            return Err(Refused::Arguments {
                message: format!(
                    "invalid arguments for {}: format must be text or json",
                    tool.name
                ),
                format: Format::Text,
            });
        }
    };
    let invalid = |why: &str| Refused::Arguments {
        message: format!("invalid arguments for {}: {why}", tool.name),
        format,
    };
    let known = takes(tool);
    for key in args.keys() {
        if !known.contains(&key.as_str()) {
            return Err(invalid(&format!(
                "unknown argument {} ({} takes {})",
                echo(key),
                tool.name,
                known.join(", ")
            )));
        }
    }
    let text = |key: &str| -> Result<Option<String>, Refused> {
        match args.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(invalid(&format!("{key} must be a string"))),
        }
    };
    let number = |key: &str, min: u64, max: u64| -> Result<Option<u64>, Refused> {
        match args.get(key) {
            None => Ok(None),
            // A whole number: an integer as `protocol::parse` leaves it, which reads `30.0`
            // and `3e1` from their text as 30 (R41). A float that reaches here is none, never
            // rounded: `10.0000000000000001` and `-1e-9999` are refused, not 10 and 0.
            Some(value) => match value.as_u64() {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                _ => Err(invalid(&format!(
                    "{key} must be a whole number from {min} to {max}"
                ))),
            },
        }
    };
    let required = |key: &str| -> Result<String, Refused> {
        match text(key)? {
            None => Err(invalid(&format!("missing required property {key}"))),
            Some(value) if value.is_empty() => Err(invalid(&format!("{key} is empty"))),
            Some(value) => Ok(value),
        }
    };

    let profile = match text("profile")? {
        Some(name) => launch
            .profiles
            .iter()
            // In any ASCII case, as `--profile` and the CLI's profiles match.
            .position(|p| p.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| {
                let names: Vec<&str> = launch.profiles.iter().map(|p| p.name.as_str()).collect();
                invalid(&format!(
                    "profile {} isn't one of {}",
                    echo(&name),
                    names.join(", ")
                ))
            })?,
        None if launch.profiles.len() == 1 => 0,
        None => {
            let names: Vec<&str> = launch.profiles.iter().map(|p| p.name.as_str()).collect();
            return Err(invalid(&format!(
                "missing required property profile: one of {}",
                names.join(", ")
            )));
        }
    };
    let database = text("database")?;
    if let Some(database) = &database
        && (database.is_empty() || database.encode_utf16().count() > 128)
    {
        return Err(invalid("database must be 1 to 128 characters"));
    }
    let max_rows = number("max_rows", 0, MAX_ROWS_ARG)?;
    let max_cell = number("max_cell", 0, MAX_CELL_ARG)?.map(|n| n as usize);
    let timeout = number("timeout", 1, u64::from(u32::MAX))?.map(|n| n as u32);
    let flag = |key: &str| -> Result<bool, Refused> {
        match args.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(invalid(&format!("{key} must be true or false"))),
        }
    };
    let in_definitions = flag("in_definitions")?;
    let exact_count = flag("exact_count")?;
    let what = match tool.name {
        "query" | "check" => What::Sql {
            sql: required("sql")?,
            check: tool.name == "check",
        },
        "info" => What::Command(SchemaCommand::Info),
        "databases" => What::Command(SchemaCommand::Dbs),
        "tables" => What::Command(SchemaCommand::Tables(
            text("pattern")?.filter(|p| !p.is_empty()),
        )),
        "describe" => What::Command(SchemaCommand::Describe(required("object")?)),
        "find" => What::Command(SchemaCommand::Find {
            pattern: required("pattern")?,
            text: in_definitions,
        }),
        "definition" => What::Command(SchemaCommand::Def(required("object")?)),
        _ => What::Command(SchemaCommand::Deps(required("object")?)),
    };
    Ok(Planned {
        tool: tool.name,
        profile,
        what,
        database,
        max_rows,
        max_cell,
        timeout,
        exact_count,
        format,
    })
}

fn arg_name(arg: Arg) -> &'static str {
    match arg {
        Arg::Sql => "sql",
        Arg::Object => "object",
        Arg::Pattern | Arg::OptionalPattern => "pattern",
        Arg::InDefinitions => "in_definitions",
        Arg::ExactCount => "exact_count",
    }
}

/// The result of a call refused before it ran (`Refused::Arguments`, or one call too many): a
/// usage error, exit 3, as the run's own output would give it, so its text is escaped like any
/// other and held to the server's `--max-output` (`limits`, the launch's).
pub(crate) fn refusal(message: String, format: Format, limits: Limits) -> String {
    let limits = Limits {
        advice: Advice::Mcp,
        ..limits
    };
    let mut out = Collected::new(format, limits);
    let outcome = RunOutcome::new(Err(Failure::usage(message)));
    run::close(out.output(), outcome, Instant::now(), Advice::Mcp);
    out.into_text()
}

/// Runs a planned call: its output, in the format asked for, and whether it is an error
/// (`isError`: the run's exit code isn't 0).
pub(crate) async fn execute(
    engine: &Engine,
    launch: &Launch,
    planned: Planned,
    cancel: &Cancel,
) -> (String, Exit) {
    let started = Instant::now();
    let settings = engine.settings(planned.profile);
    let mut limits: Limits = settings.limits;
    if let What::Command(command) = &planned.what {
        command.limits(
            &mut limits,
            launch.max_rows.is_some(),
            launch.max_cell.is_some(),
        );
    }
    if let Some(rows) = planned.max_rows {
        limits.max_rows = rows;
    }
    if let Some(cell) = planned.max_cell {
        limits.max_cell = cell;
    }
    // A call may lower the timeout, never raise it past the server's (0: none).
    let timeout = match (planned.timeout, settings.query_timeout) {
        (Some(asked), 0) => asked,
        (Some(asked), limit) => asked.min(limit),
        (None, limit) => limit,
    };
    limits.timeout_at_limit = settings.query_timeout > 0 && timeout >= settings.query_timeout;
    let mut out = Collected::new(planned.format, limits);
    let outcome = match prepare(&planned, settings) {
        Ok(prepared) => {
            engine
                .run(Call {
                    profile: planned.profile,
                    prepared: &prepared,
                    timeout,
                    out: out.output(),
                    cancel,
                    started,
                })
                .await
        }
        Err(failure) => RunOutcome::new(Err(failure)),
    };
    let exit = run::close(out.output(), outcome, started, settings.limits.advice);
    (out.into_text(), exit)
}

/// The run core's input for a call. `$(var)` substitution is off: in the CLI it reads the
/// caller's own environment, but here the environment is the server's, so a call could read
/// what the harness keeps there.
fn prepare(
    planned: &Planned,
    settings: &crate::config::Settings,
) -> Result<run::Prepared, crate::error::Failure> {
    let (input, check, origin, report) = match &planned.what {
        What::Sql { sql, check } => {
            if sql.len() > MAX_SQL_BYTES {
                return Err(crate::error::Failure::usage(format!(
                    "the SQL is {} bytes; a call takes at most {MAX_SQL_BYTES}",
                    sql.len()
                )));
            }
            (
                Input::Query(sql.clone()),
                *check,
                QueryOrigin::User,
                Report::Off,
            )
        }
        // `info` says how the connection was made, in a note, as `sqlh info` does.
        What::Command(command) => (
            command.input(settings.limits.advice)?,
            false,
            QueryOrigin::Generated,
            command.report(),
        ),
    };
    let request = Request {
        input,
        vars: None,
        terminator: "GO".into(),
        check,
        continue_on_error: false,
        report,
        database: planned.database.clone(),
        exact_count: planned.exact_count,
    };
    run::prepare(request, settings, origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(names: &[&str]) -> Launch {
        Launch {
            profiles: names
                .iter()
                .map(|n| Profile {
                    name: (*n).into(),
                    description: Some(format!("{n} server")),
                })
                .collect(),
            max_rows: None,
            max_cell: None,
        }
    }

    fn call(value: Value) -> Map<String, Value> {
        super::super::protocol::object(value)
    }

    // docs/mcp-mode.md §4: the tools, in order, read-only, each with a closed schema whose
    // profile is the launch's list.
    #[test]
    fn the_numeric_arguments_are_the_schemas_integers() {
        let tools = definitions(&launch(&["test"]));
        for tool in tools.as_array().unwrap() {
            let mut integers: Vec<&str> = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, schema)| schema["type"] == "integer")
                .map(|(name, _)| name.as_str())
                .collect();
            integers.sort_unstable();
            // `info` takes no max_rows (it returns one row), but max_cell (R49).
            let mut wanted: Vec<&str> = NUMERIC_ARGS
                .iter()
                .copied()
                .filter(|arg| tool["name"] != "info" || *arg != "max_rows")
                .collect();
            wanted.sort_unstable();
            assert_eq!(integers, wanted, "{}", tool["name"]);
        }
    }

    // MCP client feedback (2026-10-31, decision 45): the schemas state each limit's default,
    // `check` says its SQL is compiled, not run, `info` takes no max_rows (but max_cell, which
    // a small configured limit still applies to its values, R49), and `query` alone takes
    // `exact_count`, which a call then passes on to the run.
    #[test]
    fn the_schemas_state_their_defaults_and_exact_count_reaches_the_run() {
        let schema = |tools: &Value, name: &str| {
            tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap()["inputSchema"]["properties"]
                .clone()
        };
        let tools = definitions(&launch(&["test"]));
        let query = schema(&tools, "query");
        assert_eq!(
            query["max_rows"]["description"],
            "Rows shown per result set (default 100, or the profile's max_rows); 0 shows all \
             (within the size limit)."
        );
        assert_eq!(
            query["max_cell"]["description"],
            "Characters shown per value (default 500, or the profile's max_cell); 0 shows all \
             (within the size limit)."
        );
        assert_eq!(query["exact_count"]["type"], "boolean");
        assert_eq!(query["sql"]["description"], "The T-SQL to run.");
        let check = schema(&tools, "check");
        assert_eq!(
            check["sql"]["description"],
            "The T-SQL to compile (not run)."
        );
        assert!(check.get("exact_count").is_none());
        let info = schema(&tools, "info");
        assert!(info.get("max_rows").is_none(), "{info}");
        assert_eq!(
            info["max_cell"]["description"],
            "Characters shown per value (default 500, or the profile's max_cell); 0 shows all \
             (within the size limit)."
        );
        let describe = schema(&tools, "describe");
        assert!(
            describe["max_rows"]["description"]
                .as_str()
                .unwrap()
                .contains("(default 2000)")
                && describe["max_cell"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("(default 4000)"),
            "{describe}"
        );
        let definition = schema(&tools, "definition");
        assert!(
            definition["max_cell"]["description"]
                .as_str()
                .unwrap()
                .contains("(default: no limit)"),
            "{definition}"
        );
        // A launch's own limits are every tool's default.
        let mut given = launch(&["test"]);
        given.max_rows = Some(25);
        given.max_cell = Some(0);
        let tools = definitions(&given);
        for name in ["query", "describe", "definition"] {
            let props = schema(&tools, name);
            assert!(
                props["max_rows"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("(default 25)")
                    && props["max_cell"]["description"]
                        .as_str()
                        .unwrap()
                        .contains("(default: no limit)"),
                "{name}: {props}"
            );
        }
        let one = launch(&["test"]);
        // R49: info's values can be read whole under a small configured max_cell.
        let Ok(planned) = plan(
            &call(json!({"name": "info", "arguments": {"max_cell": 0}})),
            &one,
        ) else {
            panic!("info takes max_cell");
        };
        assert_eq!(planned.max_cell, Some(0));
        let exact = |args: Value| {
            let planned = plan(&call(json!({"name": "query", "arguments": args})), &one)
                .unwrap_or_else(|_| panic!("planned"));
            let settings = run::unconnected_settings(false);
            prepare(&planned, &settings).unwrap().exact_count()
        };
        assert!(exact(json!({"sql": "SELECT 1", "exact_count": true})));
        assert!(!exact(json!({"sql": "SELECT 1", "exact_count": false})));
        assert!(!exact(json!({"sql": "SELECT 1"})));
    }

    #[test]
    fn the_tools_are_listed_with_closed_schemas() {
        let tools = definitions(&launch(&["test", "reporting"]));
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "query",
                "check",
                "info",
                "databases",
                "tables",
                "describe",
                "find",
                "definition",
                "deps"
            ]
        );
        for tool in tools.as_array().unwrap() {
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["annotations"]["openWorldHint"], false);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert_eq!(
                tool["inputSchema"]["properties"]["profile"]["enum"],
                json!(["test", "reporting"])
            );
        }
        // With several profiles, a call must name one; with one, it is the default.
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            json!(["sql", "profile"])
        );
        assert_eq!(tools[2]["inputSchema"]["required"], json!(["profile"]));
        let one = definitions(&launch(&["test"]));
        assert_eq!(one[0]["inputSchema"]["required"], json!(["sql"]));
        assert_eq!(one[2]["inputSchema"].get("required"), None);
        assert_eq!(
            one[0]["inputSchema"]["properties"]["database"]["maxLength"],
            128
        );
        let text = instructions(&launch(&["test", "reporting"]));
        assert!(
            text.contains(
                "Profiles: test (test server); reporting (reporting server). Pass `profile`"
            ),
            "{text}"
        );
    }

    // A request that doesn't fit CallToolRequest is -32602; arguments the tool can't take are
    // a tool execution error, in the format asked for; the profile is one of the launch's (the
    // default when there is one).
    #[test]
    fn calls_are_checked_against_the_schema() {
        let one = launch(&["test"]);
        let two = launch(&["test", "reporting"]);
        let planned = plan(
            &call(json!({"name": "query", "arguments": {"sql": "SELECT 1"}})),
            &one,
        )
        .unwrap();
        assert_eq!((planned.tool, planned.profile), ("query", 0));
        let planned = plan(
            &call(json!({"name": "describe", "arguments": {"object": "dbo.T", "profile": "reporting", "max_rows": 5, "format": "json"}})),
            &two,
        )
        .unwrap();
        assert_eq!(planned.profile, 1);
        let any_case = plan(
            &call(json!({"name": "query", "arguments": {"sql": "x", "profile": "Reporting"}})),
            &two,
        )
        .unwrap();
        assert_eq!(any_case.profile, 1, "a profile's name in any ASCII case");
        assert_eq!(planned.max_rows, Some(5));
        assert_eq!(planned.format, Format::Json);
        // A whole number written as 30.0 or 3e1 is that number, as `protocol::parse` reads it
        // from its text (R41); a float that reaches the plan is none, never rounded.
        let parsed = |arguments: &str| {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"query","arguments":{arguments}}}}}"#
            );
            match super::super::protocol::parse(&line) {
                Ok(super::super::protocol::Incoming::Request { params, .. }) => params,
                other => panic!("{arguments}: {other:?}"),
            }
        };
        for raw in ["30", "30.0", "3e1", "300e-1"] {
            let planned =
                plan(&parsed(&format!(r#"{{"sql":"x","timeout":{raw}}}"#)), &one).unwrap();
            assert_eq!(planned.timeout, Some(30), "{raw}");
        }
        for raw in [
            "30.0000000000000001",
            "-1e-9999",
            "1e30",
            "30.5",
            "0",
            "-5.0",
        ] {
            match plan(&parsed(&format!(r#"{{"sql":"x","timeout":{raw}}}"#)), &one) {
                Err(Refused::Arguments { message, .. }) => assert!(
                    message.contains("timeout must be a whole number from 1 to"),
                    "{raw}: {message}"
                ),
                _ => panic!("{raw}: not refused"),
            }
        }
        for raw in ["10.0000000000000001", "-1e-9999", "10001", "1e30"] {
            match plan(&parsed(&format!(r#"{{"sql":"x","max_rows":{raw}}}"#)), &one) {
                Err(Refused::Arguments { message, .. }) => assert!(
                    message.contains("max_rows must be a whole number from 0 to 10000"),
                    "{raw}: {message}"
                ),
                _ => panic!("{raw}: not refused"),
            }
        }
        // Zero stays valid where it is (all rows, a whole value), however it is written.
        for raw in ["0", "0.0", "-0"] {
            let planned = plan(
                &parsed(&format!(
                    r#"{{"sql":"x","max_rows":{raw},"max_cell":{raw}}}"#
                )),
                &one,
            )
            .unwrap();
            assert_eq!(
                (planned.max_rows, planned.max_cell),
                (Some(0), Some(0)),
                "{raw}"
            );
        }
        for (params, wanted) in [
            (json!({}), "needs a tool name"),
            (json!({"name": "drop"}), "Unknown tool: drop"),
            (
                json!({"name": "query", "arguments": []}),
                "the arguments of tool query must be an object",
            ),
            // R39: `null` isn't an object either; only `arguments` left out is none.
            (
                json!({"name": "query", "arguments": null}),
                "the arguments of tool query must be an object",
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x"}, "requestState": 1}),
                "Invalid params: requestState must be a string",
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x"}, "inputResponses": []}),
                "Invalid params: inputResponses must be an object",
            ),
        ] {
            match plan(&call(params.clone()), &one) {
                Err(Refused::Protocol(error)) => {
                    assert_eq!(error.code, super::super::protocol::INVALID_PARAMS);
                    assert!(
                        error.message.contains(wanted),
                        "{params}: {}",
                        error.message
                    );
                }
                _ => panic!("{params}: not a protocol error"),
            }
        }
        for (params, wanted, launch) in [
            (
                json!({"name": "query"}),
                "missing required property sql",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": 1}}),
                "sql must be a string",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "rows": 1}}),
                "invalid arguments for query: unknown argument rows (query takes sql, \
                 exact_count, profile, database, max_rows, max_cell, timeout, format)",
                &one,
            ),
            // `info` takes no max_rows (R49: but max_cell); `exact_count` is query's alone, and
            // a boolean.
            (
                json!({"name": "info", "arguments": {"max_rows": 5}}),
                "unknown argument max_rows (info takes profile, database, max_cell, timeout, \
                 format)",
                &one,
            ),
            (
                json!({"name": "tables", "arguments": {"exact_count": true}}),
                "unknown argument exact_count",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "exact_count": 1}}),
                "exact_count must be true or false",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x"}}),
                "missing required property profile: one of test, reporting",
                &two,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "profile": "prod"}}),
                "profile prod isn't one of test, reporting",
                &two,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "max_rows": 10001}}),
                "max_rows must be a whole number from 0 to 10000",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "timeout": 0}}),
                "timeout must be a whole number",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "format": "csv"}}),
                "format must be text or json",
                &one,
            ),
            (
                json!({"name": "find", "arguments": {"pattern": ""}}),
                "pattern is empty",
                &one,
            ),
            (
                json!({"name": "describe", "arguments": {"object": ""}}),
                "object is empty",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": ""}}),
                "sql is empty",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "max_rows": 2.5}}),
                "max_rows must be a whole number",
                &one,
            ),
            (
                json!({"name": "query", "arguments": {"sql": "x", "database": ""}}),
                "database must be 1 to 128 characters",
                &one,
            ),
            (
                json!({"name": "find", "arguments": {"pattern": "x", "in_definitions": "yes"}}),
                "in_definitions must be true or false",
                &one,
            ),
        ] {
            match plan(&call(params.clone()), launch) {
                Err(Refused::Arguments { message, .. }) => {
                    assert!(message.contains(wanted), "{params}: {message}");
                }
                _ => panic!("{params}: not refused as arguments"),
            }
        }
        // The refusal is the run's usage error, in the call's format where it named one.
        let Err(Refused::Arguments { message, format }) = plan(
            &call(json!({"name": "query", "arguments": {"sql": "", "format": "json"}})),
            &one,
        ) else {
            panic!("refused");
        };
        assert_eq!(format, Format::Json);
        let json: Value =
            serde_json::from_str(refusal(message, format, Limits::default()).trim_end()).unwrap();
        assert_eq!(json["exit_code"], 3, "{json}");
        assert_eq!(json["error"], "invalid arguments for query: sql is empty");
        // A client's text in the refusal is escaped like any other: one line stays one line.
        let Err(Refused::Arguments { message, format }) = plan(
            &call(json!({"name": "query", "arguments": {"sql": "x", "r\n-- ok: forged": 1}})),
            &one,
        ) else {
            panic!("refused");
        };
        assert_eq!(format, Format::Text);
        let text = refusal(message, format, Limits::default());
        assert!(
            text.starts_with("-- error: invalid arguments for query: unknown argument r")
                && text.contains("-- failed (exit 3): usage error, "),
            "{text}"
        );
        assert_eq!(text.lines().count(), 2, "{text}");
        // A name the client sent is shown cut short, and the answer is held to the server's
        // --max-output.
        let Err(Refused::Arguments { message, .. }) = plan(
            &call(json!({"name": "query", "arguments": {"sql": "x", "k".repeat(5000): 1}})),
            &one,
        ) else {
            panic!("refused");
        };
        assert!(
            message.contains(&format!(
                "unknown argument {}… (query takes",
                "k".repeat(100)
            )),
            "{message}"
        );
        let small = Limits {
            max_output: 4096,
            ..Limits::default()
        };
        let long = format!("invalid arguments for query: {}", "x".repeat(10_000));
        let text = refusal(long, Format::Text, small);
        assert!(text.len() <= 4096, "{}", text.len());
        assert!(
            text.starts_with("-- error: invalid arguments for query: xxx")
                && text.contains("-- failed (exit 3): usage error, "),
            "{text}"
        );
        assert_eq!(echo("short"), "short");
        assert_eq!(echo(&"é".repeat(150)), format!("{}…", "é".repeat(100)));
    }
}
