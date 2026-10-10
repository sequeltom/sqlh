//! Process-level tests: they run the real `sqlh` binary and check exit codes, files and
//! output, which unit tests can't (PLAN.md Phase 1). Run them against the release binary
//! too: `cargo test --release --test cli`.
//!
//! Every test is isolated from the machine: no real profiles, no SQLCMD* variables. Tests
//! that need no server use an unreachable address, so "the guard let it through" shows as
//! exit 2 (connection failed) and "blocked" as exit 5.
//!
//! The `live` tests are ignored unless asked for (see that module).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEAD: &[&str] = &["-S", "127.0.0.1,1", "-l", "1"];

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sqlh() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
    // A profiles file that doesn't exist: no profiles, and the real one is never touched.
    let no_profiles = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-such-profiles.toml");
    command.env("SQLH_CONFIG", no_profiles);
    for var in [
        "SQLH_PROFILE",
        "SQLCMDSERVER",
        "SQLCMDDBNAME",
        "SQLCMDUSER",
        "SQLCMDPASSWORD",
    ] {
        command.env_remove(var);
    }
    command.stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> Run {
    let output = command.output().expect("sqlh runs");
    Run {
        code: output.status.code().expect("an exit code, not a signal"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn sqlh_args(args: &[&str]) -> Run {
    run(sqlh().args(args))
}

/// A stream whose reader is already gone, so every write to it fails: a harness that
/// stopped reading. Unlike closing a pipe after spawning, this can't race the child.
fn closed() -> Stdio {
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    Stdio::from(writer)
}

/// Runs with stdout and/or stderr closed; the other streams are captured.
fn run_closed(command: &mut Command, stdout_closed: bool, stderr_closed: bool) -> Run {
    command.stdout(if stdout_closed {
        closed()
    } else {
        Stdio::piped()
    });
    command.stderr(if stderr_closed {
        closed()
    } else {
        Stdio::piped()
    });
    run(command)
}

/// A fresh directory for one test, under target/tmp.
fn scratch(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("cli")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The file names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("readable dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or("")
}

/// The run got past the guard and failed only at the dead address. Which timer fires first
/// decides between exit 2 (TCP connect timed out) and exit 4 (login deadline).
fn assert_reached_connect(r: &Run, what: &str) {
    assert!(
        matches!(r.code, 2 | 4),
        "{what}: exit {}: {}",
        r.code,
        r.stdout
    );
}

#[test]
fn usage_errors_are_exit_3_with_a_status_line() {
    let r = sqlh_args(&["--no-such-flag"]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(last_line(&r.stdout).starts_with("-- failed (exit 3): usage error"));
    assert_eq!(sqlh_args(&["--help"]).code, 0);
}

// F01: the SQL file must survive `-o` naming it, however the path is spelled.
#[test]
fn output_onto_an_input_file_is_refused() {
    let dir = scratch("f01_same_file");
    let sql = dir.join("a.sql");
    fs::write(&sql, "SELECT 123;").unwrap();
    let alias = dir.join(".").join("A.SQL");
    let link = dir.join("link.sql");
    fs::hard_link(&sql, &link).unwrap();
    for out in [&sql, &alias, &link] {
        let mut args: Vec<String> = DEAD.iter().map(|s| s.to_string()).collect();
        args.extend([
            "-i".into(),
            sql.display().to_string(),
            "-o".into(),
            out.display().to_string(),
        ]);
        let r = run(sqlh().args(&args));
        assert_eq!(r.code, 3, "-o {}: {}", out.display(), r.stdout);
        assert!(
            r.stdout.contains("would overwrite the input file"),
            "{}",
            r.stdout
        );
        assert_eq!(
            fs::read_to_string(&sql).unwrap(),
            "SELECT 123;",
            "-o {}",
            out.display()
        );
    }
}

// F01, R01: a run that fails before it starts leaves an existing output file alone; one
// that runs replaces it whole. Neither leaves a .partial file behind or touches one that
// was already there.
#[test]
fn output_files_are_replaced_whole_or_not_at_all() {
    let dir = scratch("f01_atomic");
    let out = dir.join("out.txt");
    fs::write(&out, "previous").unwrap();
    let squatter = dir.join("out.txt.partial");
    fs::write(&squatter, "not ours").unwrap();
    let missing = dir.join("missing.sql");
    let r = sqlh_args(&[
        "-i",
        &missing.display().to_string(),
        "-o",
        &out.display().to_string(),
    ]);
    assert_eq!(r.code, 3);
    assert_eq!(fs::read_to_string(&out).unwrap(), "previous");

    let r = sqlh_args(&[
        "-Q",
        "UPDATE dbo.T SET x = 1",
        "-o",
        &out.display().to_string(),
    ]);
    assert_eq!(r.code, 5);
    let written = fs::read_to_string(&out).unwrap();
    assert!(
        written.starts_with("-- blocked: line 1: UPDATE dbo.T would write"),
        "{written}"
    );
    assert!(
        last_line(&written).starts_with("-- failed (exit 5)"),
        "{written}"
    );
    assert_eq!(fs::read_to_string(&squatter).unwrap(), "not ours");
    assert_eq!(names(&dir), ["out.txt", "out.txt.partial"]);
}

// R01: concurrent runs never share a .partial file. A run still writing can't change what
// another run already published, and the last run to publish replaces the file whole.
#[test]
fn concurrent_runs_publish_whole_files() {
    let dir = scratch("r01_concurrent");
    let out = dir.join("out.txt");
    let out_arg = out.display().to_string();
    // Accepts connections and never answers, so run A waits for its login deadline.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("127.0.0.1,{}", listener.local_addr().unwrap().port());
    let a = sqlh()
        .args(["-S", &server, "-l", "2", "-Q", "SELECT 1", "-o", &out_arg])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    // A opens its .partial file before it connects.
    let (_held, _) = listener.accept().expect("run A connects");
    assert_eq!(names(&dir).len(), 1, "{:?}", names(&dir));

    let b = sqlh_args(&["-Q", "DELETE FROM dbo.T", "-o", &out_arg]);
    assert_eq!(b.code, 5, "{}", b.stderr);
    let published_by_b = fs::read_to_string(&out).unwrap();
    assert!(
        published_by_b.starts_with("-- blocked: line 1: DELETE dbo.T would write")
            && last_line(&published_by_b).starts_with("-- failed (exit 5)"),
        "{published_by_b}"
    );

    let a = a.wait_with_output().unwrap();
    let a_code = a.status.code().unwrap();
    let a_stderr = String::from_utf8_lossy(&a.stderr);
    assert!(matches!(a_code, 2 | 4), "run A: exit {a_code}: {a_stderr}");
    let published_by_a = fs::read_to_string(&out).unwrap();
    assert!(
        published_by_a.starts_with("-- error: ")
            && !published_by_a.contains("blocked")
            && last_line(&published_by_a).starts_with(&format!("-- failed (exit {a_code})")),
        "{published_by_a}"
    );
    assert_eq!(names(&dir), ["out.txt"]);
}

// R01: an existing file that can't be replaced is refused before anything runs, or, if it
// only becomes clear at the end, the run is exit 7 and the file is left exactly as it was.
#[cfg(windows)]
#[test]
fn a_file_that_cannot_be_replaced_is_left_alone() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    let dir = scratch("r01_held");
    let out = dir.join("out.txt");
    fs::write(&out, "previous").unwrap();
    let out_arg = out.display().to_string();
    let hold = |share| {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(share)
            .open(&out)
            .unwrap()
    };

    // Held without write sharing: refused up front.
    let held = hold(FILE_SHARE_READ);
    let r = sqlh_args(&["-Q", "UPDATE dbo.T SET x = 1", "-o", &out_arg]);
    drop(held);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert_eq!(fs::read_to_string(&out).unwrap(), "previous");

    // Writable but not deletable: the run happens, and moving the result into place fails.
    let held = hold(FILE_SHARE_READ | FILE_SHARE_WRITE);
    let r = sqlh_args(&["-Q", "UPDATE dbo.T SET x = 1", "-o", &out_arg]);
    drop(held);
    assert_eq!(r.code, 7, "{}", r.stderr);
    assert!(
        r.stderr.contains("could not be delivered")
            && r.stderr.contains("the run itself ended with exit 5"),
        "{}",
        r.stderr
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "previous");
    assert_eq!(names(&dir), ["out.txt"]);
}

// -o to a device or pipe writes straight to it: there is no file to replace.
#[test]
fn output_to_a_device_is_written_directly() {
    let r = sqlh_args(&["-Q", "UPDATE dbo.T SET x = 1", "-o", "NUL"]);
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert_eq!(r.stderr, "");
}

// F02: if the reader goes away, the output wasn't delivered: exit 7, never 0.
#[test]
fn undelivered_output_is_exit_7() {
    for format in ["text", "json"] {
        let r = run_closed(sqlh().args(["-Q", "", "--format", format]), true, false);
        assert_eq!(r.code, 7, "--format {format}: {}", r.stderr);
        assert!(
            r.stderr.contains("output could not be delivered"),
            "{}",
            r.stderr
        );
    }
}

// R02: every delivery failure is exit 7, and a diagnostic that can't be written never
// crashes sqlh or changes its exit code. Each case: arguments, stdout closed, stderr
// closed, expected exit code.
#[test]
fn closed_streams_give_exact_exit_codes() {
    let blocked = "UPDATE dbo.T SET x = 1";
    let cases: &[(&[&str], bool, bool, i32)] = &[
        (&["--help"], true, false, 7),
        (&["--version"], true, false, 7),
        (&["--help"], false, true, 0),
        (&["--no-such-flag"], true, true, 7),
        (&["-Q", ""], true, false, 7),
        (&["-Q", ""], false, true, 0),
        (&["-Q", ""], true, true, 7),
        (&["-Q", blocked], true, true, 7),
        (&["-Q", "", "--format", "json"], true, false, 7),
        (&["-Q", "", "--format", "json"], false, true, 0),
        (&["-Q", "", "--format", "json"], true, true, 7),
        // csv's metadata and status go to stderr; with no rows, stdout gets nothing.
        (&["-Q", "", "--format", "csv"], false, true, 7),
        (&["-Q", "", "--format", "csv"], true, true, 7),
        (&["-Q", "", "--format", "csv"], true, false, 0),
        (&["-Q", blocked, "--format", "csv"], false, true, 7),
    ];
    for (args, stdout_closed, stderr_closed, want) in cases {
        let r = run_closed(sqlh().args(*args), *stdout_closed, *stderr_closed);
        assert_eq!(
            r.code, *want,
            "{args:?}, stdout closed: {stdout_closed}, stderr closed: {stderr_closed}\n{}{}",
            r.stdout, r.stderr
        );
    }
}

// F05: malformed input never crashes sqlh.
#[test]
fn malformed_input_does_not_crash() {
    let mut args = DEAD.to_vec();
    args.extend(["-Q", "SELECT [é"]);
    let r = sqlh_args(&args);
    assert_reached_connect(&r, "SELECT [é");
    assert!(
        last_line(&r.stdout).starts_with("-- failed (exit "),
        "{}",
        r.stdout
    );
}

// The panic net: a bug still exits 10 rather than aborting (0xC0000409), with a status
// line when stdout can take one. SQLH_DEBUG_PANIC exists only in debug builds, so these
// tests do too; a release build has the same hook but no way to trigger it.
#[cfg(debug_assertions)]
#[test]
fn a_panic_still_exits_10_with_a_status_line() {
    let mut args = DEAD.to_vec();
    args.extend(["-Q", "SELECT 1"]);
    let r = run(sqlh().args(&args).env("SQLH_DEBUG_PANIC", "1"));
    assert_eq!(r.code, 10, "{}", r.stderr);
    assert_eq!(last_line(&r.stdout), "-- failed (exit 10): internal error");
    assert!(
        r.stderr
            .contains("internal error: forced by SQLH_DEBUG_PANIC"),
        "{}",
        r.stderr
    );
}

// R02: the panic net holds even when the streams it reports to are gone.
#[cfg(debug_assertions)]
#[test]
fn a_panic_with_closed_streams_still_exits_10() {
    let mut args = DEAD.to_vec();
    args.extend(["-Q", "SELECT 1"]);
    for (stdout_closed, stderr_closed) in [(true, false), (false, true), (true, true)] {
        let mut command = sqlh();
        command.args(&args).env("SQLH_DEBUG_PANIC", "1");
        let r = run_closed(&mut command, stdout_closed, stderr_closed);
        assert_eq!(
            r.code, 10,
            "stdout closed: {stdout_closed}, stderr closed: {stderr_closed}"
        );
    }
}

// F04: --check refuses input that could turn compile-only mode off, even with writes allowed.
#[test]
fn check_refuses_noexec() {
    for extra in [&[][..], &["--allow-write"][..]] {
        for sql in [
            "SET NOEXEC OFF; SELECT 1",
            "SET NOEXEC OFF\nGO\nINSERT dbo.T VALUES (1)",
        ] {
            let mut args = DEAD.to_vec();
            args.extend(["--check", "-Q", sql]);
            args.extend(extra);
            let r = sqlh_args(&args);
            assert_eq!(r.code, 5, "{sql} {extra:?}: {}", r.stdout);
            assert!(
                r.stdout.contains("SET NOEXEC can't be used with --check"),
                "{}",
                r.stdout
            );
        }
    }
}

// R04: the advice fits the refusal. --allow-write can't lift this one, so no format may
// suggest it.
#[test]
fn noexec_refusal_advice_fits_in_every_format() {
    for format in ["text", "json", "csv"] {
        let mut args = DEAD.to_vec();
        args.extend([
            "--check",
            "--allow-write",
            "--format",
            format,
            "-Q",
            "SET NOEXEC OFF",
        ]);
        let r = sqlh_args(&args);
        assert_eq!(r.code, 5, "{format}: {}", r.stdout);
        let shown = if format == "csv" {
            &r.stderr
        } else {
            &r.stdout
        };
        assert!(
            shown.contains("nothing was run. Remove SET NOEXEC"),
            "{format}: {shown}"
        );
        assert!(
            !shown.contains("Rerun with --allow-write"),
            "{format}: {shown}"
        );
    }
}

// F03 at process level (the cases themselves are unit-tested in guard.rs).
#[test]
fn sequences_and_global_temp_writes_are_blocked() {
    for sql in [
        "SELECT NEXT VALUE FOR dbo.S",
        "INSERT INTO ##shared VALUES (1)",
        "EXEC Lnk.master.sys.sp_who",
    ] {
        let r = sqlh_args(&["-Q", sql]);
        assert_eq!(r.code, 5, "{sql}: {}", r.stdout);
    }
}

// F10: a truncated UTF-16 file is an error, not a silent loss of the last byte.
#[test]
fn odd_length_utf16_is_rejected() {
    let dir = scratch("f10_utf16");
    let file = dir.join("u16.sql");
    fs::write(&file, [0xFF, 0xFE, 0x53, 0x00, 0xFF]).unwrap();
    let r = sqlh_args(&["-i", &file.display().to_string()]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stdout.contains("odd number of bytes"), "{}", r.stdout);
}

// F09: a Windows-auth profile stays Windows auth whatever SQLCMDUSER says.
#[test]
fn windows_profile_ignores_sqlcmduser() {
    let dir = scratch("f09_auth");
    let config = dir.join("profiles.toml");
    fs::write(
        &config,
        "[profiles.win]\nserver = \"127.0.0.1,1\"\nauth = \"windows\"\nlogin_timeout = 1\n",
    )
    .unwrap();
    let r = run(sqlh()
        .args(["--profile", "win", "-Q", "SELECT 1"])
        .env("SQLH_CONFIG", &config)
        .env("SQLCMDUSER", "environment_login"));
    assert_reached_connect(&r, "windows profile");
    assert!(r.stdout.contains("(Windows auth,"), "{}", r.stdout);
}

mod live {
    //! Against the owner's dev/test server, through a local profile. They are ignored by a
    //! plain `cargo test`, which reports them as ignored, and fail if asked for without a
    //! profile:
    //!
    //!     $env:SQLH_TEST_PROFILE='test'; cargo test --test cli -- --ignored live
    //!
    //! They use the scratch database SQLH_TEST_DATABASE (default `sqlh_test`), created by
    //! `tests/fixtures/sqlh_test.sql`. Before writing, a test checks the database's
    //! `dbo.sqlh_scratch` marker; it writes only to a table of its own, which it drops.

    use super::*;

    fn target() -> (String, String) {
        let profile = std::env::var("SQLH_TEST_PROFILE")
            .ok()
            .filter(|p| !p.is_empty())
            .expect("live tests need SQLH_TEST_PROFILE, the name of a dev/test server profile");
        let database = std::env::var("SQLH_TEST_DATABASE").unwrap_or_else(|_| "sqlh_test".into());
        (profile, database)
    }

    /// Like `sqlh()`, but with the real profiles file so the test profile resolves.
    fn live(profile: &str, database: &str, args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command
            .stdin(Stdio::null())
            .args(["--profile", profile, "-d", database])
            .args(args);
        run(&mut command)
    }

    /// A table for one test, in a database marked as scratch; dropped when it goes out of
    /// scope, whether the test passed or not.
    struct Probe {
        profile: String,
        database: String,
        name: String,
    }

    impl Probe {
        fn create(profile: &str, database: &str) -> Probe {
            let marker = live(
                profile,
                database,
                &[
                    "-Q",
                    "SELECT CASE WHEN OBJECT_ID(N'dbo.sqlh_scratch', N'U') IS NULL \
                     THEN 0 ELSE 1 END AS scratch",
                ],
            );
            assert!(
                marker.stdout.contains("scratch:int\n1\n"),
                "{database} has no dbo.sqlh_scratch marker, so the live tests won't write to it; \
                 create the scratch database with tests/fixtures/sqlh_test.sql\n{}",
                marker.stdout
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!("dbo.probe_{}_{nanos}", std::process::id());
            let sql = format!("CREATE TABLE {name} (id int NOT NULL)");
            let r = live(profile, database, &["--allow-write", "-Q", &sql]);
            assert_eq!(r.code, 0, "{}", r.stdout);
            Probe {
                profile: profile.into(),
                database: database.into(),
                name,
            }
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let sql = format!("DROP TABLE {}", self.name);
            let _ = live(
                &self.profile,
                &self.database,
                &["--allow-write", "-Q", &sql],
            );
        }
    }

    // F04, end to end: whatever the input says, --check executes nothing.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn check_never_executes() {
        let (p, db) = target();
        let probe = Probe::create(&p, &db);
        let t = &probe.name;
        let insert = format!("INSERT {t} VALUES (1)");
        let checked = live(&p, &db, &["--check", "--allow-write", "-Q", &insert]);
        assert_eq!(checked.code, 0, "{}", checked.stdout);
        let escape = format!("SET NOEXEC OFF\nGO\nINSERT {t} VALUES (2)");
        let escaped = live(&p, &db, &["--check", "--allow-write", "-Q", &escape]);
        assert_eq!(escaped.code, 5, "{}", escaped.stdout);
        let count = live(&p, &db, &["-Q", &format!("SELECT COUNT(*) AS n FROM {t}")]);
        assert!(count.stdout.contains("n:int\n0\n"), "{}", count.stdout);
    }

    // F11, end to end.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn hostile_column_names_are_escaped() {
        let (p, db) = target();
        let r = live(&p, &db, &["-Q", "SELECT 1 AS [-- forged], 2 AS [a\tb]"]);
        assert!(
            r.stdout.contains("\n\\-- forged:int\ta\\tb:int\n"),
            "{}",
            r.stdout
        );
    }

    // F10, end to end: a variable can't split a batch, and multi-line values keep error
    // lines pointing at the source.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn substitution_keeps_batches_and_lines() {
        let (p, db) = target();
        let r = live(
            &p,
            &db,
            &["-v", "sep=GO", "-Q", "SELECT 1 AS a\n$(sep)\nSELECT 2 AS b"],
        );
        assert!(
            r.stdout.contains("Incorrect syntax near 'GO'"),
            "{}",
            r.stdout
        );
        let r = live(
            &p,
            &db,
            &[
                "-v",
                "cols=1 AS a,\n2 AS b",
                "-Q",
                "SELECT\n$(cols),\nno_such_column\nFROM sys.objects",
            ],
        );
        assert!(
            r.stdout
                .contains("error 207 (severity 16, state 1, line 3)"),
            "{}",
            r.stdout
        );
    }
}
