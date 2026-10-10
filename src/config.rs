//! Connection profiles (DESIGN.md §8) and settings resolution.
//!
//! The file is `%APPDATA%\sqlharness\profiles.toml` (override: `SQLH_CONFIG`), read with a
//! small strict parser for the TOML subset we write: a top-level `default`, `[profiles.NAME]`
//! tables, and string / integer / boolean values. Passwords are never in this file.
//!
//! Precedence: flags > profile > SQLCMD* environment variables > defaults. The profile is
//! `--profile`, else `SQLH_PROFILE`, else the file's `default` when no server was given.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::cli::Opts;
use crate::credman;
use crate::db::{Auth, ConnectOptions, Encrypt};
use crate::error::Failure;
use crate::output::Limits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePolicy {
    /// Never write; `--allow-write` is refused.
    Deny,
    /// Writes need `--allow-write` (default).
    Flag,
    /// Writes allowed without the flag.
    Allow,
}

impl WritePolicy {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "deny" => Some(WritePolicy::Deny),
            "flag" => Some(WritePolicy::Flag),
            "allow" => Some(WritePolicy::Allow),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WritePolicy::Deny => "deny",
            WritePolicy::Flag => "flag",
            WritePolicy::Allow => "allow",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub name: String,
    pub server: Option<String>,
    pub database: Option<String>,
    /// Set means SQL auth; unset means Windows auth.
    pub user: Option<String>,
    pub encrypt: Option<Encrypt>,
    pub trust_server_certificate: Option<bool>,
    pub write_policy: Option<WritePolicy>,
    pub login_timeout: Option<u32>,
    pub query_timeout: Option<u32>,
    pub max_rows: Option<u64>,
    pub max_cell: Option<usize>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub default: Option<String>,
    pub profiles: Vec<Profile>,
}

impl Config {
    pub fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn upsert(&mut self, profile: Profile) {
        match self
            .profiles
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&profile.name))
        {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|p| !p.name.eq_ignore_ascii_case(name));
        if self
            .default
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(name))
        {
            self.default = None;
        }
        self.profiles.len() != before
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SQLH_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let appdata = std::env::var_os("APPDATA")
        .filter(|p| !p.is_empty())
        .ok_or("APPDATA is not set; set SQLH_CONFIG to the profiles file path")?;
    Ok(PathBuf::from(appdata)
        .join("sqlharness")
        .join("profiles.toml"))
}

/// Loads the profiles file; a missing file is an empty config.
pub fn load() -> Result<Config, String> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}:{e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn save(config: &Config) -> Result<PathBuf, String> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, render(config)).map_err(|e| format!("{}: {e}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Everything a run needs, after merging flags, profile, environment and defaults.
pub struct Settings {
    pub connect: ConnectOptions,
    pub limits: Limits,
    pub query_timeout: u32,
    pub allow_write: bool,
    pub profile: Option<String>,
}

pub fn resolve(opts: &Opts) -> Result<Settings, Failure> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let named = opts.profile.clone().or_else(|| env("SQLH_PROFILE"));
    let server_given = opts.server.is_some() || env("SQLCMDSERVER").is_some();
    let config = if named.is_some() || !server_given {
        load().map_err(Failure::usage)?
    } else {
        Config::default()
    };
    let profile = match named.or_else(|| {
        if server_given {
            None
        } else {
            config.default.clone()
        }
    }) {
        Some(name) => Some(config.find(&name).cloned().ok_or_else(|| {
            Failure::usage(format!("no profile named {name:?}; see sqlh profile list"))
        })?),
        None => None,
    };
    let p = profile.clone().unwrap_or_default();
    let server = opts
        .server
        .clone()
        .or(p.server.clone())
        .or_else(|| env("SQLCMDSERVER"))
        .unwrap_or_else(|| ".".into());

    let user = if opts.integrated {
        None
    } else {
        opts.user
            .clone()
            .or(p.user.clone())
            .or_else(|| env("SQLCMDUSER"))
    };
    let auth = match user {
        None => Auth::Integrated,
        Some(user) => Auth::Sql {
            password: password(profile.as_ref(), &server, &user)?,
            user,
        },
    };

    let policy = p.write_policy.unwrap_or(WritePolicy::Flag);
    if opts.allow_write && policy == WritePolicy::Deny {
        return Err(Failure::usage(format!(
            "profile {} has write_policy = \"deny\", so --allow-write is refused",
            p.name
        )));
    }
    let defaults = Limits::default();
    Ok(Settings {
        connect: ConnectOptions {
            server,
            database: opts
                .database
                .clone()
                .or(p.database.clone())
                .or_else(|| env("SQLCMDDBNAME")),
            auth,
            encrypt: opts.encrypt.or(p.encrypt).unwrap_or(Encrypt::Mandatory),
            trust_server_certificate: opts.trust_server_certificate
                || p.trust_server_certificate.unwrap_or(false),
            login_timeout: opts.login_timeout.or(p.login_timeout).unwrap_or(15),
            read_only_intent: opts.read_only_intent,
            multi_subnet_failover: opts.multi_subnet_failover,
        },
        limits: Limits {
            max_rows: opts.max_rows.or(p.max_rows).unwrap_or(defaults.max_rows),
            max_cell: opts.max_cell.or(p.max_cell).unwrap_or(defaults.max_cell),
            max_results: opts.max_results.unwrap_or(defaults.max_results),
            max_messages: defaults.max_messages,
        },
        query_timeout: opts.query_timeout.or(p.query_timeout).unwrap_or(60),
        allow_write: opts.allow_write || policy == WritePolicy::Allow,
        profile: profile.map(|p| p.name),
    })
}

/// The profile's stored credential, else SQLCMDPASSWORD. A stored password is only ever sent
/// to the profile's own server and login: `--profile x -S elsewhere` must not leak it.
fn password(profile: Option<&Profile>, server: &str, user: &str) -> Result<String, Failure> {
    let owns = |p: &Profile| {
        p.user.as_deref() == Some(user)
            && p.server
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(server))
    };
    if let Some(p) = profile.filter(|p| owns(p)) {
        match credman::read(&p.name) {
            Ok(Some(password)) => return Ok(password),
            Ok(None) => {}
            Err(e) => {
                return Err(Failure::usage(format!(
                    "reading the password for profile {} from Credential Manager: {e}",
                    p.name
                )));
            }
        }
    }
    std::env::var("SQLCMDPASSWORD")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| match profile {
            Some(p) if owns(p) => Failure::usage(format!(
                "no password stored for profile {0}; run: sqlh profile add {0}",
                p.name
            )),
            Some(p) if p.user.is_some() => Failure::usage(format!(
                "profile {} keeps its password for {} as {} only, and never sends it to another \
                 server or login; drop -S/-U, or set SQLCMDPASSWORD",
                p.name,
                p.server.as_deref().unwrap_or("?"),
                p.user.as_deref().unwrap_or("?")
            )),
            _ => Failure::usage(format!(
                "SQL login {user} needs a password: save a profile (sqlh profile add NAME -S server -U {user}) or set SQLCMDPASSWORD"
            )),
        })
}

fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config::default();
    let mut current: Option<usize> = None;
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let err = |message: String| format!("{line_no}: {message}");
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let header = header
                .strip_suffix(']')
                .ok_or_else(|| err("expected ] at the end of the table header".into()))?
                .trim();
            let name = header.strip_prefix("profiles.").ok_or_else(|| {
                err(format!(
                    "unknown table [{header}]; expected [profiles.NAME]"
                ))
            })?;
            let name = name.trim_matches('"');
            if !valid_name(name) {
                return Err(err(format!(
                    "profile name {name:?} may only use letters, digits, _ and -"
                )));
            }
            if config.find(name).is_some() {
                return Err(err(format!("profile {name} is defined twice")));
            }
            config.profiles.push(Profile {
                name: name.to_owned(),
                ..Profile::default()
            });
            current = Some(config.profiles.len() - 1);
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| err("expected key = value".into()))?;
        let key = key.trim();
        let value = Value::parse(value.trim()).map_err(err)?;
        let Some(index) = current else {
            match key {
                "default" => config.default = Some(value.string(key).map_err(err)?),
                _ => {
                    return Err(err(format!(
                        "unknown top-level key {key}; expected default"
                    )));
                }
            }
            continue;
        };
        let p = &mut config.profiles[index];
        match key {
            "server" => p.server = Some(value.string(key).map_err(err)?),
            "database" => p.database = Some(value.string(key).map_err(err)?),
            "user" => p.user = Some(value.string(key).map_err(err)?),
            "auth" => match value.string(key).map_err(err)?.as_str() {
                "sql" | "windows" => {} // implied by `user`; kept for readability
                other => {
                    return Err(err(format!(
                        "auth {other:?}: expected \"sql\" or \"windows\""
                    )));
                }
            },
            "encrypt" => {
                p.encrypt = Some(match value.string(key).map_err(err)?.as_str() {
                    "optional" => Encrypt::Optional,
                    "mandatory" => Encrypt::Mandatory,
                    "strict" => Encrypt::Strict,
                    other => {
                        return Err(err(format!(
                            "encrypt {other:?}: expected optional, mandatory or strict"
                        )));
                    }
                })
            }
            "trust_server_certificate" => {
                p.trust_server_certificate = Some(value.boolean(key).map_err(err)?)
            }
            "write_policy" => {
                let text = value.string(key).map_err(err)?;
                p.write_policy = Some(WritePolicy::parse(&text).ok_or_else(|| {
                    err(format!(
                        "write_policy {text:?}: expected deny, flag or allow"
                    ))
                })?);
            }
            "login_timeout" => p.login_timeout = Some(value.integer(key).map_err(err)?),
            "query_timeout" => p.query_timeout = Some(value.integer(key).map_err(err)?),
            "max_rows" => p.max_rows = Some(value.integer(key).map_err(err)?),
            "max_cell" => p.max_cell = Some(value.integer(key).map_err(err)?),
            _ => return Err(err(format!("unknown key {key} in [profiles.{}]", p.name))),
        }
    }
    if let Some(default) = &config.default
        && config.find(default).is_none()
    {
        return Err(format!("0: default = {default:?} names no profile"));
    }
    Ok(config)
}

