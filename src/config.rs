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
use crate::console;
use crate::credman;
use crate::db::{Auth, ConnectOptions, Encrypt, check_server};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Windows,
    Sql,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::Windows => "windows",
            AuthKind::Sql => "sql",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub name: String,
    pub server: Option<String>,
    pub database: Option<String>,
    /// `auth` as written. Older files may omit it; then `user` decides (see `auth_kind`).
    pub auth: Option<AuthKind>,
    /// The SQL login; must be set exactly when the auth is SQL.
    pub user: Option<String>,
    pub encrypt: Option<Encrypt>,
    pub trust_server_certificate: Option<bool>,
    pub write_policy: Option<WritePolicy>,
    pub login_timeout: Option<u32>,
    pub query_timeout: Option<u32>,
    pub max_rows: Option<u64>,
    pub max_cell: Option<usize>,
}

impl Profile {
    pub fn auth_kind(&self) -> AuthKind {
        self.auth.unwrap_or(if self.user.is_some() {
            AuthKind::Sql
        } else {
            AuthKind::Windows
        })
    }
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
        let path = PathBuf::from(path);
        // A Git Bash path (`/c/Users/me/profiles.toml`) in cmd.exe or PowerShell names a
        // folder on the current drive, which usually doesn't exist. A missing file is just
        // "no profiles yet", and saving would create the folder, so refuse it here instead
        // (DESIGN.md §4.2).
        let hint = path.to_str().and_then(console::git_bash_hint);
        if let Some(hint) = hint
            && path.parent().is_some_and(|folder| !folder.is_dir())
        {
            return Err(format!(
                "SQLH_CONFIG {}: the folder wasn't found; {hint}",
                path.display()
            ));
        }
        return Ok(path);
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

/// What settings resolution reads from the machine, so tests can supply their own.
pub struct Sources<'a> {
    /// An environment variable, or `None` when unset or empty.
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// The profiles file.
    pub load: &'a dyn Fn() -> Result<Config, String>,
    /// A profile's stored password.
    pub password: &'a dyn Fn(&str) -> std::io::Result<Option<String>>,
}

pub fn resolve(opts: &Opts) -> Result<Settings, Failure> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    resolve_with(
        opts,
        &Sources {
            var: &var,
            load: &load,
            password: &credman::read,
        },
    )
}

pub fn resolve_with(opts: &Opts, sources: &Sources) -> Result<Settings, Failure> {
    let env = sources.var;
    let named = opts.profile.clone().or_else(|| env("SQLH_PROFILE"));
    let server_given = opts.server.is_some() || env("SQLCMDSERVER").is_some();
    let config = if named.is_some() || !server_given {
        (sources.load)().map_err(Failure::usage)?
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
    // Where the server came from, for the message when it is refused.
    let (server, origin) = if let Some(server) = &opts.server {
        (server.clone(), "-S".to_owned())
    } else if let Some(server) = &p.server {
        (server.clone(), format!("profile {} server", p.name))
    } else if let Some(server) = env("SQLCMDSERVER") {
        (server, "SQLCMDSERVER".to_owned())
    } else {
        (".".to_owned(), "-S".to_owned())
    };
    // A Named Pipes request to another machine can't be timed out or cancelled, so it is
    // refused here, whichever way the server was given (DESIGN.md §4.2).
    check_server(&server).map_err(|reason| Failure::usage(format!("{origin} {reason}")))?;

    // Decide the auth method first, with the same precedence as everything else, then the
    // login. A Windows-auth profile stays Windows auth whatever SQLCMDUSER says.
    let kind = if opts.integrated {
        AuthKind::Windows
    } else if opts.user.is_some() {
        AuthKind::Sql
    } else if let Some(profile) = &profile {
        profile.auth_kind()
    } else if env("SQLCMDUSER").is_some() {
        AuthKind::Sql
    } else {
        AuthKind::Windows
    };
    let auth = match kind {
        AuthKind::Windows => Auth::Integrated,
        AuthKind::Sql => {
            let user = opts
                .user
                .clone()
                .or(p.user.clone())
                .or_else(|| env("SQLCMDUSER"))
                .ok_or_else(|| Failure::usage("SQL auth needs a login: pass -U"))?;
            Auth::Sql {
                password: password(sources, profile.as_ref(), &server, &user)?,
                user,
            }
        }
    };

    let policy = p.write_policy.unwrap_or(WritePolicy::Flag);
    if opts.allow_write && policy == WritePolicy::Deny {
        return Err(Failure::usage(format!(
            "profile {} has write_policy = \"deny\", so --allow-write is refused",
            p.name
        )));
    }
    let defaults = Limits::default();
    let query_timeout = opts.query_timeout.or(p.query_timeout).unwrap_or(60);
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
            trust_server_certificate: opts
                .trust_server_certificate
                .or(p.trust_server_certificate)
                .unwrap_or(false),
            login_timeout: opts.login_timeout.or(p.login_timeout).unwrap_or(15),
            read_only_intent: opts.read_only_intent,
            multi_subnet_failover: opts.multi_subnet_failover,
        },
        limits: Limits {
            max_rows: opts.max_rows.or(p.max_rows).unwrap_or(defaults.max_rows),
            max_cell: opts.max_cell.or(p.max_cell).unwrap_or(defaults.max_cell),
            max_results: opts.max_results.unwrap_or(defaults.max_results),
            max_messages: defaults.max_messages,
            max_output: opts.max_output.unwrap_or(defaults.max_output),
            stall_secs: query_timeout,
        },
        query_timeout,
        allow_write: opts.allow_write || policy == WritePolicy::Allow,
        profile: profile.map(|p| p.name),
    })
}

