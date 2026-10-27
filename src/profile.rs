//! `sqlh profiles add | list | test | rm` (DESIGN.md §8). `add` is the tool's only
//! interactive command: it reads the password with echo off and stores it in Credential
//! Manager, so agents can use the profile without ever seeing the password.

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use crate::app::{Out, finish, finish_ok, run_command, stdout_sink};
use crate::cli::{Opts, ProfileCommand};
use crate::config::{self, AuthKind, Config, DEFAULT_LOCK_TIMEOUT, Profile, WritePolicy};
use crate::credman::{Binding, Stored};
use crate::db::{Column, Value, check_server};
use crate::error::{Exit, Failure};
use crate::output::Limits;
use crate::run::{QueryOrigin, RunOutcome};
use crate::secret::Secret;
use crate::{console, credman, files};

/// Connection facts for `profiles test`: the `info` command's query.
use crate::schema::INFO_SQL;

/// What `profiles add` and `rm` read and change (F12): the profiles file, the stored passwords
/// and the console. A trait so that tests can make any step fail.
trait Store {
    /// Holds off other sqlh processes' edits of the profiles until the guard is dropped.
    fn lock(&self) -> Result<Option<files::Lock>, String>;
    /// Loads the profiles file; the caller holds the lock.
    fn load(&self) -> Result<Config, String>;
    fn save(&self, config: &Config) -> Result<PathBuf, String>;
    /// What the profile's stored password was stored for, without reading the password.
    fn binding(&self, profile: &str) -> io::Result<Option<Binding>>;
    fn read_password(&self, profile: &str) -> io::Result<Option<Stored>>;
    fn write_password(&self, profile: &str, binding: &Binding, password: &str) -> io::Result<()>;
    fn delete_password(&self, profile: &str) -> io::Result<bool>;
    fn is_console(&self) -> bool;
    fn prompt(&self, prompt: &str) -> io::Result<Secret>;
}

/// The real profiles file, Credential Manager and console.
struct System;

impl Store for System {
    fn lock(&self) -> Result<Option<files::Lock>, String> {
        config::lock().map(Some)
    }
    fn load(&self) -> Result<Config, String> {
        config::load_locked()
    }
    fn save(&self, config: &Config) -> Result<PathBuf, String> {
        config::save(config)
    }
    fn binding(&self, profile: &str) -> io::Result<Option<Binding>> {
        credman::binding(profile)
    }
    fn read_password(&self, profile: &str) -> io::Result<Option<Stored>> {
        credman::read(profile)
    }
    fn write_password(&self, profile: &str, binding: &Binding, password: &str) -> io::Result<()> {
        credman::write(profile, binding, password)
    }
    fn delete_password(&self, profile: &str) -> io::Result<bool> {
        credman::delete(profile)
    }
    fn is_console(&self) -> bool {
        console::stdin_is_console()
    }
    fn prompt(&self, prompt: &str) -> io::Result<Secret> {
        console::read_password(prompt)
    }
}