fn render(config: &Config) -> String {
    let mut out = String::from(
        "# sqlh connection profiles. Passwords are not stored here: they are in Windows\n\
         # Credential Manager as \"sqlharness:<profile>\". `sqlh profile add` rewrites this file.\n",
    );
    if let Some(default) = &config.default {
        let _ = writeln!(out, "default = {}", quote(default));
    }
    for p in &config.profiles {
        let _ = writeln!(out, "\n[profiles.{}]", p.name);
        let mut string = |key: &str, value: &Option<String>| {
            if let Some(v) = value {
                let _ = writeln!(out, "{key} = {}", quote(v));
            }
        };
        string("server", &p.server);
        string("database", &p.database);
        string(
            "auth",
            &Some(if p.user.is_some() { "sql" } else { "windows" }.to_owned()),
        );
        string("user", &p.user);
        string(
            "encrypt",
            &p.encrypt.map(|e| {
                match e {
                    Encrypt::Optional => "optional",
                    Encrypt::Mandatory => "mandatory",
                    Encrypt::Strict => "strict",
                }
                .to_owned()
            }),
        );
        string(
            "write_policy",
            &p.write_policy.map(|w| w.as_str().to_owned()),
        );
        if let Some(v) = p.trust_server_certificate {
            let _ = writeln!(out, "trust_server_certificate = {v}");
        }
        for (key, value) in [
            ("login_timeout", p.login_timeout.map(u64::from)),
            ("query_timeout", p.query_timeout.map(u64::from)),
            ("max_rows", p.max_rows),
            ("max_cell", p.max_cell.map(|v| v as u64)),
        ] {
            if let Some(v) = value {
                let _ = writeln!(out, "{key} = {v}");
            }
        }
    }
    out
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Drops a `#` comment that isn't inside a string.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

enum Value {
    Str(String),
    Int(u64),
    Bool(bool),
}

impl Value {
    fn parse(text: &str) -> Result<Value, String> {
        if let Some(body) = text.strip_prefix('"') {
            let body = body
                .strip_suffix('"')
                .ok_or_else(|| format!("unterminated string {text}"))?;
            let mut out = String::new();
            let mut chars = body.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    other => return Err(format!("unsupported escape \\{}", other.unwrap_or(' '))),
                }
            }
            return Ok(Value::Str(out));
        }
        match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => text.parse().map(Value::Int).map_err(|_| {
                format!("unsupported value {text}; use \"text\", a whole number, true or false")
            }),
        }
    }

    fn string(self, key: &str) -> Result<String, String> {
        match self {
            Value::Str(s) => Ok(s),
            _ => Err(format!("{key} must be a \"string\"")),
        }
    }

    fn boolean(self, key: &str) -> Result<bool, String> {
        match self {
            Value::Bool(b) => Ok(b),
            _ => Err(format!("{key} must be true or false")),
        }
    }

    fn integer<T: TryFrom<u64>>(self, key: &str) -> Result<T, String> {
        match self {
            Value::Int(n) => T::try_from(n).map_err(|_| format!("{key} is out of range")),
            _ => Err(format!("{key} must be a whole number")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let config = Config {
            default: Some("dev".into()),
            profiles: vec![
                Profile {
                    name: "dev".into(),
                    server: Some(r"srv\inst".into()),
                    database: Some("App \"Db\"".into()),
                    user: Some("reader".into()),
                    encrypt: Some(Encrypt::Strict),
                    trust_server_certificate: Some(true),
                    write_policy: Some(WritePolicy::Deny),
                    login_timeout: Some(5),
                    query_timeout: Some(120),
                    max_rows: Some(50),
                    max_cell: Some(1000),
                },
                Profile {
                    name: "local".into(),
                    server: Some(".".into()),
                    ..Profile::default()
                },
            ],
        };
        let text = render(&config);
        assert_eq!(parse(&text).unwrap(), config, "{text}");
    }

    #[test]
    fn parses_hand_written_files() {
        let text = "# mine\ndefault = \"a\"  # trailing\n\n[profiles.a]\nserver = \"x#1\"\nauth = \"windows\"\nmax_rows = 10\n";
        let config = parse(text).unwrap();
        assert_eq!(config.find("A").unwrap().server.as_deref(), Some("x#1"));
        assert_eq!(config.find("a").unwrap().max_rows, Some(10));
    }

    #[test]
    fn errors_name_the_line() {
        assert_eq!(
            parse("[profiles.a]\nservr = \"x\"").unwrap_err(),
            "2: unknown key servr in [profiles.a]"
        );
        assert_eq!(
            parse("[other]").unwrap_err(),
            "1: unknown table [other]; expected [profiles.NAME]"
        );
        assert_eq!(
            parse("[profiles.a]\nmax_rows = \"10\"").unwrap_err(),
            "2: max_rows must be a whole number"
        );
        assert!(
            parse("default = \"nope\"")
                .unwrap_err()
                .contains("names no profile")
        );
        assert!(
            parse("[profiles.a]\n[profiles.A]")
                .unwrap_err()
                .contains("defined twice")
        );
    }
}
