//! Arguments: sqlcmd-compatible short flags plus our long flags (DESIGN.md §4.2), and the
//! `profile` subcommands. Settings a profile can supply are `Option`s so config.rs can tell
//! "not given" from "given".

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::{MAX_LOCK_TIMEOUT, WritePolicy, valid_name};
use crate::db::Encrypt;
use crate::output::{Format, MIN_MAX_OUTPUT};

pub use crate::schema::Command as SchemaCommand;

pub enum Command {
    Run(Box<Opts>),
    Profile(ProfileCommand),
    /// `sqlh mcp`: the MCP server (docs/mcp-mode.md).
    Mcp(Box<McpOpts>),
    Help,
    Version,
}

/// `sqlh mcp --profile A [--profile B ...] [limits]` (docs/mcp-mode.md §3; owner decisions 16,
/// 18 and 19): the profiles the server may reach, in order, and the launch limits every call
/// starts from. No server, login or write flag: those come from the profiles, and the server
/// is read-only.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct McpOpts {
    pub profiles: Vec<String>,
    pub max_rows: Option<u64>,
    pub max_cell: Option<usize>,
    pub max_output: Option<u64>,
    pub query_timeout: Option<u32>,
    /// `--pool N`: connections per profile, 1 to `engine::MAX_POOL`.
    pub pool: Option<usize>,
    /// `--idle MINUTES`: how long a connection may stay idle; 0 keeps them open.
    pub idle_minutes: Option<u64>,
    pub verbose: bool,
}

pub enum ProfileCommand {
    Add {
        name: String,
        opts: Box<Opts>,
    },
    /// `profiles list [--search TEXT]`: the profiles whose name or description contains
    /// `search` (any case), or all of them.
    List {
        search: Option<String>,
    },
    Test {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Default)]
pub struct Opts {
    pub profile: Option<String>,
    pub server: Option<String>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub integrated: bool,
    pub query: Option<String>,
    pub inputs: Vec<String>,
    pub output: Option<PathBuf>,
    pub vars: Vec<(String, String)>,
    pub no_substitution: bool,
    pub login_timeout: Option<u32>,
    pub query_timeout: Option<u32>,
    pub terminator: Option<String>,
    /// `-C` or `--trust-server-certificate[=true|false]`; unset defers to the profile.
    pub trust_server_certificate: Option<bool>,
    pub encrypt: Option<Encrypt>,
    pub read_only_intent: bool,
    pub multi_subnet_failover: bool,
    pub max_rows: Option<u64>,
    pub max_cell: Option<usize>,
    pub max_results: Option<usize>,
    pub max_output: Option<u64>,
    pub allow_write: bool,
    /// `--lock-timeout SECONDS`: how long a statement waits for a lock before it fails with
    /// error 1222; 0 means no limit (DESIGN.md §4.2). Unset defers to the profile.
    pub lock_timeout: Option<u32>,
    /// `--read-uncommitted[=true|false]`: read without shared locks, in read-only runs
    /// (DESIGN.md §4.3). Unset defers to the profile.
    pub read_uncommitted: Option<bool>,
    /// `--exact-count[=true|false]`: count every row that is not shown, however long that takes
    /// (DESIGN.md §4.4). Unset defers to the profile.
    pub exact_count: Option<bool>,
    pub check: bool,
    pub continue_on_error: bool,
    pub verbose: bool,
    /// Only for `sqlh profiles add`.
    pub write_policy: Option<WritePolicy>,
    pub format: Option<Format>,
    /// Only for `sqlh profiles add`: prompt for a new password even if one is stored.
    pub new_password: bool,
    /// Only for `sqlh profiles add`: the profile's description; `""` removes it.
    pub description: Option<String>,
    /// A schema command (`sqlh describe dbo.T`), named by the first positional word.
    pub command: Option<SchemaCommand>,
    /// Internal (set by `profiles test`): print a `-- note:` describing the connection.
    pub show_connection: bool,
    /// sqlcmd flags accepted for compatibility but without effect.
    pub ignored: Vec<String>,
}

pub fn parse() -> Result<Command, String> {
    parse_all(std::env::args_os().skip(1).collect())
}