/// The profile's stored credential, else SQLCMDPASSWORD. A stored password is only ever sent
/// to the profile's own server and login: `--profile x -S elsewhere` must not leak it.
fn password(
    sources: &Sources,
    profile: Option<&Profile>,
    server: &str,
    user: &str,
) -> Result<String, Failure> {
    let owns = |p: &Profile| {
        p.user.as_deref() == Some(user)
            && p.server
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(server))
    };
    if let Some(p) = profile.filter(|p| owns(p)) {
        match (sources.password)(&p.name) {
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
    (sources.var)("SQLCMDPASSWORD").ok_or_else(|| match profile {
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
    // Keys seen in the current table, so a repeated key is an error instead of last-wins.
    let mut seen: Vec<String> = Vec::new();
    // Line of each profile's header, for errors that span several keys.
    let mut header_lines: Vec<usize> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let err = |message: String| format!("{line_no}: {message}");
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            seen.clear();
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
            header_lines.push(line_no);
            current = Some(config.profiles.len() - 1);
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| err("expected key = value".into()))?;
        let key = key.trim();
        if seen.iter().any(|k| k == key) {
            return Err(err(format!("{key} is set twice")));
        }
        seen.push(key.to_owned());
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
            "auth" => {
                p.auth = Some(match value.string(key).map_err(err)?.as_str() {
                    "sql" => AuthKind::Sql,
                    "windows" => AuthKind::Windows,
                    other => {
                        return Err(err(format!(
                            "auth {other:?}: expected \"sql\" or \"windows\""
                        )));
                    }
                })
            }
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
    for (p, line) in config.profiles.iter().zip(&header_lines) {
        match (p.auth, &p.user) {
            (Some(AuthKind::Windows), Some(_)) => {
                return Err(format!(
                    "{line}: [profiles.{}] has auth = \"windows\" but also a user; remove one",
                    p.name
                ));
            }
            (Some(AuthKind::Sql), None) => {
                return Err(format!(
                    "{line}: [profiles.{}] has auth = \"sql\" but no user",
                    p.name
                ));
            }
            _ => {}
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
        string("auth", &Some(p.auth_kind().as_str().to_owned()));
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
                    auth: Some(AuthKind::Sql),
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
                    auth: Some(AuthKind::Windows),
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

    #[test]
    fn duplicate_and_contradictory_keys_are_errors() {
        assert_eq!(
            parse("[profiles.a]\nserver = \"one\"\nserver = \"two\"").unwrap_err(),
            "3: server is set twice"
        );
        assert_eq!(
            parse("default = \"a\"\ndefault = \"a\"\n[profiles.a]").unwrap_err(),
            "2: default is set twice"
        );
        // The same key in different tables is fine.
        assert!(parse("[profiles.a]\nserver = \"x\"\n[profiles.b]\nserver = \"y\"").is_ok());
        assert!(
            parse("[profiles.a]\nauth = \"windows\"\nuser = \"bob\"")
                .unwrap_err()
                .contains("auth = \"windows\" but also a user")
        );
        assert!(
            parse("[profiles.a]\nauth = \"sql\"")
                .unwrap_err()
                .contains("auth = \"sql\" but no user")
        );
    }

    // F09: resolution precedence, with the machine's environment and stores injected.
    mod precedence {
        use super::*;
        use crate::cli::Opts;

        fn resolve_in(opts: &Opts, file: &str, env: &[(&str, &str)]) -> Result<Settings, Failure> {
            let config = parse(file).unwrap();
            let var = |name: &str| {
                env.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            };
            let load = || {
                Ok(Config {
                    default: config.default.clone(),
                    profiles: config.profiles.clone(),
                })
            };
            let password = |profile: &str| Ok((profile == "sqlp").then(|| "stored".to_owned()));
            resolve_with(
                opts,
                &Sources {
                    var: &var,
                    load: &load,
                    password: &password,
                },
            )
        }

        fn login(settings: &Settings) -> Option<String> {
            match &settings.connect.auth {
                Auth::Integrated => None,
                Auth::Sql { user, .. } => Some(user.clone()),
            }
        }

        const FILE: &str = "[profiles.win]\nserver = \"srv\"\nauth = \"windows\"\n\
                            [profiles.legacy]\nserver = \"srv\"\n\
                            [profiles.sqlp]\nserver = \"srv\"\nauth = \"sql\"\nuser = \"tom\"\ntrust_server_certificate = true\n";

        fn with_profile(name: &str) -> Opts {
            Opts {
                profile: Some(name.into()),
                ..Opts::default()
            }
        }

        #[test]
        fn a_windows_profile_ignores_sqlcmduser() {
            let env = [
                ("SQLCMDUSER", "environment_login"),
                ("SQLCMDPASSWORD", "pw"),
            ];
            for name in ["win", "legacy"] {
                let settings = resolve_in(&with_profile(name), FILE, &env).unwrap();
                assert_eq!(login(&settings), None, "profile {name}");
            }
        }

        #[test]
        fn flags_beat_the_profile_and_the_profile_beats_the_environment() {
            let env = [
                ("SQLCMDUSER", "environment_login"),
                ("SQLCMDPASSWORD", "pw"),
            ];
            let settings = resolve_in(&with_profile("sqlp"), FILE, &env).unwrap();
            assert_eq!(login(&settings).as_deref(), Some("tom"));
            let opts = Opts {
                user: Some("flag_login".into()),
                ..with_profile("win")
            };
            assert_eq!(
                login(&resolve_in(&opts, FILE, &env).unwrap()).as_deref(),
                Some("flag_login")
            );
            let opts = Opts {
                integrated: true,
                ..with_profile("sqlp")
            };
            assert_eq!(login(&resolve_in(&opts, FILE, &env).unwrap()), None);
            // No profile at all: the environment decides.
            let opts = Opts {
                server: Some("srv".into()),
                ..Opts::default()
            };
            assert_eq!(
                login(&resolve_in(&opts, FILE, &env).unwrap()).as_deref(),
                Some("environment_login")
            );
        }

        // DESIGN.md §4.2: a Named Pipes request to another machine is refused whichever way
        // the server is given, and the message says which. Only the server that would be
        // used counts: a flag replaces a profile's.
        #[test]
        fn a_remote_named_pipe_is_refused_from_every_source() {
            use crate::error::Exit;
            let file = "[profiles.old]\nserver = \"np:db01\"\nauth = \"windows\"\n";
            let refused = |opts: &Opts, env: &[(&str, &str)]| {
                let failure = resolve_in(opts, file, env).err().expect("refused");
                assert_eq!(failure.exit, Exit::Usage);
                failure.message
            };
            let flag = Opts {
                server: Some(r"\\db01\pipe\sql\query".into()),
                ..Opts::default()
            };
            assert!(
                refused(&flag, &[])
                    .starts_with(r"-S \\db01\pipe\sql\query: Named Pipes to a remote"),
            );
            let message = refused(&with_profile("old"), &[]);
            assert!(
                message.starts_with("profile old server np:db01: Named Pipes to a remote"),
                "{message}"
            );
            assert!(message.ends_with("connect over TCP: -S db01"), "{message}");
            let message = refused(&Opts::default(), &[("SQLCMDSERVER", "np:db01")]);
            assert!(
                message.starts_with("SQLCMDSERVER np:db01: Named Pipes to a remote"),
                "{message}"
            );

            let replaced = Opts {
                server: Some("db01".into()),
                ..with_profile("old")
            };
            let settings = resolve_in(&replaced, file, &[]).unwrap();
            assert_eq!(settings.connect.server, "db01");
            for local in ["np:.", r"\\.\pipe\sql\query", "lpc:.", "tcp:db01"] {
                let opts = Opts {
                    server: Some(local.into()),
                    ..Opts::default()
                };
                assert!(resolve_in(&opts, file, &[]).is_ok(), "{local}");
            }
        }

        #[test]
        fn a_saved_trust_can_be_turned_off() {
            let settings = resolve_in(&with_profile("sqlp"), FILE, &[]).unwrap();
            assert!(settings.connect.trust_server_certificate);
            let opts = Opts {
                trust_server_certificate: Some(false),
                ..with_profile("sqlp")
            };
            assert!(
                !resolve_in(&opts, FILE, &[])
                    .unwrap()
                    .connect
                    .trust_server_certificate
            );
        }
    }
}
