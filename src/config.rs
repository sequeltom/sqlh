//! Connection profiles (DESIGN.md §8) and settings resolution.
//!
//! The file is `%APPDATA%\sqlh\profiles.toml` (override: `SQLH_CONFIG`), read with a
//! small strict parser for the TOML subset we write: a top-level `default`, `[profiles.NAME]`
//! tables, and string / integer / boolean values. Passwords are never in this file.
//!
//! Precedence: flags > profile > SQLCMD* environment variables > defaults. The profile is
//! `--profile`, else `SQLH_PROFILE`, else the file's `default` when no server was given.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cli::Opts;
use crate::console;
use crate::credman;
use crate::db::{Auth, ConnectOptions, Encrypt, check_server};
use crate::error::Failure;
use crate::files;
use crate::output::Limits;
use crate::secret::Secret;

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

/// How long a statement waits for a lock by default, in seconds (DESIGN.md §4.3). Long
/// enough for an ordinary short blocker, short enough to fail well inside `-t` so an agent
/// gets error 1222 and can look at what blocks it.
pub const DEFAULT_LOCK_TIMEOUT: u32 = 10;

/// The longest `--lock-timeout`, in seconds: `SET LOCK_TIMEOUT` takes milliseconds in an
/// `int`, so 2,147,483 s (about 24.8 days) is the most that fits.
pub const MAX_LOCK_TIMEOUT: u32 = 2_147_483;