fn parse_all(args: Vec<OsString>) -> Result<Command, String> {
    // `profile` is the older spelling of `profiles` (decision 37); scripts still use it.
    if args
        .first()
        .is_some_and(|a| a == "profiles" || a == "profile")
    {
        return parse_profile(args.into_iter().skip(1).collect());
    }
    if args.first().is_some_and(|a| a == "mcp") {
        return parse_mcp(lexopt::Parser::from_args(args.into_iter().skip(1)));
    }
    parse_from(lexopt::Parser::from_args(args))
}

#[cfg(test)]
fn parse_args(args: &[&str]) -> Result<Command, String> {
    parse_all(args.iter().map(OsString::from).collect())
}

const MCP_USAGE: &str = "usage: sqlh mcp --profile NAME [--profile NAME ...] [--max-rows N] [--max-cell N] [--max-output N] [-t seconds] [--pool N] [--idle MINUTES] [--verbose]";

/// `sqlh mcp`'s own flags (`McpOpts`). Shared limits are parsed as for a run.
fn parse_mcp(mut p: lexopt::Parser) -> Result<Command, String> {
    use lexopt::prelude::*;
    let mut o = McpOpts::default();
    while let Some(arg) = p.next().map_err(|e| e.to_string())? {
        match arg {
            Long("help") | Short('?') => return Ok(Command::Help),
            Long("profile") => {
                let name = string(p.value())?;
                if !valid_name(&name) {
                    return Err(format!(
                        "profile name {name:?} may only use letters, digits, _ and -"
                    ));
                }
                if o.profiles.iter().any(|p| p.eq_ignore_ascii_case(&name)) {
                    return Err(format!("profile {name} is listed twice"));
                }
                o.profiles.push(name);
            }
            Long("max-rows") => o.max_rows = Some(number(p.value(), "--max-rows")?),
            Long("max-cell") => o.max_cell = Some(number(p.value(), "--max-cell")?),
            Long("max-output") => {
                // 0 (no limit) isn't offered here: a call's result is held in memory.
                let bytes: u64 = number(p.value(), "--max-output")?;
                if bytes < MIN_MAX_OUTPUT {
                    return Err(format!(
                        "--max-output {bytes}: sqlh mcp holds each result in memory, so it takes at least {MIN_MAX_OUTPUT} bytes"
                    ));
                }
                o.max_output = Some(bytes);
            }
            Short('t') | Long("query-timeout") => o.query_timeout = Some(number(p.value(), "-t")?),
            Long("pool") => {
                let pool: usize = number(p.value(), "--pool")?;
                if !(1..=crate::engine::MAX_POOL).contains(&pool) {
                    return Err(format!(
                        "--pool {pool}: expected 1 to {} connections per profile",
                        crate::engine::MAX_POOL
                    ));
                }
                o.pool = Some(pool);
            }
            Long("idle") => o.idle_minutes = Some(number(p.value(), "--idle")?),
            Long("verbose") => o.verbose = true,
            Long("allow-write") => {
                return Err(
                    "sqlh mcp is read-only: it takes no --allow-write (writes go through the sqlh command line)"
                        .to_owned(),
                );
            }
            _ => {
                return Err(format!(
                    "{} (sqlh mcp takes its connections from its profiles); {MCP_USAGE}",
                    arg.unexpected()
                ));
            }
        }
    }
    if o.profiles.is_empty() {
        return Err(format!(
            "sqlh mcp needs at least one --profile; {MCP_USAGE}"
        ));
    }
    Ok(Command::Mcp(Box::new(o)))
}

const PROFILE_USAGE: &str = "usage: sqlh profiles add NAME -S server [-d db] [-U login] [options] | list [--search TEXT] | test NAME | rm NAME";

