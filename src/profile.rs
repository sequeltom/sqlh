//! `sqlh profile add | list | test | rm` (DESIGN.md §8). `add` is the tool's only
//! interactive command: it reads the password with echo off and stores it in Credential
//! Manager, so agents can use the profile without ever seeing the password.

use std::time::Instant;

use crate::cli::{Opts, ProfileCommand};
use crate::config::{self, AuthKind, Profile};
use crate::db::{Column, Value};
use crate::error::{Exit, Failure};
use crate::output::Limits;
use crate::{Out, console, credman, finish, finish_ok, run_command, stdout_sink};

/// Connection facts for `profile test`. Runs on SQL Server 2012+ and Azure SQL DB without
/// extra permissions (CONNECTIONPROPERTY needs no VIEW SERVER STATE). Encryption is a
/// client-side fact, printed as a note by `show_connection`.
const INFO_SQL: &str = "SELECT @@SERVERNAME AS server, \
    CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS version, \
    CAST(SERVERPROPERTY('Edition') AS nvarchar(128)) AS edition, \
    DB_NAME() AS [database], SUSER_SNAME() AS login, USER_NAME() AS [user], \
    CAST(CONNECTIONPROPERTY('net_transport') AS nvarchar(40)) AS transport, \
    CAST(CONNECTIONPROPERTY('auth_scheme') AS nvarchar(40)) AS auth_scheme, \
    SYSDATETIMEOFFSET() AS server_time;";

pub fn run(command: ProfileCommand, started: Instant) -> Exit {
    match command {
        ProfileCommand::Test { name } => test(name, started),
        ProfileCommand::Add { name, opts } => {
            let mut out = stdout_sink(Limits::default());
            match add(&name, &opts, &mut out) {
                Ok(()) => {
                    let _ = out.flush();
                    drop(out);
                    test(name, started)
                }
                Err(failure) => finish(&mut out, Err(failure), started),
            }
        }
        ProfileCommand::List => {
            let mut out = stdout_sink(Limits {
                max_rows: 0,
                ..Limits::default()
            });
            match list(&mut out) {
                Ok(count) => {
                    let s = if count == 1 { "" } else { "s" };
                    finish_ok(&mut out, &format!("{count} profile{s}"), started)
                }
                Err(failure) => finish(&mut out, Err(failure), started),
            }
        }
        ProfileCommand::Remove { name } => {
            let mut out = stdout_sink(Limits::default());
            match remove(&name, &mut out) {
                Ok(()) => finish_ok(&mut out, &format!("removed profile {name}"), started),
                Err(failure) => finish(&mut out, Err(failure), started),
            }
        }
    }
}

fn test(name: String, started: Instant) -> Exit {
    let opts = Opts {
        profile: Some(name),
        query: Some(INFO_SQL.into()),
        show_connection: true,
        ..Opts::default()
    };
    run_command(opts, started)
}

