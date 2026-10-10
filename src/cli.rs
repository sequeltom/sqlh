//! Arguments: sqlcmd-compatible short flags plus our long flags (DESIGN.md §4.2), and the
//! `profile` subcommands. Settings a profile can supply are `Option`s so config.rs can tell
//! "not given" from "given".

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::{WritePolicy, valid_name};
use crate::db::Encrypt;
use crate::output::{Format, MIN_MAX_OUTPUT};

pub enum Command {
    Run(Box<Opts>),
    Profile(ProfileCommand),
    Help,
    Version,
}

pub enum ProfileCommand {
    Add { name: String, opts: Box<Opts> },
    List,
    Test { name: String },
    Remove { name: String },
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
    pub check: bool,
    pub continue_on_error: bool,
    pub verbose: bool,
    /// Only for `sqlh profile add`.
    pub write_policy: Option<WritePolicy>,
    pub format: Option<Format>,
    /// Only for `sqlh profile add`: prompt for a new password even if one is stored.
    pub new_password: bool,
    /// Internal (set by `profile test`): print a `-- note:` describing the connection.
    pub show_connection: bool,
    /// sqlcmd flags accepted for compatibility but without effect.
    pub ignored: Vec<String>,
}

pub fn parse() -> Result<Command, String> {
    parse_all(std::env::args_os().skip(1).collect())
}

fn parse_all(args: Vec<OsString>) -> Result<Command, String> {
    if args.first().is_some_and(|a| a == "profile") {
        return parse_profile(args.into_iter().skip(1).collect());
    }
    parse_from(lexopt::Parser::from_args(args))
}

#[cfg(test)]
fn parse_args(args: &[&str]) -> Result<Command, String> {
    parse_all(args.iter().map(OsString::from).collect())
}

const PROFILE_USAGE: &str = "usage: sqlh profile add NAME -S server [-d db] [-U login] [options] | list | test NAME | rm NAME";

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
            if opts.query.is_some() || !opts.inputs.is_empty() {
                return Err("profile add takes connection options only, not SQL".into());
            }
            ProfileCommand::Add { name, opts }
        }
        "list" | "ls" => ProfileCommand::List,
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

fn parse_from(mut p: lexopt::Parser) -> Result<Command, String> {
    use lexopt::prelude::*;
    let mut o = Opts::default();
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
                return Err("-P is refused: a password on the command line leaks into transcripts and process lists. Save a profile (sqlh profile add) or set SQLCMDPASSWORD".into());
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
                        _ => set_query(&mut o, value)?,
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
                o.trust_server_certificate = Some(match p.optional_value() {
                    None => true,
                    Some(v) => match string(Ok(v))?.to_ascii_lowercase().as_str() {
                        "true" | "yes" => true,
                        "false" | "no" => false,
                        other => {
                            return Err(format!(
                                "--trust-server-certificate={other}: expected true or false"
                            ));
                        }
                    },
                })
            }
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
            Long("format") => {
                let format = string(p.value())?;
                o.format = Some(Format::parse(&format).ok_or_else(|| match format.as_str() {
                    "md" => "--format md is not implemented yet; use text, json or csv".to_owned(),
                    _ => format!("--format {format}: expected text, json or csv"),
                })?);
            }
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
            Value(value) => set_query(&mut o, string(Ok(value))?)?,
            _ => return Err(arg.unexpected().to_string()),
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
    fn profile_commands() {
        match parse_args(&[
            "profile",
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
            _ => panic!("expected profile add"),
        }
        assert!(matches!(
            parse_args(&["profile", "list"]),
            Ok(Command::Profile(ProfileCommand::List))
        ));
        assert!(matches!(
            parse_args(&["profile", "test", "dev"]),
            Ok(Command::Profile(ProfileCommand::Test { .. }))
        ));
        assert!(parse_args(&["profile", "add", "bad.name"]).is_err());
        assert!(parse_args(&["profile", "add", "x", "-Q", "SELECT 1"]).is_err());
        assert!(parse_args(&["profile", "test", "dev", "extra"]).is_err());
        assert!(parse_args(&["profile"]).is_err());
    }
}