fn parse_profile(args: Vec<OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let action = args
        .next()
        .and_then(|a| a.into_string().ok())
        .ok_or(PROFILE_USAGE)?;
    let mut name = || -> Result<String, String> {
        let name = args
            .next()
            .and_then(|a| a.into_string().ok())
            .ok_or_else(|| format!("profile {action}: missing NAME; {PROFILE_USAGE}"))?;
        if valid_name(&name) {
            Ok(name)
        } else {
            Err(format!(
                "profile name {name:?} may only use letters, digits, _ and -"
            ))
        }
    };
    let command = match action.as_str() {
        "add" => {
            let name = name()?;
            let rest: Vec<OsString> = args.by_ref().collect();
            let opts = match parse_from(lexopt::Parser::from_args(rest))? {
                Command::Run(opts) => opts,
                _ => return Err(PROFILE_USAGE.into()),
            };
            if opts.query.is_some() || !opts.inputs.is_empty() || opts.command.is_some() {
                return Err("profiles add takes connection options only, not SQL".into());
            }
            ProfileCommand::Add { name, opts }
        }
        "list" | "ls" => {
            let rest: Vec<OsString> = args.by_ref().collect();
            parse_list(lexopt::Parser::from_args(rest))?
        }
        "test" => ProfileCommand::Test { name: name()? },
        "rm" | "remove" => ProfileCommand::Remove { name: name()? },
        _ => {
            return Err(format!(
                "unknown profile action {action:?}; {PROFILE_USAGE}"
            ));
        }
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {extra:?}; {PROFILE_USAGE}"));
    }
    Ok(Command::Profile(command))
}

/// `profiles list`'s one option, `--search TEXT` (plan-history.md 4.2). The list is text only
/// (owner, 2026-10-13): it is setup an agent reads once, and text is the cheapest format to read.
fn parse_list(mut p: lexopt::Parser) -> Result<ProfileCommand, String> {
    use lexopt::prelude::*;
    let mut search = None;
    while let Some(arg) = p.next().map_err(|e| e.to_string())? {
        match arg {
            Long("search") => search = Some(string(p.value())?),
            _ => {
                return Err(format!(
                    "profiles list takes --search TEXT only (its output is text), not {}; \
                     {PROFILE_USAGE}",
                    arg.unexpected()
                ));
            }
        }
    }
    Ok(ProfileCommand::List { search })
}

fn output_format(text: &str) -> Result<Format, String> {
    Format::parse(text).ok_or_else(|| match text {
        "md" => "--format md is not implemented yet; use text, json or csv".to_owned(),
        _ => format!("--format {text}: expected text, json or csv"),
    })
}