pub fn run(command: ProfileCommand, started: Instant) -> Exit {
    match command {
        ProfileCommand::Test { name } => test(name, started),
        ProfileCommand::Add { name, opts } => {
            let mut out = stdout_sink(Limits::default());
            match add(&name, &opts, &mut out, &System) {
                Ok(()) => {
                    let _ = out.flush();
                    drop(out);
                    test(name, started)
                }
                Err(failure) => finish(&mut out, RunOutcome::new(Err(failure)), started),
            }
        }
        ProfileCommand::List { search } => {
            let mut out = stdout_sink(Limits {
                max_rows: 0,
                ..Limits::default()
            });
            match list(&mut out, search.as_deref()) {
                Ok((shown, total)) => {
                    let s = if total == 1 { "" } else { "s" };
                    let detail = match search {
                        None => format!("{total} profile{s}"),
                        Some(_) => {
                            let verb = if shown == 1 { "matches" } else { "match" };
                            format!("{shown} of {total} profile{s} {verb}")
                        }
                    };
                    finish_ok(&mut out, &detail, started)
                }
                Err(failure) => finish(&mut out, RunOutcome::new(Err(failure)), started),
            }
        }
        ProfileCommand::Remove { name } => {
            let mut out = stdout_sink(Limits::default());
            match remove(&name, &mut out, &System) {
                Ok(()) => finish_ok(&mut out, &format!("removed profile {name}"), started),
                Err(failure) => finish(&mut out, RunOutcome::new(Err(failure)), started),
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
    run_command(opts, QueryOrigin::Generated, started)
}

/// Creates or updates a profile. Options not given keep their saved values. A new profile
/// refuses writes (`write_policy = "deny"`) unless `--write-policy` says otherwise: an agent
/// given `--allow-write` in `--help` used it on its own (agent trial 2, owner decision 12). The password is
/// asked for only when none is stored, when the server or login changes (so a profile can't
/// be repointed at another server and handed the stored password), or with --new-password.
///
/// The edit holds the profiles lock from the read of the file to its write, so another sqlh's
/// edit can't be lost in between (F12). The file and the stored password change together or
/// not at all: a new password is stored first and, if the file then can't be saved, the
/// previous one is put back as it was (or the new one deleted); an old password is deleted
/// only after the file no longer refers to it.
///
/// The stored password is bound to the profile's identity (`config::identity`, R20), so a run
/// that reads the file while this edit is between its two writes, or after it was killed
/// there, finds a password that doesn't fit the profile and never sends it. A password is
/// kept only when it fits the profile as saved: one an interrupted edit stored for another
/// server is asked for again, never rebound to the old one. A kept password whose identity
/// changes (encryption, certificate trust) is stored again with the new binding.
fn add(name: &str, opts: &Opts, out: &mut Out, store: &dyn Store) -> Result<(), Failure> {
    if let Some(description) = opts.description.as_deref().filter(|d| !d.is_empty()) {
        config::check_description(description)
            .map_err(|e| Failure::usage(format!("--description: {e}")))?;
    }
    let _lock = store.lock().map_err(Failure::usage)?;
    let mut config = store.load().map_err(Failure::usage)?;
    let existing = config.find(name).cloned();
    let new = existing.is_none();
    let base = existing.clone().unwrap_or_default();
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
        // Given, it replaces the saved one, and an empty one removes it; left out, it stays.
        description: match opts.description.as_deref() {
            Some("") => None,
            Some(description) => Some(description.to_owned()),
            None => base.description,
        },
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
        write_policy: opts
            .write_policy
            .or(base.write_policy)
            .or(new.then_some(WritePolicy::Deny)),
        login_timeout: opts.login_timeout.or(base.login_timeout),
        query_timeout: opts.query_timeout.or(base.query_timeout),
        max_rows: opts.max_rows.or(base.max_rows),
        max_cell: opts.max_cell.or(base.max_cell),
        lock_timeout: opts.lock_timeout.or(base.lock_timeout),
        read_uncommitted: opts.read_uncommitted.or(base.read_uncommitted),
        exact_count: opts.exact_count.or(base.exact_count),
    };
    let server = profile
        .server
        .clone()
        .ok_or_else(|| Failure::usage("profiles add needs -S server"))?;
    // Before the password is asked for or anything is saved: a run with this server would be
    // refused anyway (DESIGN.md §4.2).
    check_server(&server).map_err(|reason| Failure::usage(format!("-S {reason}")))?;

    // A stored password is kept only when it belongs to the profile as saved (R20).
    let stored = match (&user, &existing) {
        (Some(_), Some(saved)) => store
            .binding(name)
            .ok()
            .flatten()
            .filter(|binding| config::fits(binding, saved)),
        _ => None,
    };
    let kept_password = stored.is_some() && !login_changed && !opts.new_password;
    let binding = user.as_ref().map(|user| Binding {
        user: user.clone(),
        identity: Some(config::identity(&profile)),
    });
    let target = credman::target(name);
    // A new password, or a kept one bound anew: stored before the file is saved; what it
    // replaces is kept in memory so that a failed save can put it back as it was.
    let replaced = match &binding {
        Some(binding) if !kept_password || stored.as_ref() != Some(binding) => {
            let typed = if kept_password {
                None
            } else {
                if !store.is_console() {
                    return Err(Failure::usage(
                        "profiles add needs to read a password from the console, but stdin is \
                         not a console. Run it in a terminal; for automation set \
                         SQLCMDPASSWORD instead",
                    ));
                }
                let password = store
                    .prompt(&format!("Password for {} on {server}: ", binding.user))
                    .map_err(|e| Failure::usage(format!("reading the password: {e}")))?;
                if password.is_empty() {
                    return Err(Failure::usage("empty password; nothing was saved"));
                }
                Some(password)
            };
            let previous = store.read_password(name).map_err(|e| {
                Failure::new(
                    Exit::Internal,
                    format!("reading the stored password {target}: {e}; nothing was saved"),
                )
            })?;
            let password = match (typed, &previous) {
                (Some(typed), _) => typed,
                (None, Some(previous)) => previous.password.clone(),
                (None, None) => {
                    return Err(Failure::usage(format!(
                        "the stored password {target} is gone; nothing was saved. Rerun sqlh \
                         profiles add {name} --new-password"
                    )));
                }
            };
            store
                .write_password(name, binding, &password)
                .map_err(|e| {
                    Failure::new(
                        Exit::Internal,
                        format!("saving to Credential Manager: {e}; nothing was saved"),
                    )
                })?;
            Some(previous)
        }
        _ => None,
    };

    config.upsert(profile);
    let became_default = config.default.is_none();
    if became_default {
        config.default = Some(name.to_owned());
    }
    let path = match store.save(&config) {
        Ok(path) => path,
        Err(e) => {
            // The file still describes the profile as it was, so its password must be too.
            let undone = match &replaced {
                Some(Some(previous)) => {
                    store.write_password(name, &previous.binding, &previous.password)
                }
                Some(None) => store.delete_password(name).map(|_| ()),
                None => Ok(()),
            };
            return Err(match undone {
                Ok(()) => Failure::usage(format!(
                    "saving the profiles file failed: {e}; nothing was changed"
                )),
                Err(undo) => Failure::new(
                    Exit::Internal,
                    format!(
                        "saving the profiles file failed: {e}; and putting back the password \
                         {target} as it was failed too: {undo}; rerun sqlh profiles add {name} \
                         --new-password, or remove it with: cmdkey /delete:{target}"
                    ),
                ),
            });
        }
    };
    // Windows auth: a password left from an earlier SQL-auth version of the profile goes,
    // now that the file no longer refers to it.
    if user.is_none() {
        store.delete_password(name).map_err(|e| {
            Failure::new(
                Exit::Internal,
                format!(
                    "saved profile {name}, but deleting its old password {target} from \
                     Credential Manager failed: {e}; rerun sqlh profiles add {name} -E, or remove \
                     it with: cmdkey /delete:{target}"
                ),
            )
        })?;
    }
    let _ = out.note_with_path(
        &format!("saved profile {name} in "),
        &path.display().to_string(),
    );
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
    if new && opts.write_policy.is_none() {
        let _ = out.note(&format!(
            "{name} refuses writes (write_policy deny, the default for a new profile); \
             sqlh profiles add {name} --write-policy flag lets --allow-write through"
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

/// Lists the profiles, or with `search` those whose name or description contains it, in any
/// case (PLAN.md 4.2). Returns how many were listed, and how many there are. Never reads a
/// password: a credential's existence is all it checks.
fn list(out: &mut Out, search: Option<&str>) -> Result<(usize, usize), Failure> {
    let config = config::load().map_err(Failure::usage)?;
    let path = config::path().map_err(Failure::usage)?;
    let _ = out.note_with_path("profiles file: ", &path.display().to_string());
    let needle = search.map(str::to_lowercase);
    let matches = |p: &Profile| match &needle {
        None => true,
        Some(needle) => {
            p.name.to_lowercase().contains(needle)
                || p.description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(needle))
        }
    };
    let listed: Vec<&Profile> = config.profiles.iter().filter(|p| matches(p)).collect();
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
        Column {
            name: "lock_timeout".into(),
            type_name: "int".into(),
            nullable: false,
        },
        Column {
            name: "read_uncommitted".into(),
            type_name: "bit".into(),
            nullable: false,
        },
        Column {
            name: "exact_count".into(),
            type_name: "bit".into(),
            nullable: false,
        },
        column("description"),
    ];
    let text = |v: &Option<String>| v.clone().map_or(Value::Null, Value::Text);
    let write = |out: &mut Out| -> std::io::Result<()> {
        out.columns(&columns)?;
        for p in &listed {
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
                // What a run with this profile gets, so an unset key shows its default.
                Value::Int(p.lock_timeout.unwrap_or(DEFAULT_LOCK_TIMEOUT).into()),
                Value::Int(p.read_uncommitted.unwrap_or(false).into()),
                Value::Int(p.exact_count.unwrap_or(false).into()),
                text(&p.description),
            ])?;
        }
        out.end_rows()
    };
    write(out)
        .map_err(|e| Failure::new(Exit::Undelivered, format!("writing output failed: {e}")))?;
    Ok((listed.len(), config.profiles.len()))
}

/// Removes a profile: the file first, then its stored password, under the profiles lock. A
/// removal whose password couldn't be deleted can be finished by running it again: a profile
/// that is gone but left its password behind gets the password deleted (F12).
fn remove(name: &str, out: &mut Out, store: &dyn Store) -> Result<(), Failure> {
    let _lock = store.lock().map_err(Failure::usage)?;
    let mut config = store.load().map_err(Failure::usage)?;
    if !config.remove(name) {
        if matches!(store.binding(name), Ok(Some(_))) {
            return delete_password(name, out, store).map(|_| {
                let _ = out.note(&format!(
                    "there was no profile named {name}; its password left from an earlier \
                     removal is deleted"
                ));
            });
        }
        return Err(Failure::usage(format!(
            "no profile named {name:?}; see sqlh profiles list"
        )));
    }
    store.save(&config).map_err(Failure::usage)?;
    delete_password(name, out, store).map_err(|failure| {
        Failure::new(
            failure.exit,
            format!("removed profile {name}, but {}", failure.message),
        )
    })?;
    Ok(())
}

/// Deletes a profile's stored password, saying so; an error says how to finish the job.
fn delete_password(name: &str, out: &mut Out, store: &dyn Store) -> Result<bool, Failure> {
    let target = credman::target(name);
    match store.delete_password(name) {
        Ok(deleted) => {
            if deleted {
                let _ = out.note(&format!("deleted {target} from Credential Manager"));
            }
            Ok(deleted)
        }
        Err(e) => Err(Failure::new(
            Exit::Internal,
            format!(
                "deleting {target} from Credential Manager failed: {e}; rerun sqlh profiles rm \
                 {name}, or remove it with: cmdkey /delete:{target}"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{self, Format};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// A store in memory whose steps can be made to fail (F12, PLAN.md 3.2).
    #[derive(Default)]
    struct Fake {
        config: RefCell<Config>,
        /// Profile name to (binding, password).
        passwords: RefCell<HashMap<String, (Binding, String)>>,
        /// What the password prompt answers.
        typed: RefCell<String>,
        fail_lock: bool,
        fail_save: Cell<bool>,
        fail_write: Cell<bool>,
        fail_delete: Cell<bool>,
        /// Fail every password write after this many have succeeded.
        writes_left: Cell<Option<u32>>,
        /// Runs at the start of each save: between the password's write and the file's.
        on_save: RefCell<Option<Hook>>,
    }

    type Hook = Box<dyn Fn(&Fake)>;

    impl Store for Fake {
        fn lock(&self) -> Result<Option<files::Lock>, String> {
            if self.fail_lock {
                Err("another sqlh is editing the profiles".into())
            } else {
                Ok(None)
            }
        }
        fn load(&self) -> Result<Config, String> {
            Ok(self.config.borrow().clone())
        }
        fn save(&self, config: &Config) -> Result<PathBuf, String> {
            if let Some(hook) = self.on_save.borrow().as_ref() {
                hook(self);
            }
            if self.fail_save.get() {
                return Err("disk full".into());
            }
            *self.config.borrow_mut() = config.clone();
            Ok(PathBuf::from("profiles.toml"))
        }
        fn binding(&self, profile: &str) -> io::Result<Option<Binding>> {
            Ok(self.passwords.borrow().get(profile).map(|(b, _)| b.clone()))
        }
        fn read_password(&self, profile: &str) -> io::Result<Option<Stored>> {
            Ok(self.passwords.borrow().get(profile).map(|(b, p)| Stored {
                password: Secret::new(p.clone()),
                binding: b.clone(),
            }))
        }
        fn write_password(
            &self,
            profile: &str,
            binding: &Binding,
            password: &str,
        ) -> io::Result<()> {
            if let Some(left) = self.writes_left.get() {
                if left == 0 {
                    return Err(io::Error::other("Credential Manager is unavailable"));
                }
                self.writes_left.set(Some(left - 1));
            }
            if self.fail_write.get() {
                return Err(io::Error::other("Credential Manager is unavailable"));
            }
            self.passwords
                .borrow_mut()
                .insert(profile.into(), (binding.clone(), password.into()));
            Ok(())
        }
        fn delete_password(&self, profile: &str) -> io::Result<bool> {
            if self.fail_delete.get() {
                return Err(io::Error::other("access denied"));
            }
            Ok(self.passwords.borrow_mut().remove(profile).is_some())
        }
        fn is_console(&self) -> bool {
            true
        }
        fn prompt(&self, _prompt: &str) -> io::Result<Secret> {
            Ok(Secret::new(self.typed.borrow().clone()))
        }
    }

    fn out() -> Out {
        output::create(Format::Text, Box::new(io::sink()), Limits::default())
    }

    /// A store holding profile `dev` (SQL login `me` on `srv`) with the password `old`, where
    /// the prompt answers `typed`.
    fn with_dev(typed: &str) -> Fake {
        let fake = Fake {
            typed: RefCell::new(typed.into()),
            ..Fake::default()
        };
        let dev = Profile {
            name: "dev".into(),
            server: Some("srv".into()),
            auth: Some(AuthKind::Sql),
            user: Some("me".into()),
            ..Profile::default()
        };
        fake.passwords
            .borrow_mut()
            .insert("dev".into(), (bound_to(&dev), "old".into()));
        fake.config.borrow_mut().upsert(dev);
        fake
    }

    /// The binding `profiles add` gives a password stored for `profile`.
    fn bound_to(profile: &Profile) -> Binding {
        Binding {
            user: profile.user.clone().unwrap_or_default(),
            identity: Some(config::identity(profile)),
        }
    }

    fn password(fake: &Fake, name: &str) -> Option<String> {
        fake.passwords.borrow().get(name).map(|(_, p)| p.clone())
    }

    fn new_password() -> Opts {
        Opts {
            new_password: true,
            ..Opts::default()
        }
    }

    fn windows() -> Opts {
        Opts {
            integrated: true,
            ..Opts::default()
        }
    }

    // F12: a new password is stored before the file is saved; when the save fails, the
    // previous password is put back, so the file and Credential Manager still agree.
    #[test]
    fn a_failed_save_puts_the_previous_password_back() {
        let fake = with_dev("new");
        fake.fail_save.set(true);
        let failure = add("dev", &new_password(), &mut out(), &fake).unwrap_err();
        assert!(
            failure.message.contains("nothing was changed"),
            "{}",
            failure.message
        );
        assert_eq!(password(&fake, "dev").as_deref(), Some("old"));
        // The same edit, saved: the new password stays.
        fake.fail_save.set(false);
        add("dev", &new_password(), &mut out(), &fake).unwrap();
        assert_eq!(password(&fake, "dev").as_deref(), Some("new"));
    }

    // F12: a new profile whose save fails leaves no password behind.
    #[test]
    fn a_failed_save_of_a_new_profile_leaves_no_password() {
        let fake = Fake {
            typed: RefCell::new("pw".into()),
            ..Fake::default()
        };
        fake.fail_save.set(true);
        let opts = Opts {
            server: Some("srv".into()),
            user: Some("me".into()),
            ..Opts::default()
        };
        let failure = add("qa", &opts, &mut out(), &fake).unwrap_err();
        assert_eq!(failure.exit, Exit::Usage, "{}", failure.message);
        assert_eq!(password(&fake, "qa"), None);
        assert!(fake.config.borrow().find("qa").is_none());
    }

    // F12: when putting the old password back fails too, the error says what state is left
    // and how to repair it.
    #[test]
    fn a_failed_undo_says_how_to_repair() {
        let fake = with_dev("new");
        fake.fail_save.set(true);
        // The new password is written; the write that would put `old` back fails.
        fake.writes_left.set(Some(1));
        let failure = add("dev", &new_password(), &mut out(), &fake).unwrap_err();
        assert_eq!(failure.exit, Exit::Internal);
        assert!(
            failure
                .message
                .contains("putting back the password sqlh:dev as it was failed too")
                && failure.message.contains("cmdkey /delete:sqlh:dev"),
            "{}",
            failure.message
        );
    }

    // F12: a password that can't be stored leaves everything as it was.
    #[test]
    fn a_password_that_cannot_be_stored_changes_nothing() {
        let fake = with_dev("new");
        fake.fail_write.set(true);
        let failure = add("dev", &new_password(), &mut out(), &fake).unwrap_err();
        assert!(
            failure.message.contains("nothing was saved"),
            "{}",
            failure.message
        );
        assert_eq!(password(&fake, "dev").as_deref(), Some("old"));
    }

    // F12: switching to Windows auth deletes the old password only after the file is saved.
    // A failed save keeps it (the file still says SQL login); a failed delete after the save
    // says how to finish.
    #[test]
    fn windows_auth_deletes_the_old_password_only_after_the_save() {
        let fake = with_dev("");
        fake.fail_save.set(true);
        assert!(add("dev", &windows(), &mut out(), &fake).is_err());
        assert_eq!(password(&fake, "dev").as_deref(), Some("old"));

        fake.fail_save.set(false);
        fake.fail_delete.set(true);
        let failure = add("dev", &windows(), &mut out(), &fake).unwrap_err();
        assert_eq!(failure.exit, Exit::Internal);
        assert!(
            failure
                .message
                .starts_with("saved profile dev, but deleting its old password")
                && failure.message.contains("rerun sqlh profiles add dev -E"),
            "{}",
            failure.message
        );
        assert_eq!(
            fake.config.borrow().find("dev").unwrap().auth,
            Some(AuthKind::Windows)
        );

        fake.fail_delete.set(false);
        add("dev", &windows(), &mut out(), &fake).unwrap();
        assert_eq!(password(&fake, "dev"), None);
    }

    // F12: a removal whose password delete failed can be finished by running it again.
    #[test]
    fn a_failed_removal_can_be_finished_by_running_it_again() {
        let fake = with_dev("");
        fake.fail_delete.set(true);
        let failure = remove("dev", &mut out(), &fake).unwrap_err();
        assert!(
            failure
                .message
                .starts_with("removed profile dev, but deleting sqlh:dev")
                && failure.message.contains("rerun sqlh profiles rm dev"),
            "{}",
            failure.message
        );
        assert!(fake.config.borrow().find("dev").is_none());
        assert_eq!(password(&fake, "dev").as_deref(), Some("old"));

        fake.fail_delete.set(false);
        remove("dev", &mut out(), &fake).unwrap();
        assert_eq!(password(&fake, "dev"), None);
        // Nothing left: an ordinary "no profile" error.
        let failure = remove("dev", &mut out(), &fake).unwrap_err();
        assert!(
            failure.message.starts_with("no profile named"),
            "{}",
            failure.message
        );
    }

    // F12: an edit that can't take the profiles lock changes nothing.
    #[test]
    fn an_edit_that_cannot_take_the_lock_changes_nothing() {
        let fake = Fake {
            fail_lock: true,
            ..with_dev("new")
        };
        let failure = add("dev", &new_password(), &mut out(), &fake).unwrap_err();
        assert!(
            failure.message.contains("another sqlh"),
            "{}",
            failure.message
        );
        assert_eq!(password(&fake, "dev").as_deref(), Some("old"));
        assert!(remove("dev", &mut out(), &fake).is_err());
        assert!(fake.config.borrow().find("dev").is_some());
    }

    /// What a run with `--profile dev` would connect with: the server and the password,
    /// resolved by `config::resolve_with` from `load` and the fake's passwords. `lock_free`
    /// says whether the run can take the profiles lock, or an edit holds it.
    fn run_with(
        fake: &Fake,
        load: &dyn Fn() -> Result<Config, String>,
        lock_free: bool,
    ) -> Result<(String, String), String> {
        let var = |_: &str| None;
        let password = |name: &str| fake.read_password(name);
        let lock = || {
            if lock_free {
                Ok(None)
            } else {
                Err("another sqlh is editing the profiles".to_owned())
            }
        };
        let opts = Opts {
            profile: Some("dev".into()),
            ..Opts::default()
        };
        let sources = config::Sources {
            var: &var,
            load,
            password: &password,
            lock: &lock,
        };
        let settings = config::resolve_with(&opts, &sources).map_err(|f| f.message)?;
        match settings.connect.auth {
            crate::db::Auth::Sql { password, .. } => {
                Ok((settings.connect.server, password.as_str().to_owned()))
            }
            crate::db::Auth::Integrated => Err("Windows auth".into()),
        }
    }

    fn run(fake: &Fake, lock_free: bool) -> Result<(String, String), String> {
        run_with(fake, &|| Ok(fake.config.borrow().clone()), lock_free)
    }

    fn pair(server: &str, password: &str) -> Result<(String, String), String> {
        Ok((server.into(), password.into()))
    }

    // R20: a run between an edit's two writes (the password stored, the file not yet saved)
    // never sends the new password to the old server, login or encryption. With the lock
    // held by the edit, it fails safely; once the edit is done, it gets the new pair. A
    // password-only change keeps the identity, so either password goes to the same login.
    #[test]
    fn a_run_during_an_edit_never_pairs_the_new_password_with_the_old_profile() {
        use crate::db::Encrypt;
        let edits = [
            (
                "server",
                Opts {
                    server: Some("other".into()),
                    ..Opts::default()
                },
            ),
            (
                "login",
                Opts {
                    user: Some("you".into()),
                    ..Opts::default()
                },
            ),
            (
                "encryption",
                Opts {
                    encrypt: Some(Encrypt::Strict),
                    ..new_password()
                },
            ),
            ("password", new_password()),
        ];
        for (what, opts) in edits {
            let fake = with_dev("new");
            let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
            let record = seen.clone();
            *fake.on_save.borrow_mut() = Some(Box::new(move |fake: &Fake| {
                record.borrow_mut().push(run(fake, false));
            }));
            add("dev", &opts, &mut out(), &fake).unwrap();
            let during = seen.borrow()[0].clone();
            if what == "password" {
                assert_eq!(during, pair("srv", "new"), "{what}");
            } else {
                let message = during.expect_err(what);
                assert!(
                    message.contains("isn't sent") && message.contains("another sqlh"),
                    "{what}: {message}"
                );
            }
            let server = if what == "server" { "other" } else { "srv" };
            *fake.on_save.borrow_mut() = None;
            assert_eq!(run(&fake, true), pair(server, "new"), "{what}");
        }
    }

    // R20: an edit that lands between a run's read of the file and its read of the password
    // is caught: the password doesn't fit the profile the run read, so the run reads both
    // again under the lock and gets the edit's pair.
    #[test]
    fn an_edit_between_a_runs_two_reads_is_read_again_under_the_lock() {
        let fake = with_dev("new");
        let before = fake.config.borrow().clone();
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            if loads.get() > 1 {
                return Ok(fake.config.borrow().clone());
            }
            let opts = Opts {
                server: Some("other".into()),
                ..Opts::default()
            };
            add("dev", &opts, &mut out(), &fake).unwrap();
            Ok(before.clone())
        };
        assert_eq!(run_with(&fake, &load, true), pair("other", "new"));
        assert_eq!(loads.get(), 2);
    }

    // R20: an edit killed between its two writes leaves the new password bound to the new
    // server and the file naming the old one. A fresh run refuses to send it and says how to
    // repair; the repair asks for the password again rather than keeping the stored one.
    #[test]
    fn an_edit_killed_between_its_writes_fails_closed_until_the_profile_is_added_again() {
        let fake = with_dev("new");
        *fake.on_save.borrow_mut() = Some(Box::new(|_: &Fake| panic!("killed")));
        let opts = Opts {
            server: Some("other".into()),
            ..Opts::default()
        };
        let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            add("dev", &opts, &mut out(), &fake)
        }));
        assert!(killed.is_err());
        *fake.on_save.borrow_mut() = None;
        assert_eq!(password(&fake, "dev").as_deref(), Some("new"));
        assert_eq!(
            fake.config.borrow().find("dev").unwrap().server.as_deref(),
            Some("srv")
        );

        let message = run(&fake, true).unwrap_err();
        assert!(
            message.contains("was stored for another server")
                && message.contains("run: sqlh profiles add dev"),
            "{message}"
        );

        *fake.typed.borrow_mut() = "again".into();
        add("dev", &Opts::default(), &mut out(), &fake).unwrap();
        assert_eq!(run(&fake, true), pair("srv", "again"));
    }

    // R20: an edit that changes only the encryption keeps the password, stored again with the
    // profile's new identity (no prompt: it would answer an empty password and fail).
    #[test]
    fn a_kept_password_is_bound_to_the_new_encryption() {
        let fake = with_dev("");
        let opts = Opts {
            trust_server_certificate: Some(true),
            ..Opts::default()
        };
        add("dev", &opts, &mut out(), &fake).unwrap();
        let saved = fake.config.borrow().find("dev").cloned().unwrap();
        assert_eq!(
            fake.binding("dev").unwrap(),
            Some(bound_to(&saved)),
            "rebound"
        );
        assert_eq!(run(&fake, true), pair("srv", "old"));
    }

    // PLAN.md 4.1: --description replaces the saved description, an edit without it keeps it,
    // and an empty one removes it. One that isn't a single line, or is too long, is refused
    // before anything is read or written.
    #[test]
    fn add_keeps_replaces_and_removes_the_description() {
        let fake = with_dev("");
        let described = |text: &str| Opts {
            description: Some(text.into()),
            ..Opts::default()
        };
        let saved = |fake: &Fake| {
            fake.config
                .borrow()
                .find("dev")
                .unwrap()
                .description
                .clone()
        };
        add(
            "dev",
            &described("prod reporting replica"),
            &mut out(),
            &fake,
        )
        .unwrap();
        assert_eq!(saved(&fake).as_deref(), Some("prod reporting replica"));
        let policy = Opts {
            write_policy: Some(WritePolicy::Flag),
            ..Opts::default()
        };
        add("dev", &policy, &mut out(), &fake).unwrap();
        assert_eq!(saved(&fake).as_deref(), Some("prod reporting replica"));
        add("dev", &described("dev box"), &mut out(), &fake).unwrap();
        assert_eq!(saved(&fake).as_deref(), Some("dev box"));
        add("dev", &described(""), &mut out(), &fake).unwrap();
        assert_eq!(saved(&fake), None);

        let refused = Fake {
            fail_lock: true,
            ..with_dev("")
        };
        for bad in ["two\nlines", &"x".repeat(301)] {
            let failure = add("dev", &described(bad), &mut out(), &refused).unwrap_err();
            assert_eq!(failure.exit, Exit::Usage);
            assert!(
                failure.message.starts_with("--description: description "),
                "{}",
                failure.message
            );
        }
    }

    // R20: a password stored before bindings (no identity) is used when its login matches the
    // profile, refused when it doesn't, and bound by the next edit that keeps it.
    #[test]
    fn a_password_without_an_identity_is_checked_by_its_login() {
        let fake = with_dev("");
        let legacy = |user: &str| Binding {
            user: user.into(),
            identity: None,
        };
        fake.passwords
            .borrow_mut()
            .insert("dev".into(), (legacy("me"), "old".into()));
        assert_eq!(run(&fake, true), pair("srv", "old"));
        add("dev", &Opts::default(), &mut out(), &fake).unwrap();
        let saved = fake.config.borrow().find("dev").cloned().unwrap();
        assert_eq!(fake.binding("dev").unwrap(), Some(bound_to(&saved)));

        fake.passwords
            .borrow_mut()
            .insert("dev".into(), (legacy("someone_else"), "theirs".into()));
        assert!(run(&fake, true).is_err());
    }
}
