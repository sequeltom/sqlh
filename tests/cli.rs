//! Process-level tests: they run the real `sqlh` binary and check exit codes, files and
//! output, which unit tests can't (PLAN.md Phase 1).
//!
//! Every test is isolated from the machine: no real profiles, no SQLCMD* variables. Tests
//! that need no server use an unreachable address, so "the guard let it through" shows as
//! exit 2 (connection failed) and "blocked" as exit 5.
//!
//! Live tests skip themselves unless SQLH_TEST_PROFILE names a profile on this machine.
//! They use the scratch database SQLH_TEST_DATABASE (default `sqlh_test`), created by
//! `tests/fixtures/sqlh_test.sql`.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
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

/// A fresh directory for one test, under target/tmp.
fn scratch(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("cli")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
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

// F01: a run that fails before it starts leaves an existing output file alone; one that
// runs replaces it whole, with no .partial file left behind.
#[test]
fn output_files_are_replaced_whole_or_not_at_all() {
    let dir = scratch("f01_atomic");
    let out = dir.join("out.txt");
    fs::write(&out, "previous").unwrap();
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
    assert!(!dir.join("out.txt.partial").exists());
}

// F02: if the reader goes away, the output wasn't delivered: exit 7, never 0.
#[test]
fn undelivered_output_is_exit_7() {
    for format in ["text", "json"] {
        let mut child = sqlh()
            .args(["-Q", "", "--format", format])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sqlh starts");
        drop(child.stdout.take()); // the reader closes before sqlh writes anything
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7), "--format {format}: {stderr}");
        assert!(stderr.contains("output could not be delivered"), "{stderr}");
    }
}

// F05, and the panic net: malformed input never crashes; a bug still exits 10 with a
// status line instead of aborting (0xC0000409).
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

#[test]
fn a_panic_still_exits_10_with_a_status_line() {
    let r = run(sqlh().args(["-Q", "SELECT 1"]).env("SQLH_DEBUG_PANIC", "1"));
    assert_eq!(r.code, 10, "{}", r.stderr);
    assert_eq!(last_line(&r.stdout), "-- failed (exit 10): internal error");
    assert!(
        r.stderr
            .contains("internal error: forced by SQLH_DEBUG_PANIC"),
        "{}",
        r.stderr
    );
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
    //! Against the owner's dev/test server, through a local profile. Skipped unless
    //! SQLH_TEST_PROFILE is set.

    use super::*;

    fn target() -> Option<(String, String)> {
        let profile = std::env::var("SQLH_TEST_PROFILE")
            .ok()
            .filter(|p| !p.is_empty())?;
        let database = std::env::var("SQLH_TEST_DATABASE").unwrap_or_else(|_| "sqlh_test".into());
        Some((profile, database))
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

    // F04, end to end: whatever the input says, --check executes nothing.
    #[test]
    fn check_never_executes() {
        let Some((p, db)) = target() else { return };
        let reset = live(
            &p,
            &db,
            &["--allow-write", "-Q", "DELETE FROM dbo.check_probe"],
        );
        assert_eq!(reset.code, 0, "{}", reset.stdout);
        let checked = live(
            &p,
            &db,
            &[
                "--check",
                "--allow-write",
                "-Q",
                "INSERT dbo.check_probe VALUES (1)",
            ],
        );
        assert_eq!(checked.code, 0, "{}", checked.stdout);
        let escaped = live(
            &p,
            &db,
            &[
                "--check",
                "--allow-write",
                "-Q",
                "SET NOEXEC OFF\nGO\nINSERT dbo.check_probe VALUES (2)",
            ],
        );
        assert_eq!(escaped.code, 5, "{}", escaped.stdout);
        let count = live(
            &p,
            &db,
            &["-Q", "SELECT COUNT(*) AS n FROM dbo.check_probe"],
        );
        assert!(count.stdout.contains("n:int\n0\n"), "{}", count.stdout);
    }

    // F11, end to end.
    #[test]
    fn hostile_column_names_are_escaped() {
        let Some((p, db)) = target() else { return };
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
    fn substitution_keeps_batches_and_lines() {
        let Some((p, db)) = target() else { return };
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