fn parse_from(mut p: lexopt::Parser) -> Result<Command, String> {
    use lexopt::prelude::*;
    let mut o = Opts::default();
    // Words that aren't options: the SQL, or a schema command and its argument.
    let mut words: Vec<String> = Vec::new();
    let mut text = false;
    while let Some(arg) = p.next().map_err(|e| e.to_string())? {
        match arg {
            Long("help") | Short('?') => return Ok(Command::Help),
            Long("version") => return Ok(Command::Version),
            Long("profile") => o.profile = Some(string(p.value())?),
            Short('S') | Long("server") => o.server = Some(string(p.value())?),
            Short('d') | Long("database") => o.database = Some(string(p.value())?),
            Short('E') => o.integrated = true,
            Short('U') | Long("user") => o.user = Some(string(p.value())?),
            Short('P') => {
                let _ = p.value();
                return Err("-P is refused: a password on the command line leaks into transcripts and process lists. Save a profile (sqlh profiles add) or set SQLCMDPASSWORD".into());
            }
            Short('Q') | Short('q') | Long("query") => set_query(&mut o, string(p.value())?)?,
            Short('i') | Long("input") => o.inputs.push(string(p.value())?),
            Short('o') | Long("output") => {
                o.output = Some(PathBuf::from(p.value().map_err(|e| e.to_string())?))
            }
            Short('v') | Long("var") => {
                for value in p.values().map_err(|e| e.to_string())? {
                    let value = string(Ok(value))?;
                    // In `-v a=1 "SELECT 1"`, a value that isn't name=value is the positional query.
                    match value.split_once('=') {
                        Some((name, v)) if is_var_name(name) => {
                            o.vars.push((name.to_owned(), v.to_owned()))
                        }
                        _ => words.push(value),
                    }
                }
            }
            Short('x') => o.no_substitution = true,
            Short('l') | Long("login-timeout") => o.login_timeout = Some(number(p.value(), "-l")?),
            Short('t') | Long("query-timeout") => o.query_timeout = Some(number(p.value(), "-t")?),
            Short('b') => {} // stopping at the first failed batch is already the default
            Short('c') => o.terminator = Some(string(p.value())?),
            Short('C') => o.trust_server_certificate = Some(true),
            Long("trust-server-certificate") => {
                o.trust_server_certificate = Some(switch(&mut p, "--trust-server-certificate")?)
            }
            Long("read-uncommitted") => {
                o.read_uncommitted = Some(switch(&mut p, "--read-uncommitted")?)
            }
            Long("exact-count") => o.exact_count = Some(switch(&mut p, "--exact-count")?),
            Short('N') => {
                o.encrypt = Some(match p.optional_value() {
                    None => Encrypt::Mandatory,
                    Some(v) => encrypt(&string(Ok(v))?)?,
                })
            }
            Long("encrypt") => o.encrypt = Some(encrypt(&string(p.value())?)?),
            Short('K') => {
                let intent = string(p.value())?;
                if intent.eq_ignore_ascii_case("readonly") {
                    o.read_only_intent = true;
                } else if !intent.eq_ignore_ascii_case("readwrite") {
                    return Err(format!("-K {intent}: expected ReadOnly"));
                }
            }
            Short('M') => o.multi_subnet_failover = true,
            Long("max-rows") => o.max_rows = Some(number(p.value(), "--max-rows")?),
            Long("max-cell") => o.max_cell = Some(number(p.value(), "--max-cell")?),
            Long("max-results") => o.max_results = Some(number(p.value(), "--max-results")?),
            Long("max-output") => {
                let bytes: u64 = number(p.value(), "--max-output")?;
                if bytes != 0 && bytes < MIN_MAX_OUTPUT {
                    return Err(format!(
                        "--max-output {bytes}: use 0 for no limit, or at least {MIN_MAX_OUTPUT} bytes"
                    ));
                }
                o.max_output = Some(bytes);
            }
            Long("allow-write") => o.allow_write = true,
            Long("lock-timeout") => {
                let seconds: u32 = number(p.value(), "--lock-timeout")?;
                if seconds > MAX_LOCK_TIMEOUT {
                    return Err(format!(
                        "--lock-timeout {seconds}: at most {MAX_LOCK_TIMEOUT} seconds; use 0 for no limit"
                    ));
                }
                o.lock_timeout = Some(seconds);
            }
            Long("check") => o.check = true,
            Long("continue-on-error") => o.continue_on_error = true,
            Long("verbose") => o.verbose = true,
            Long("new-password") => o.new_password = true,
            Long("write-policy") => {
                let text = string(p.value())?;
                o.write_policy = Some(WritePolicy::parse(&text).ok_or_else(|| {
                    format!("--write-policy {text}: expected deny, flag or allow")
                })?);
            }
            Long("format") => o.format = Some(output_format(&string(p.value())?)?),
            Long("description") => o.description = Some(string(p.value())?),
            // sqlcmd flags with no effect here (output is always compact UTF-8).
            Short(c @ ('h' | 's' | 'w' | 'y' | 'Y' | 'm' | 'a' | 'f' | 'H')) => {
                let _ = p.value();
                o.ignored.push(format!("-{c}"));
            }
            Short(c @ ('r' | 'X' | 'k')) => {
                let _ = p.optional_value();
                o.ignored.push(format!("-{c}"));
            }
            Short(c @ ('W' | 'e' | 'I' | 'u' | 'p' | 'R')) => o.ignored.push(format!("-{c}")),
            Long("text") => text = true,
            Value(value) => words.push(string(Ok(value))?),
            _ => return Err(arg.unexpected().to_string()),
        }
    }
    match SchemaCommand::parse(&words, text) {
        Some(command) => {
            if o.query.is_some() || !o.inputs.is_empty() {
                return Err(
                    "a schema command runs its own SQL; give it without -Q or -i".to_owned(),
                );
            }
            o.command = Some(command?);
        }
        None if text => return Err("--text only applies to sqlh find".to_owned()),
        None if words
            .first()
            .is_some_and(|w| w == "mcp" || w == "profiles" || w == "profile") =>
        {
            // `sqlh --profile x mcp`: the subcommand has to come first, or it would run as SQL.
            return Err(format!(
                "{} must come first: sqlh {} ...",
                words[0], words[0]
            ));
        }
        None => {
            for word in words {
                set_query(&mut o, word)?;
            }
        }
    }
    Ok(Command::Run(Box::new(o)))
}