/// The argument of `SET LOCK_TIMEOUT` for a timeout in seconds: milliseconds, or -1 (wait
/// for ever) for 0. On the server 0 would mean "don't wait at all", which no setting asks
/// for; `--lock-timeout 1` is the shortest wait.
pub fn lock_timeout_ms(seconds: u32) -> i64 {
    if seconds == 0 {
        -1
    } else {
        i64::from(seconds) * 1000
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
    /// Seconds a statement waits for a lock; 0 means no limit.
    pub lock_timeout: Option<u32>,
    /// Read without shared locks in read-only runs; ignored when writes are allowed.
    pub read_uncommitted: Option<bool>,
    /// Count every row that is not shown, however long it takes (DESIGN.md §4.4).
    pub exact_count: Option<bool>,
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

#[derive(Clone, Debug, Default, PartialEq)]
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
    default_paths().map(|(path, _)| path)
}

/// The profiles file at its default place, and where it was before the project was named sqlh.
fn default_paths() -> Result<(PathBuf, PathBuf), String> {
    let appdata = std::env::var_os("APPDATA")
        .filter(|p| !p.is_empty())
        .ok_or("APPDATA is not set; set SQLH_CONFIG to the profiles file path")?;
    let appdata = PathBuf::from(appdata);
    Ok((
        appdata.join("sqlh").join("profiles.toml"),
        appdata.join("sqlharness").join("profiles.toml"),
    ))
}

/// Loads the profiles file; a missing file is an empty config. At the default place, profiles
/// saved before the rename are moved across first (`migrate_from_sqlharness`).
pub fn load() -> Result<Config, String> {
    if std::env::var_os("SQLH_CONFIG").is_none_or(|p| p.is_empty()) {
        let (new, old) = default_paths()?;
        migrate_from_sqlharness(&new, &old)?;
    }
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}:{e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The one-time move of profiles saved before the project was named sqlh (DESIGN.md §8):
/// `%APPDATA%\sqlharness\profiles.toml` to `%APPDATA%\sqlh\profiles.toml`, and each SQL
/// login's stored password from `sqlharness:<profile>` to `sqlh:<profile>`. It runs only while
/// the new file doesn't exist and the old one does, and never for a `SQLH_CONFIG` file. The
/// passwords move first and the file last, and each step can be repeated, so a move cut short
/// by an error is finished by the next run. The file is copied as it is, comments included.
fn migrate_from_sqlharness(new: &Path, old: &Path) -> Result<(), String> {
    if new.exists() || !old.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(old).map_err(|e| format!("{}: {e}", old.display()))?;
    let config = parse(&text).map_err(|e| format!("{}:{e}", old.display()))?;
    for profile in &config.profiles {
        if let Some(user) = &profile.user {
            credman::move_legacy(&profile.name, user).map_err(|e| {
                format!(
                    "moving profile {}'s stored password to Credential Manager as {}: {e}",
                    profile.name,
                    credman::target(&profile.name)
                )
            })?;
        }
    }
    if let Some(dir) = new.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    files::replace(new, text.as_bytes()).map_err(|e| format!("{}: {e}", new.display()))?;
    // The copy is in place; the old file, and its folder if that leaves it empty, can go.
    let _ = std::fs::remove_file(old);
    if let Some(dir) = old.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

pub fn save(config: &Config) -> Result<PathBuf, String> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    files::replace(&path, render(config).as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Everything a run needs, after merging flags, profile, environment and defaults.
pub struct Settings {
    pub connect: ConnectOptions,
    pub limits: Limits,
    pub query_timeout: u32,
    pub allow_write: bool,
    /// The profile's name when its `write_policy` is `deny`, so `--allow-write` is refused:
    /// a guard refusal then doesn't offer it (`guard::Blocked::advice_for`).
    pub write_denied_by: Option<String>,
    /// Seconds a statement waits for a lock; 0 means no limit.
    pub lock_timeout: u32,
    /// Whether reads run under READ UNCOMMITTED: asked for by `--read-uncommitted` or the
    /// profile, and never when writes are allowed.
    pub read_uncommitted: bool,
    /// Whether hidden rows are all counted: asked for by `--exact-count` or the profile. Runs
    /// that can write count exactly whatever this says (decided in `main`, with the input).
    pub exact_count: bool,
    pub profile: Option<String>,
}

/// What settings resolution reads from the machine, so tests can supply their own.
pub struct Sources<'a> {
    /// An environment variable, or `None` when unset or empty.
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// The profiles file.
    pub load: &'a dyn Fn() -> Result<Config, String>,
    /// A profile's stored password.
    pub password: &'a dyn Fn(&str) -> std::io::Result<Option<Secret>>,
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
    let allow_write = opts.allow_write || policy == WritePolicy::Allow;
    let write_denied_by = (policy == WritePolicy::Deny).then(|| p.name.clone());
    // READ UNCOMMITTED is for reads that nothing is written from: a write based on a dirty
    // read could store rows that never existed. Asked for on the command line in a run that
    // can write, it is refused; saved in a profile, it just doesn't apply to that run
    // (DESIGN.md §4.3).
    if allow_write && opts.read_uncommitted == Some(true) {
        return Err(Failure::usage(if opts.allow_write {
            "--read-uncommitted can't be used with --allow-write: a write could be based on \
             rows that were never committed. Drop one of the two"
                .to_owned()
        } else {
            format!(
                "--read-uncommitted applies to read-only runs, and profile {} has \
                 write_policy = \"allow\", so this run can write. Use a profile whose \
                 write_policy is \"flag\" or \"deny\"",
                p.name
            )
        }));
    }
    let read_uncommitted = !allow_write
        && opts
            .read_uncommitted
            .or(p.read_uncommitted)
            .unwrap_or(false);
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
        allow_write,
        write_denied_by,
        lock_timeout: opts
            .lock_timeout
            .or(p.lock_timeout)
            .unwrap_or(DEFAULT_LOCK_TIMEOUT),
        read_uncommitted,
        exact_count: opts.exact_count.or(p.exact_count).unwrap_or(false),
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
) -> Result<Secret, Failure> {
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
    (sources.var)("SQLCMDPASSWORD").map(Secret::new).ok_or_else(|| match profile {
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
            "lock_timeout" => {
                let seconds: u64 = value.integer(key).map_err(err)?;
                p.lock_timeout = Some(
                    u32::try_from(seconds)
                        .ok()
                        .filter(|s| *s <= MAX_LOCK_TIMEOUT)
                        .ok_or_else(|| {
                            err(format!(
                                "lock_timeout {seconds} is out of range: at most \
                                 {MAX_LOCK_TIMEOUT} seconds; 0 means no limit"
                            ))
                        })?,
                );
            }
            "read_uncommitted" => p.read_uncommitted = Some(value.boolean(key).map_err(err)?),
            "exact_count" => p.exact_count = Some(value.boolean(key).map_err(err)?),
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
         # Credential Manager as \"sqlh:<profile>\". `sqlh profile add` rewrites this file.\n",
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
            ("lock_timeout", p.lock_timeout.map(u64::from)),
        ] {
            if let Some(v) = value {
                let _ = writeln!(out, "{key} = {v}");
            }
        }
        if let Some(v) = p.read_uncommitted {
            let _ = writeln!(out, "read_uncommitted = {v}");
        }
        if let Some(v) = p.exact_count {
            let _ = writeln!(out, "exact_count = {v}");
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
                    lock_timeout: Some(0),
                    read_uncommitted: Some(true),
                    exact_count: Some(true),
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

    // PLAN.md 2.15: `exact_count` is a boolean profile key; unset it is not written.
    #[test]
    fn exact_count_key() {
        let config = parse(
            "[profiles.a]\nexact_count = true\n[profiles.b]\nexact_count = false\n[profiles.c]\n",
        )
        .unwrap();
        let get = |name: &str| config.find(name).unwrap().exact_count;
        assert_eq!(
            (get("a"), get("b"), get("c")),
            (Some(true), Some(false), None)
        );
        let text = render(&config);
        assert!(
            !text
                .split("[profiles.c]")
                .nth(1)
                .unwrap()
                .contains("exact_count")
        );
        assert_eq!(
            parse("[profiles.a]\nexact_count = 1\n")
                .unwrap_err()
                .to_string(),
            "2: exact_count must be true or false"
        );
    }

    #[test]
    fn lock_timeout_and_read_uncommitted_keys() {
        let config = parse(
            "[profiles.a]\nlock_timeout = 0\nread_uncommitted = true\n\
             [profiles.b]\nlock_timeout = 2147483\nread_uncommitted = false\n[profiles.c]\n",
        )
        .unwrap();
        let a = config.find("a").unwrap();
        assert_eq!((a.lock_timeout, a.read_uncommitted), (Some(0), Some(true)));
        let b = config.find("b").unwrap();
        assert_eq!(
            (b.lock_timeout, b.read_uncommitted),
            (Some(2_147_483), Some(false))
        );
        let c = config.find("c").unwrap();
        assert_eq!((c.lock_timeout, c.read_uncommitted), (None, None));
        // Unset keys are not written, so a saved file doesn't pin the defaults.
        let text = render(&config);
        let c_table = text.split("[profiles.c]").nth(1).unwrap();
        assert!(
            !c_table.contains("lock_timeout") && !c_table.contains("read_uncommitted"),
            "{text}"
        );

        for (line, expected) in [
            (
                "lock_timeout = 2147484",
                "2: lock_timeout 2147484 is out of range: at most 2147483 seconds; 0 means no limit",
            ),
            (
                "lock_timeout = 99999999999",
                "2: lock_timeout 99999999999 is out of range: at most 2147483 seconds; 0 means no limit",
            ),
            (
                "lock_timeout = \"10\"",
                "2: lock_timeout must be a whole number",
            ),
            (
                "lock_timeout = true",
                "2: lock_timeout must be a whole number",
            ),
            (
                "read_uncommitted = 1",
                "2: read_uncommitted must be true or false",
            ),
            (
                "read_uncommitted = \"true\"",
                "2: read_uncommitted must be true or false",
            ),
        ] {
            assert_eq!(
                parse(&format!("[profiles.a]\n{line}")).unwrap_err(),
                expected,
                "{line}"
            );
        }
        assert_eq!(
            parse("[profiles.a]\nlock_timeout = 5\nlock_timeout = 6").unwrap_err(),
            "3: lock_timeout is set twice"
        );
    }

    #[test]
    fn lock_timeouts_become_milliseconds() {
        assert_eq!(lock_timeout_ms(DEFAULT_LOCK_TIMEOUT), 10_000);
        assert_eq!(lock_timeout_ms(1), 1000);
        // 0 is "no limit" for the user, but 0 on the server means "don't wait at all".
        assert_eq!(lock_timeout_ms(0), -1);
        // The largest setting still fits SET LOCK_TIMEOUT's int.
        assert!(lock_timeout_ms(MAX_LOCK_TIMEOUT) <= i64::from(i32::MAX));
        assert!(lock_timeout_ms(MAX_LOCK_TIMEOUT + 1) > i64::from(i32::MAX));
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
            let password =
                |profile: &str| Ok((profile == "sqlp").then(|| Secret::new("stored".to_owned())));
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

        const LOCKS: &str = "[profiles.plain]\nserver = \"srv\"\nauth = \"windows\"\n\
                             [profiles.patient]\nserver = \"srv\"\nauth = \"windows\"\nlock_timeout = 30\n\
                             [profiles.forever]\nserver = \"srv\"\nauth = \"windows\"\nlock_timeout = 0\n";

        // PLAN.md 2.8: 10 s by default, from a profile, and a flag beats the profile. 0 is a
        // value ("no limit"), not "unset". There is no environment variable for it.
        #[test]
        fn the_lock_timeout_defaults_to_ten_seconds_and_follows_the_usual_precedence() {
            let timeout = |opts: &Opts| resolve_in(opts, LOCKS, &[]).unwrap().lock_timeout;
            assert_eq!(timeout(&with_profile("plain")), 10);
            assert_eq!(timeout(&with_profile("patient")), 30);
            assert_eq!(timeout(&with_profile("forever")), 0);
            let flagged = |name: &str, seconds: u32| Opts {
                lock_timeout: Some(seconds),
                ..with_profile(name)
            };
            assert_eq!(timeout(&flagged("patient", 3)), 3);
            assert_eq!(timeout(&flagged("patient", 0)), 0);
            assert_eq!(timeout(&flagged("forever", 7)), 7);
            // Without a profile, and the same in write runs.
            let bare = Opts {
                server: Some("srv".into()),
                ..Opts::default()
            };
            assert_eq!(timeout(&bare), 10);
            let writing = Opts {
                allow_write: true,
                ..with_profile("patient")
            };
            assert_eq!(timeout(&writing), 30);
        }

        const DIRTY: &str = "[profiles.plain]\nserver = \"srv\"\nauth = \"windows\"\n\
                             [profiles.dirty]\nserver = \"srv\"\nauth = \"windows\"\nread_uncommitted = true\n\
                             [profiles.dirty_writer]\nserver = \"srv\"\nauth = \"windows\"\nread_uncommitted = true\nwrite_policy = \"allow\"\n\
                             [profiles.writer]\nserver = \"srv\"\nauth = \"windows\"\nwrite_policy = \"allow\"\n";

        // Decision 6: exact counting is asked for by the flag or the profile, and the flag wins.
        // (Runs that can write count exactly whatever this says; that is decided in `main`.)
        #[test]
        fn exact_count_comes_from_the_flag_or_the_profile() {
            const PROFILES: &str = "[profiles.plain]\nserver = \"srv\"\nauth = \"windows\"\n\
                                    [profiles.exact]\nserver = \"srv\"\nauth = \"windows\"\nexact_count = true\n";
            let exact = |name: &str, flag: Option<bool>| {
                let opts = Opts {
                    exact_count: flag,
                    ..with_profile(name)
                };
                resolve_in(&opts, PROFILES, &[]).unwrap().exact_count
            };
            assert!(!exact("plain", None));
            assert!(exact("plain", Some(true)));
            assert!(exact("exact", None));
            assert!(!exact("exact", Some(false)));
        }

        // Decision 7: READ UNCOMMITTED is opt-in, for read-only runs only.
        #[test]
        fn read_uncommitted_is_opt_in_and_for_read_only_runs() {
            let dirty = |opts: &Opts| resolve_in(opts, DIRTY, &[]).unwrap().read_uncommitted;
            let with = |name: &str, flag: Option<bool>, allow_write: bool| Opts {
                read_uncommitted: flag,
                allow_write,
                ..with_profile(name)
            };
            // Off unless asked for.
            assert!(!dirty(&with("plain", None, false)));
            // On by the flag, or by the profile; the flag can turn a saved setting off.
            assert!(dirty(&with("plain", Some(true), false)));
            assert!(dirty(&with("dirty", None, false)));
            assert!(dirty(&with("dirty", Some(true), false)));
            assert!(!dirty(&with("dirty", Some(false), false)));
            let bare = Opts {
                server: Some("srv".into()),
                read_uncommitted: Some(true),
                ..Opts::default()
            };
            assert!(dirty(&bare));
            // A profile's setting is ignored when this run can write: with --allow-write, or
            // because the profile lets every run write.
            assert!(!dirty(&with("dirty", None, true)));
            assert!(!dirty(&with("dirty", Some(false), true)));
            assert!(!dirty(&with("dirty_writer", None, false)));
            assert!(!dirty(&with("dirty_writer", Some(false), false)));
            // The settings that follow it are not touched.
            let settings = resolve_in(&with("dirty", None, true), DIRTY, &[]).unwrap();
            assert!(settings.allow_write);
            assert_eq!(settings.lock_timeout, 10);
        }

        #[test]
        fn read_uncommitted_on_the_command_line_with_writes_is_a_usage_error() {
            use crate::error::Exit;
            let refused = |opts: &Opts| {
                let failure = resolve_in(opts, DIRTY, &[]).err().expect("refused");
                assert_eq!(failure.exit, Exit::Usage);
                failure.message
            };
            // With --allow-write, whichever profile or none: the message says why.
            for profile in ["plain", "dirty", "writer"] {
                let message = refused(&Opts {
                    read_uncommitted: Some(true),
                    allow_write: true,
                    ..with_profile(profile)
                });
                assert!(
                    message.starts_with("--read-uncommitted can't be used with --allow-write: ")
                        && message.contains("never committed"),
                    "{message}"
                );
            }
            let message = refused(&Opts {
                read_uncommitted: Some(true),
                allow_write: true,
                server: Some("srv".into()),
                ..Opts::default()
            });
            assert!(message.contains("--allow-write"), "{message}");
            // A profile that lets every run write is the same thing without the flag.
            let message = refused(&Opts {
                read_uncommitted: Some(true),
                ..with_profile("writer")
            });
            assert!(
                message.contains("profile writer has write_policy = \"allow\"")
                    && message.contains("can write"),
                "{message}"
            );
            // Turned off explicitly, it is no conflict.
            let off = Opts {
                read_uncommitted: Some(false),
                allow_write: true,
                ..with_profile("dirty")
            };
            assert!(resolve_in(&off, DIRTY, &[]).is_ok());
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