/// Creates or updates a profile. Options not given keep their saved values. The password is
/// asked for only when none is stored, when the server or login changes (so a profile can't
/// be repointed at another server and handed the stored password), or with --new-password.
fn add(name: &str, opts: &Opts, out: &mut Out) -> Result<(), Failure> {
    let mut config = config::load().map_err(Failure::usage)?;
    let base = config.find(name).cloned().unwrap_or_default();
    let user = if opts.integrated {
        None
    } else {
        opts.user.clone().or(base.user.clone())
    };
    let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    let login_changed = !same(&user, &base.user)
        || !same(&opts.server.clone().or(base.server.clone()), &base.server);
    let profile = Profile {
        name: name.to_owned(),
        server: opts.server.clone().or(base.server),
        database: opts.database.clone().or(base.database),
        auth: Some(if user.is_some() {
            AuthKind::Sql
        } else {
            AuthKind::Windows
        }),
        user: user.clone(),
        encrypt: opts.encrypt.or(base.encrypt),
        trust_server_certificate: opts
            .trust_server_certificate
            .or(base.trust_server_certificate),
        write_policy: opts.write_policy.or(base.write_policy),
        login_timeout: opts.login_timeout.or(base.login_timeout),
        query_timeout: opts.query_timeout.or(base.query_timeout),
        max_rows: opts.max_rows.or(base.max_rows),
        max_cell: opts.max_cell.or(base.max_cell),
    };
    let server = profile
        .server
        .clone()
        .ok_or_else(|| Failure::usage("profile add needs -S server"))?;

    let stored = user.is_some() && matches!(credman::exists(name), Ok(true));
    let mut kept_password = false;
    match &user {
        Some(_) if stored && !login_changed && !opts.new_password => kept_password = true,
        Some(user) => {
            if !console::stdin_is_console() {
                return Err(Failure::usage(
                    "profile add needs to read a password from the console, but stdin is not a \
                     console. Run it in a terminal; for automation set SQLCMDPASSWORD instead",
                ));
            }
            let password = console::read_password(&format!("Password for {user} on {server}: "))
                .map_err(|e| Failure::usage(format!("reading the password: {e}")))?;
            if password.is_empty() {
                return Err(Failure::usage("empty password; nothing was saved"));
            }
            credman::write(name, user, &password).map_err(|e| {
                Failure::new(Exit::Internal, format!("saving to Credential Manager: {e}"))
            })?;
        }
        // Windows auth: drop any password left from an earlier SQL-auth version of the profile.
        None => {
            credman::delete(name).map_err(|e| {
                Failure::new(
                    Exit::Internal,
                    format!(
                        "removing the old password {} from Credential Manager: {e}; nothing was saved",
                        credman::target(name)
                    ),
                )
            })?;
        }
    }

    config.upsert(profile);
    let became_default = config.default.is_none();
    if became_default {
        config.default = Some(name.to_owned());
    }
    let path = config::save(&config).map_err(Failure::usage)?;
    let _ = out.note(&format!("saved profile {name} in {}", path.display()));
    if user.is_some() {
        let how = if kept_password {
            "kept the password stored"
        } else {
            "password stored"
        };
        let _ = out.note(&format!(
            "{how} in Windows Credential Manager as {}",
            credman::target(name)
        ));
    }
    if became_default {
        let _ = out.note(&format!(
            "{name} is the default profile: runs without -S or --profile use it"
        ));
    }
    let _ = out.note("testing the connection");
    Ok(())
}

fn list(out: &mut Out) -> Result<usize, Failure> {
    let config = config::load().map_err(Failure::usage)?;
    let path = config::path().map_err(Failure::usage)?;
    let _ = out.note(&format!("profiles file: {}", path.display()));
    let column = |name: &str| Column {
        name: name.into(),
        type_name: "text".into(),
        nullable: true,
    };
    let columns = [
        column("name"),
        Column {
            name: "default".into(),
            type_name: "bit".into(),
            nullable: false,
        },
        column("server"),
        column("database"),
        column("auth"),
        column("user"),
        column("password"),
        column("write_policy"),
    ];
    let text = |v: &Option<String>| v.clone().map_or(Value::Null, Value::Text);
    let write = |out: &mut Out| -> std::io::Result<()> {
        out.columns(&columns)?;
        for p in &config.profiles {
            let is_default = config
                .default
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(&p.name));
            let sql = p.auth_kind() == AuthKind::Sql;
            let password = match (sql, credman::exists(&p.name)) {
                (false, _) => "-",
                (true, Ok(true)) => "stored",
                (true, Ok(false)) => "missing",
                (true, Err(_)) => "unreadable",
            };
            out.row(&[
                Value::Text(p.name.clone()),
                Value::Int(is_default.into()),
                text(&p.server),
                text(&p.database),
                Value::Text(p.auth_kind().as_str().into()),
                text(&p.user),
                Value::Text(password.into()),
                Value::Text(p.write_policy.map_or("flag", |w| w.as_str()).into()),
            ])?;
        }
        out.end_rows()
    };
    write(out)
        .map_err(|e| Failure::new(Exit::Undelivered, format!("writing output failed: {e}")))?;
    Ok(config.profiles.len())
}

fn remove(name: &str, out: &mut Out) -> Result<(), Failure> {
    let mut config = config::load().map_err(Failure::usage)?;
    if !config.remove(name) {
        return Err(Failure::usage(format!(
            "no profile named {name:?}; see sqlh profile list"
        )));
    }
    config::save(&config).map_err(Failure::usage)?;
    match credman::delete(name) {
        Ok(true) => {
            let _ = out.note(&format!(
                "deleted {} from Credential Manager",
                credman::target(name)
            ));
        }
        Ok(false) => {}
        // The profile is gone; say so, and that rerunning `rm` won't help but cmdkey will.
        Err(e) => {
            return Err(Failure::new(
                Exit::Internal,
                format!(
                    "removed profile {name}, but deleting {} from Credential Manager failed: {e}; \
                     remove it with: cmdkey /delete:{}",
                    credman::target(name),
                    credman::target(name)
                ),
            ));
        }
    }
    Ok(())
}