fn string(value: Result<OsString, lexopt::Error>) -> Result<String, String> {
    value
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|v| format!("argument is not valid Unicode: {v:?}"))
}

/// A long flag that is on when given bare and can be set either way with `=`:
/// `--flag`, `--flag=true`, `--flag=false`. The `=false` form turns off, for one run, what a
/// profile saved.
fn switch(p: &mut lexopt::Parser, flag: &str) -> Result<bool, String> {
    match p.optional_value() {
        None => Ok(true),
        Some(v) => match string(Ok(v))?.to_ascii_lowercase().as_str() {
            "true" | "yes" => Ok(true),
            "false" | "no" => Ok(false),
            other => Err(format!("{flag}={other}: expected true or false")),
        },
    }
}

fn number<T: std::str::FromStr>(
    value: Result<OsString, lexopt::Error>,
    flag: &str,
) -> Result<T, String> {
    let text = string(value)?;
    text.parse()
        .map_err(|_| format!("{flag} {text}: expected a non-negative whole number"))
}

fn set_query(o: &mut Opts, query: String) -> Result<(), String> {
    if o.query.is_some() {
        return Err(format!(
            "unexpected argument {query:?}: SQL was already given (quote the whole query as one argument)"
        ));
    }
    o.query = Some(query);
    Ok(())
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn encrypt(value: &str) -> Result<Encrypt, String> {
    match value.to_ascii_lowercase().as_str() {
        "s" | "strict" => Ok(Encrypt::Strict),
        "m" | "mandatory" | "true" | "yes" => Ok(Encrypt::Mandatory),
        "o" | "optional" | "false" | "no" => Ok(Encrypt::Optional),
        _ => Err(format!(
            "encryption {value:?}: expected s(trict), m(andatory) or o(ptional)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Opts {
        match parse_args(args) {
            Ok(Command::Run(o)) => *o,
            Ok(_) => panic!("not a run command"),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn sqlcmd_style_flags() {
        let o = opts(&[
            "-S", "srv,1433", "-dAppDb", "-U", "reader", "-Q", "SELECT 1", "-v", "a=1", "b=x=y",
            "-Ns", "-C", "-t", "5",
        ]);
        assert_eq!(o.server.as_deref(), Some("srv,1433"));
        assert_eq!(o.database.as_deref(), Some("AppDb"));
        assert_eq!(o.user.as_deref(), Some("reader"));
        assert_eq!(o.query.as_deref(), Some("SELECT 1"));
        assert_eq!(
            o.vars,
            vec![("a".into(), "1".into()), ("b".into(), "x=y".into())]
        );
        assert_eq!(o.encrypt, Some(Encrypt::Strict));
        assert_eq!(o.trust_server_certificate, Some(true));
        assert_eq!(o.query_timeout, Some(5));
    }

    #[test]
    fn positional_query_after_vars() {
        let o = opts(&["-v", "db=x", "SELECT $(db)"]);
        assert_eq!(o.query.as_deref(), Some("SELECT $(db)"));
        assert_eq!(o.vars.len(), 1);
    }

    #[test]
    fn formatting_flags_are_ignored() {
        let o = opts(&["-W", "-s", ",", "-h", "-1", "-Q", "x"]);
        assert_eq!(o.ignored, vec!["-W", "-s", "-h"]);
    }

    #[test]
    fn password_flag_is_refused() {
        let err = parse_args(&["-U", "sa", "-P", "secret"]).err().unwrap();
        assert!(err.contains("SQLCMDPASSWORD") && !err.contains("secret"));
    }

    #[test]
    fn two_queries_are_an_error() {
        assert!(parse_args(&["-Q", "SELECT 1", "SELECT 2"]).is_err());
    }

    #[test]
    fn max_output_is_zero_or_at_least_the_minimum() {
        assert_eq!(opts(&["--max-output", "0", "x"]).max_output, Some(0));
        assert_eq!(opts(&["--max-output", "4096", "x"]).max_output, Some(4096));
        let err = parse_args(&["--max-output", "100", "x"]).err().unwrap();
        assert!(err.contains("at least 4096"), "{err}");
        assert!(parse_args(&["--max-output", "1MB", "x"]).is_err());
    }

    #[test]
    fn lock_timeout_is_seconds_and_zero_means_no_limit() {
        assert_eq!(opts(&["x"]).lock_timeout, None);
        assert_eq!(opts(&["--lock-timeout", "5", "x"]).lock_timeout, Some(5));
        assert_eq!(opts(&["--lock-timeout", "0", "x"]).lock_timeout, Some(0));
        assert_eq!(opts(&["--lock-timeout=30", "x"]).lock_timeout, Some(30));
        assert_eq!(
            opts(&["--lock-timeout", "2147483", "x"]).lock_timeout,
            Some(2_147_483)
        );
        let err = |args: &[&str]| parse_args(args).err().expect("an error");
        assert_eq!(
            err(&["--lock-timeout", "2147484", "x"]),
            "--lock-timeout 2147484: at most 2147483 seconds; use 0 for no limit"
        );
        for bad in ["abc", "-1", "1.5", "10s", ""] {
            assert_eq!(
                err(&["--lock-timeout", bad, "x"]),
                format!("--lock-timeout {bad}: expected a non-negative whole number"),
                "{bad:?}"
            );
        }
        assert!(err(&["--lock-timeout"]).contains("lock-timeout"));
    }

    // plan-history.md 2.15: `--exact-count` is a switch like the others.
    #[test]
    fn exact_count_is_a_switch() {
        assert_eq!(opts(&["x"]).exact_count, None);
        assert_eq!(opts(&["--exact-count", "x"]).exact_count, Some(true));
        assert_eq!(opts(&["--exact-count=no", "x"]).exact_count, Some(false));
        let o = opts(&["--exact-count", "SELECT 1"]);
        assert_eq!(o.exact_count, Some(true));
        assert_eq!(o.query.as_deref(), Some("SELECT 1"));
        assert_eq!(
            parse_args(&["--exact-count=maybe", "x"]).err().unwrap(),
            "--exact-count=maybe: expected true or false"
        );
    }

    #[test]
    fn read_uncommitted_is_a_switch() {
        assert_eq!(opts(&["x"]).read_uncommitted, None);
        assert_eq!(
            opts(&["--read-uncommitted", "x"]).read_uncommitted,
            Some(true)
        );
        for on in ["true", "TRUE", "yes"] {
            let flag = format!("--read-uncommitted={on}");
            assert_eq!(opts(&[&flag, "x"]).read_uncommitted, Some(true), "{on}");
        }
        for off in ["false", "No"] {
            let flag = format!("--read-uncommitted={off}");
            assert_eq!(opts(&[&flag, "x"]).read_uncommitted, Some(false), "{off}");
        }
        // The word after a bare flag is not its value.
        let o = opts(&["--read-uncommitted", "SELECT 1"]);
        assert_eq!(o.read_uncommitted, Some(true));
        assert_eq!(o.query.as_deref(), Some("SELECT 1"));
        assert_eq!(
            parse_args(&["--read-uncommitted=maybe", "x"])
                .err()
                .unwrap(),
            "--read-uncommitted=maybe: expected true or false"
        );
        // The older switch keeps its message.
        assert_eq!(
            parse_args(&["--trust-server-certificate=maybe", "x"])
                .err()
                .unwrap(),
            "--trust-server-certificate=maybe: expected true or false"
        );
        assert_eq!(
            opts(&["--trust-server-certificate=false", "x"]).trust_server_certificate,
            Some(false)
        );
    }

    #[test]
    fn profile_add_takes_the_lock_settings() {
        match parse_args(&[
            "profiles",
            "add",
            "dev",
            "-S",
            "srv",
            "--lock-timeout",
            "0",
            "--read-uncommitted=false",
        ]) {
            Ok(Command::Profile(ProfileCommand::Add { opts, .. })) => {
                assert_eq!(opts.lock_timeout, Some(0));
                assert_eq!(opts.read_uncommitted, Some(false));
            }
            _ => panic!("expected profiles add"),
        }
    }

    #[test]
    fn profile_commands() {
        match parse_args(&[
            "profiles",
            "add",
            "test",
            "-S",
            "srv",
            "-U",
            "me",
            "--write-policy",
            "deny",
        ]) {
            Ok(Command::Profile(ProfileCommand::Add { name, opts })) => {
                assert_eq!(name, "test");
                assert_eq!(opts.server.as_deref(), Some("srv"));
                assert_eq!(opts.write_policy, Some(WritePolicy::Deny));
            }
            _ => panic!("expected profiles add"),
        }
        assert!(matches!(
            parse_args(&["profiles", "list"]),
            Ok(Command::Profile(ProfileCommand::List { search: None }))
        ));
        // The older spelling still works (decision 37).
        assert!(matches!(
            parse_args(&["profile", "list"]),
            Ok(Command::Profile(ProfileCommand::List { search: None }))
        ));
        assert!(matches!(
            parse_args(&["profile", "test", "dev"]),
            Ok(Command::Profile(ProfileCommand::Test { .. }))
        ));
        assert!(matches!(
            parse_args(&["profiles", "test", "dev"]),
            Ok(Command::Profile(ProfileCommand::Test { .. }))
        ));
        assert!(parse_args(&["profiles", "add", "bad.name"]).is_err());
        assert!(parse_args(&["profiles", "add", "x", "-Q", "SELECT 1"]).is_err());
        assert!(parse_args(&["profiles", "test", "dev", "extra"]).is_err());
        assert!(parse_args(&["profile"]).is_err());
    }

    // schema-commands.md §2: the first positional word names a schema command, wherever the
    // options are; anything else is SQL, as before. A command can't come with -Q or -i, and
    // --text is for find only.
    #[test]
    fn schema_commands_are_the_first_word() {
        let o = opts(&["--profile", "dev", "describe", "dbo.Orders", "-d", "AppDb"]);
        assert_eq!(
            o.command,
            Some(SchemaCommand::Describe("dbo.Orders".into()))
        );
        assert_eq!((o.query, o.database.as_deref()), (None, Some("AppDb")));
        let o = opts(&["-v", "a=1", "find", "Id", "--text"]);
        assert_eq!(
            o.command,
            Some(SchemaCommand::Find {
                pattern: "Id".into(),
                text: true
            })
        );
        assert_eq!(o.vars, vec![("a".into(), "1".into())]);
        // Not a command: SQL.
        let o = opts(&["SELECT 1"]);
        assert_eq!((o.command, o.query.as_deref()), (None, Some("SELECT 1")));
        for (args, wanted) in [
            (
                &["-Q", "SELECT 1", "tables"][..],
                "give it without -Q or -i",
            ),
            (&["-i", "x.sql", "dbs"][..], "give it without -Q or -i"),
            (
                &["--text", "SELECT 1"][..],
                "--text only applies to sqlh find",
            ),
            (&["describe"][..], "describe needs NAME"),
            (&["tables", "a", "b"][..], "tables takes one pattern"),
            (&["SELECT 1", "SELECT 2"][..], "SQL was already given"),
            (
                &["profiles", "add", "x", "describe", "y"][..],
                "connection options only",
            ),
        ] {
            let error = parse_args(args).err().expect("refused");
            assert!(error.contains(wanted), "{args:?}: {error}");
        }
    }

    // plan-history.md 4.1 and 4.2: `profiles add` takes --description; `profiles list` takes
    // --search, and nothing else.
    #[test]
    fn profile_descriptions_and_search() {
        match parse_args(&[
            "profiles",
            "add",
            "dev",
            "--description",
            "prod replica, read-only",
        ]) {
            Ok(Command::Profile(ProfileCommand::Add { opts, .. })) => {
                assert_eq!(opts.description.as_deref(), Some("prod replica, read-only"));
            }
            _ => panic!("expected profiles add"),
        }
        match parse_args(&["profile", "ls", "--search", "Prod"]) {
            Ok(Command::Profile(ProfileCommand::List { search })) => {
                assert_eq!(search.as_deref(), Some("Prod"));
            }
            _ => panic!("expected profiles list"),
        }
        // The list is text only (owner, 2026-10-13): --format is refused like any other flag.
        let only = "profiles list takes --search TEXT only (its output is text)";
        for (args, wanted) in [
            (vec!["profiles", "list", "-S", "srv"], only),
            (vec!["profiles", "list", "prod"], only),
            (vec!["profiles", "list", "--format", "json"], only),
            (vec!["profiles", "list", "--search"], "missing argument"),
        ] {
            let error = parse_args(&args).err().expect("refused");
            assert!(error.contains(wanted), "{args:?}: {error}");
        }
    }
}
