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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// The run got past the guard and failed only at the dead address (exit 2: the TCP connect
/// gets -l seconds, the login deadline one more).
fn assert_reached_connect(r: &Run, what: &str) {
    assert_eq!(r.code, 2, "{what}: {}", r.stdout);
}

/// Waits for `child`, killing it and failing the test after `limit`.
fn wait_within(
    child: &mut std::process::Child,
    limit: Duration,
    what: &str,
) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("child status") {
            return status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("{what}: sqlh still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Starts sqlh in a process group of its own, so `ctrl_break` reaches it and not the tests.
#[cfg(windows)]
fn spawn_in_group(command: &mut Command) -> std::process::Child {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    command
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .expect("sqlh starts")
}

/// Presses Ctrl+Break for a child started by `spawn_in_group`. That needs a console shared
/// with the child, which `cargo test` run from a terminal has.
#[cfg(windows)]
fn ctrl_break(child: &std::process::Child) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // SAFETY: takes an event code and a process group id by value.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert_ne!(
        sent,
        0,
        "Ctrl+Break wasn't sent (run the tests from a console): {}",
        std::io::Error::last_os_error()
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
    let a_stderr = String::from_utf8_lossy(&a.stderr);
    assert_eq!(a.status.code(), Some(4), "run A: {a_stderr}");
    let published_by_a = fs::read_to_string(&out).unwrap();
    assert!(
        published_by_a.starts_with("-- error: login timeout: ")
            && !published_by_a.contains("blocked")
            && last_line(&published_by_a).starts_with("-- failed (exit 4)"),
        "{published_by_a}"
    );
    assert_eq!(names(&dir), ["out.txt"]);
}

// PLAN.md 2.4: a refused connection fails after one attempt (it took 14 s with the driver's
// retries), and a server that takes the connection but never answers hits the login deadline,
// -l plus one second. Neither depends on which timer fires first.
#[test]
fn connect_failures_end_the_same_way_every_time() {
    let started = Instant::now();
    let r = sqlh_args(&["-S", "127.0.0.1,1", "-Q", "SELECT 1"]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("-- error: cannot connect to 127.0.0.1,1 "),
        "{}",
        r.stdout
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    // -l 0 is no limit, as in sqlcmd: the refusal itself ends the attempt, not a timer.
    let r = sqlh_args(&["-S", "127.0.0.1,1", "-l", "0", "-Q", "SELECT 1"]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    assert!(!r.stdout.contains("timed out"), "{}", r.stdout);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("127.0.0.1,{}", listener.local_addr().unwrap().port());
    let mut child = sqlh()
        .args(["-S", &server, "-l", "1", "-Q", "SELECT 1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    let (_held, _) = listener.accept().expect("sqlh connects");
    let status = wait_within(&mut child, Duration::from_secs(10), "silent server");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert_eq!(status.code(), Some(4), "{stdout}");
    assert!(
        stdout.starts_with(&format!(
            "-- error: login timeout: {server} (Windows auth, encrypt mandatory) didn't \
             complete the login within 2 s;"
        )),
        "{stdout}"
    );
}

// PLAN.md 2.4: Ctrl+Break ends a connect that is waiting on the server, at once, with exit 6.
#[cfg(windows)]
#[test]
fn ctrl_break_cancels_a_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("127.0.0.1,{}", listener.local_addr().unwrap().port());
    let mut child = spawn_in_group(
        sqlh()
            .args(["-S", &server, "-l", "30", "-Q", "SELECT 1"])
            .stdout(Stdio::piped()),
    );
    // Connected, so the console handlers are in place, and waiting for the login answer.
    let (_held, _) = listener.accept().expect("sqlh connects");
    let started = Instant::now();
    ctrl_break(&child);
    let status = wait_within(&mut child, Duration::from_secs(10), "Ctrl+Break");
    let elapsed = started.elapsed();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert_eq!(status.code(), Some(6), "{stdout}");
    assert!(
        stdout.starts_with("-- error: cancelled by Ctrl+Break: cancelled while connecting\n"),
        "{stdout}"
    );
    assert!(
        last_line(&stdout).starts_with("-- failed (exit 6): cancelled, "),
        "{stdout}"
    );
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

// Phase 2 review, answer 1: a Named Pipes request to another machine is refused before
// anything connects, however the server is named. The driver's pipe open can't be timed out or
// cancelled: it held a run for 21 s, past -l and Ctrl+C. With -l 30, a run that tried would
// still be going; exit 3 within moments shows that nothing was tried.
#[test]
fn remote_named_pipes_are_refused_before_anything_connects() {
    // The text format escapes a backslash as two.
    let escaped = |text: &str| text.replace('\\', r"\\");
    let started = Instant::now();
    for (server, tcp) in [
        ("np:192.0.2.1", "192.0.2.1"),
        (r"NP:db01\SALES", r"db01\SALES"),
        (r"\\192.0.2.1\pipe\sql\query", "192.0.2.1"),
        (r"tcp:\\db01\pipe\MSSQL$SALES\sql\query", r"db01\SALES"),
    ] {
        let r = sqlh_args(&["-S", server, "-l", "30", "-Q", "SELECT 1"]);
        assert_eq!(r.code, 3, "{server}: {}", r.stdout);
        let expected = format!(
            "-- error: -S {}: Named Pipes to a remote server aren't supported, because their \
             open can't be timed out or cancelled; connect over TCP: -S {}\n\
             -- failed (exit 3): usage error (see sqlh --help), ",
            escaped(server),
            escaped(tcp)
        );
        assert!(r.stdout.starts_with(&expected), "{server}: {}", r.stdout);
    }

    // The server can come from SQLCMDSERVER or a profile, and the message says which.
    let r = run(sqlh()
        .env("SQLCMDSERVER", "np:db01")
        .args(["-l", "30", "-Q", "SELECT 1"]));
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("-- error: SQLCMDSERVER np:db01: Named Pipes to a remote server"),
        "{}",
        r.stdout
    );
    let dir = scratch("remote_pipe_profile");
    let config = dir.join("profiles.toml");
    fs::write(
        &config,
        "[profiles.old]\nserver = \"np:db01\"\nauth = \"windows\"\nlogin_timeout = 30\n",
    )
    .unwrap();
    let r = run(sqlh()
        .env("SQLH_CONFIG", &config)
        .args(["--profile", "old", "-Q", "SELECT 1"]));
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("-- error: profile old server np:db01: Named Pipes to a remote server"),
        "{}",
        r.stdout
    );
    // A flag replaces the profile's server, so the profile's pipe no longer matters.
    let r = run(sqlh().env("SQLH_CONFIG", &config).args([
        "--profile",
        "old",
        "-S",
        "127.0.0.1,1",
        "-l",
        "1",
        "-Q",
        "SELECT 1",
    ]));
    assert_reached_connect(&r, "a flag replaces the profile's server");

    // `profile add` refuses it too, before it asks for a password or saves anything.
    let saved = dir.join("saved.toml");
    let r = run(sqlh()
        .env("SQLH_CONFIG", &saved)
        .args(["profile", "add", "dev", "-S", "np:db01", "-U", "reader"]));
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("-- error: -S np:db01: Named Pipes to a remote server"),
        "{}",
        r.stdout
    );
    assert!(!saved.exists(), "no profile was saved");

    // Local pipes aren't refused: they get as far as the driver, and whatever it says.
    for server in [
        "np:.",
        "NP:localhost",
        r"\\.\pipe\sql\query",
        "lpc:.",
        "tcp:127.0.0.1,1",
    ] {
        let r = sqlh_args(&["-S", server, "-l", "1", "-Q", "SELECT 1"]);
        assert_ne!(r.code, 3, "{server}: {}", r.stdout);
    }
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
}

// Phase 2 review, answer 2: a name can have several addresses, and they are tried in turn. On
// a dual-stack machine `localhost` has two, ::1 and 127.0.0.1, and a refused attempt at each takes
// about 2 s. Each TCP attempt gets -l seconds and the whole login one more. If every attempt
// fails before that deadline the run is exit 2 (4 s here); if the deadline expires first, even
// during a later address's attempt, it is exit 4. The four runs go at once, as they don't share
// anything: the name with the default -l, the name with -l 2, and each address alone with -l 2.
#[test]
fn a_name_with_several_addresses_is_tried_in_turn_until_the_login_deadline() {
    use std::net::ToSocketAddrs;
    let addresses = ("localhost", 1)
        .to_socket_addrs()
        .map_or(0, Iterator::count);
    if addresses < 2 {
        eprintln!("skipped: localhost has {addresses} address(es) here, and the test needs two");
        return;
    }
    let timed = |args: &[&str]| {
        let started = Instant::now();
        let r = sqlh_args(args);
        (r, started.elapsed())
    };
    let ((both, both_time), (cut, cut_time), (v4, v4_time), (v6, v6_time)) =
        std::thread::scope(|scope| {
            let both = scope.spawn(|| timed(&["-S", "localhost,1", "-Q", "SELECT 1"]));
            let cut = scope.spawn(|| timed(&["-S", "localhost,1", "-l", "2", "-Q", "SELECT 1"]));
            let v4 = scope.spawn(|| timed(&["-S", "127.0.0.1,1", "-l", "2", "-Q", "SELECT 1"]));
            let v6 = scope.spawn(|| timed(&["-S", "::1,1", "-l", "2", "-Q", "SELECT 1"]));
            (
                both.join().unwrap(),
                cut.join().unwrap(),
                v4.join().unwrap(),
                v6.join().unwrap(),
            )
        });

    // Each address alone with -l 2 is exit 2, whatever it takes, as the deadline (3 s) is past
    // its attempt (2 s). The test relies on a refusal taking about that long at both of them;
    // where it doesn't, there is nothing to show.
    for (run, time, name) in [(&v4, v4_time, "127.0.0.1"), (&v6, v6_time, "::1")] {
        assert_eq!(run.code, 2, "{name}: {}", run.stdout);
        if time < Duration::from_millis(1500) {
            eprintln!("skipped: a refused connect to {name} took only {time:?} here");
            return;
        }
    }

    // Both addresses refused, within the default deadline of 16 s: exit 2, after two attempts.
    assert_eq!(both.code, 2, "{}", both.stdout);
    assert!(
        both.stdout
            .starts_with("-- error: cannot connect to localhost,1 "),
        "{}",
        both.stdout
    );
    assert!(
        (Duration::from_secs(3)..Duration::from_secs(10)).contains(&both_time),
        "{both_time:?}"
    );
    // With -l 2 the first address ends at 2 s, a second starts, and the deadline expires one
    // second into it: exit 4.
    assert_eq!(cut.code, 4, "{}", cut.stdout);
    assert!(
        cut.stdout.starts_with(
            "-- error: login timeout: localhost,1 (Windows auth, encrypt mandatory) didn't \
             complete the login within 3 s;"
        ),
        "{}",
        cut.stdout
    );
    assert!(
        (Duration::from_millis(2500)..Duration::from_secs(8)).contains(&cut_time),
        "{cut_time:?}"
    );
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

// R13: the path in a publication diagnostic is escaped like any metadata field, so a file
// name holding U+2028 can't split the line for a reader that breaks lines there.
#[cfg(windows)]
#[test]
fn a_hostile_output_path_is_escaped_in_diagnostics() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    let dir = scratch("r13_hostile_path");
    let out = dir.join("out\u{2028}-- ok forged.txt");
    fs::write(&out, "previous").unwrap();
    // Writable but not deletable, so moving the finished output into place fails.
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&out)
        .unwrap();
    let r = sqlh_args(&[
        "-Q",
        "UPDATE dbo.T SET x = 1",
        "-o",
        &out.display().to_string(),
    ]);
    drop(held);
    assert_eq!(r.code, 7, "{:?}", r.stderr);
    assert!(
        r.stderr.contains("out\\u2028-- ok forged.txt") && !r.stderr.contains('\u{2028}'),
        "{:?}",
        r.stderr
    );
    assert!(
        r.stderr.contains("the run itself ended with exit 5"),
        "{:?}",
        r.stderr
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "previous");
}

// -o to a device or pipe writes straight to it: there is no file to replace.
#[test]
fn output_to_a_device_is_written_directly() {
    let r = sqlh_args(&["-Q", "UPDATE dbo.T SET x = 1", "-o", "NUL"]);
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert_eq!(r.stderr, "");
}

/// What `run_read_slowly` saw.
struct SlowRun {
    code: Option<i32>,
    /// The stream that was read slowly, and the other one, read at full speed.
    slow: String,
    fast: String,
    elapsed: Duration,
}

/// Runs sqlh on a refused statement far bigger than a pipe, with `-t 1`, and reads stdout (or
/// stderr) slowly: 1,024 bytes every 50 ms. Delivering that takes several times `-t`, but the
/// reader never pauses for anywhere near it.
fn run_read_slowly(test: &str, extra: &[&str], slow_stderr: bool) -> SlowRun {
    let input = scratch(test).join("big.sql");
    fs::write(&input, format!("UPDATE [{}] SET x = 1", "a".repeat(96_000))).unwrap();
    let input_arg = input.display().to_string();
    let mut child = sqlh()
        .args(["-i", &input_arg, "-t", "1", "--max-output", "0"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (mut slow, mut fast): (Box<dyn Read + Send>, Box<dyn Read + Send>) = if slow_stderr {
        (Box::new(stderr), Box::new(stdout))
    } else {
        (Box::new(stdout), Box::new(stderr))
    };
    let started = Instant::now();
    let slow = std::thread::spawn(move || {
        let mut received = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = slow.read(&mut buf) {
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            std::thread::sleep(Duration::from_millis(50));
        }
        received
    });
    let fast = std::thread::spawn(move || {
        let mut received = Vec::new();
        let _ = fast.read_to_end(&mut received);
        received
    });
    let status = wait_within(&mut child, Duration::from_secs(60), test);
    let elapsed = started.elapsed();
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    SlowRun {
        code: status.code(),
        slow: text(slow.join().unwrap()),
        fast: text(fast.join().unwrap()),
        elapsed,
    }
}

// R11: the stall limit (-t) is about a reader that stops reading, not one that is slow. This
// refusal is far bigger than a pipe, and the reader takes 1,024 bytes every 50 ms, so
// delivering it outlasts -t several times over. The reader never pauses for anywhere near -t,
// so the output is delivered whole, and the exit code is the guard's own (5), not 7.
#[test]
fn a_slow_reader_that_keeps_reading_is_not_a_stall() {
    let r = run_read_slowly("r11_slow_reader", &[], false);
    assert_eq!(r.code, Some(5), "{}", r.fast);
    assert_eq!(r.fast, "");
    assert!(
        r.slow.contains(&"a".repeat(96_000))
            && last_line(&r.slow).starts_with("-- failed (exit 5)"),
        "the refusal wasn't delivered whole ({} bytes)",
        r.slow.len()
    );
    assert!(
        r.elapsed > Duration::from_secs(1),
        "delivery was meant to outlast -t 1: {:?}",
        r.elapsed
    );
}

// R11: the same for csv, which writes its metadata and the refusal to stderr, a second stream
// that the one delivery thread writes in order with stdout.
#[test]
fn a_slow_reader_of_csv_metadata_is_not_a_stall() {
    let r = run_read_slowly("r11_slow_csv_reader", &["--format", "csv"], true);
    assert_eq!(r.code, Some(5), "{}", last_line(&r.slow));
    assert_eq!(r.fast, "");
    assert!(
        r.slow.contains(&"a".repeat(96_000))
            && last_line(&r.slow).starts_with("-- failed (exit 5)"),
        "the refusal wasn't delivered whole ({} bytes)",
        r.slow.len()
    );
    assert!(
        r.elapsed > Duration::from_secs(1),
        "delivery was meant to outlast -t 1: {:?}",
        r.elapsed
    );
}

// R11 keeps the other side of the line: a reader that holds stdout open and takes nothing
// beyond what the pipe holds is still a stall, exit 7 after -t. (The live suite checks the
// same with a result that fills sqlh's queue.)
#[test]
fn a_reader_that_reads_nothing_is_still_a_stall() {
    let input = scratch("r11_unread").join("big.sql");
    fs::write(&input, format!("UPDATE [{}] SET x = 1", "a".repeat(96_000))).unwrap();
    let input_arg = input.display().to_string();
    let mut child = sqlh()
        .args(["-i", &input_arg, "-t", "1", "--max-output", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    let _held_open = child.stdout.take();
    let started = Instant::now();
    let status = wait_within(&mut child, Duration::from_secs(30), "an unread stdout");
    let elapsed = started.elapsed();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(7), "{stderr}");
    assert!(
        stderr.contains("the reader stopped reading for 1 s (-t)")
            && stderr.contains("the run itself ended with exit 5"),
        "{stderr}"
    );
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
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

/// How many bytes a pipe takes before the next write blocks, measured once: a thread writes
/// one byte at a time into a scratch pipe until it blocks.
fn pipe_capacity() -> usize {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};
    static CAPACITY: OnceLock<usize> = OnceLock::new();
    *CAPACITY.get_or_init(|| {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        std::thread::spawn(move || {
            while writer.write_all(b".").is_ok() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        let mut last = usize::MAX;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let now = count.load(Ordering::SeqCst);
            if now == last && now > 0 {
                break;
            }
            last = now;
        }
        // Ends the blocked write, and with it the thread.
        drop(reader);
        last
    })
}

/// A pipe that is open, full and never read: a reader that stopped reading without closing
/// the pipe. (`closed()` is the other failure, which a write sees at once.) Filled to the
/// byte, with no write left waiting: a blocked writer would hold the pipe's file object, and
/// block the child's own queries of its stdout (`is_terminal`) at startup. Keep the reader
/// until the test is over.
fn stalled_pipe() -> (std::io::PipeReader, std::io::PipeWriter) {
    use std::io::Write;
    let (reader, mut writer) = std::io::pipe().expect("a pipe");
    for _ in 0..pipe_capacity() {
        writer.write_all(b".").expect("room in a new pipe");
    }
    (reader, writer)
}

/// Points `command`'s stdout and/or stderr at a stalled pipe, and captures the other one
/// (so it must stay small). Returns the readers: keep them until the test is over.
fn stall(
    command: &mut Command,
    stdout_stalled: bool,
    stderr_stalled: bool,
) -> Vec<std::io::PipeReader> {
    let mut readers = Vec::new();
    let mut stream = |stalled: bool| {
        if stalled {
            let (reader, writer) = stalled_pipe();
            readers.push(reader);
            Stdio::from(writer)
        } else {
            Stdio::piped()
        }
    };
    command.stdout(stream(stdout_stalled));
    command.stderr(stream(stderr_stalled));
    readers
}

/// The exit code of a finished `child`, and what it left on the streams that were captured.
fn finished(child: &mut std::process::Child, status: std::process::ExitStatus) -> Run {
    let read = |pipe: Option<&mut dyn Read>| {
        let mut text = String::new();
        if let Some(pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    };
    let stdout = read(child.stdout.as_mut().map(|p| p as &mut dyn Read));
    let stderr = read(child.stderr.as_mut().map(|p| p as &mut dyn Read));
    Run {
        code: status.code().expect("an exit code, not a signal"),
        stdout,
        stderr,
    }
}

/// Runs `command` with stdout and/or stderr stalled (see `stall`), failing the test if it is
/// still running after `limit`. Returns what it left on the other streams, and how long it
/// ran.
fn run_with_stalled(
    command: &mut Command,
    stdout_stalled: bool,
    stderr_stalled: bool,
    limit: Duration,
) -> (Run, Duration) {
    let readers = stall(command, stdout_stalled, stderr_stalled);
    let started = Instant::now();
    let mut child = command.spawn().expect("sqlh starts");
    let status = wait_within(&mut child, limit, "a stalled pipe");
    let elapsed = started.elapsed();
    let run = finished(&mut child, status);
    drop(readers);
    (run, elapsed)
}

// R09: csv sends the refusal and the status to stderr. With stderr full and never read, the
// output thread blocks in its write holding Rust's stderr lock. Reporting the stall used to
// wait for that same lock, so sqlh never got to exit 7. The diagnostic is now abandoned
// after half a second.
#[test]
fn a_stalled_stderr_in_csv_still_exits_7() {
    let (r, elapsed) = run_with_stalled(
        sqlh().args(["-Q", "DELETE FROM dbo.T", "--format", "csv", "-t", "1"]),
        false,
        true,
        Duration::from_secs(20),
    );
    assert_eq!(r.code, 7, "{}", r.stdout);
    assert_eq!(r.stdout, "", "csv's data stream carries no refusal");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

// R09: both streams stalled at once, in every format: nothing can be reported anywhere, and
// sqlh still ends on time with exit 7.
#[test]
fn stalled_stdout_and_stderr_together_still_exit_7() {
    for format in ["text", "json", "csv"] {
        let (r, elapsed) = run_with_stalled(
            sqlh().args(["-Q", "DELETE FROM dbo.T", "--format", format, "-t", "1"]),
            true,
            true,
            Duration::from_secs(20),
        );
        assert_eq!(r.code, 7, "--format {format}");
        assert!(
            elapsed < Duration::from_secs(5),
            "--format {format}: {elapsed:?}"
        );
    }
}

// R09: with a healthy stderr the diagnostic is still delivered, and says what the run
// itself ended with (the guard's refusal, exit 5) and why the output was given up on.
#[test]
fn a_stalled_stdout_is_reported_on_stderr() {
    let (r, elapsed) = run_with_stalled(
        sqlh().args(["-Q", "DELETE FROM dbo.T", "-t", "1"]),
        true,
        false,
        Duration::from_secs(20),
    );
    assert_eq!(r.code, 7, "{}", r.stderr);
    assert!(
        r.stderr.contains("the reader stopped reading for 1 s (-t)")
            && r.stderr.contains("the run itself ended with exit 5: "),
        "{}",
        r.stderr
    );
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

// R09: the panic net holds when the streams it reports to are stalled, not only when they
// are closed. Debug builds only, like every panic injection.
#[cfg(debug_assertions)]
#[test]
fn a_panic_with_stalled_streams_still_exits_10() {
    let mut args = DEAD.to_vec();
    args.extend(["-Q", "SELECT 1"]);
    let (r, elapsed) = run_with_stalled(
        sqlh().args(&args).env("SQLH_DEBUG_PANIC", "1"),
        true,
        true,
        Duration::from_secs(20),
    );
    assert_eq!(r.code, 10);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

/// R10: runs sqlh against a server that takes the connection and never answers, so the run
/// ends with its login timeout (exit 4) 2 s in (`-l 1` plus the second the deadline adds).
/// Its few closing lines then wait for a reader that stopped reading. Presses Ctrl+Break at
/// that point, and checks what the wait did with it: exit 7 and not Windows' 0xC000013A,
/// soon, and with `-t 0` too. Where stderr can be read, it must say what the run itself
/// ended with and that the signal cut the delivery short.
#[cfg(windows)]
fn ctrl_break_while_delivering(format: &str, timeout: &str) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("127.0.0.1,{}", listener.local_addr().unwrap().port());
    let mut command = sqlh();
    command.args(["-S", &server, "-E", "-l", "1", "-t", timeout]);
    command.args(["--format", format, "-Q", "SELECT 1"]);
    // The closing lines go to stdout, except in csv, which keeps stdout for data and
    // sends them to stderr.
    let csv = format == "csv";
    let readers = stall(&mut command, !csv, csv);
    let mut child = spawn_in_group(&mut command);
    let (_held, _) = listener.accept().expect("sqlh connects");
    std::thread::sleep(Duration::from_millis(3500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "--format {format}, -t {timeout}: sqlh should be waiting for its reader"
    );
    let started = Instant::now();
    ctrl_break(&child);
    let status = wait_within(&mut child, Duration::from_secs(20), "Ctrl+Break");
    let elapsed = started.elapsed();
    let r = finished(&mut child, status);
    drop(readers);
    assert_eq!(
        r.code, 7,
        "--format {format}, -t {timeout}: exit {:#x}\n{}",
        r.code, r.stderr
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "--format {format}, -t {timeout}: {elapsed:?}"
    );
    if !csv {
        assert!(
            r.stderr.contains("output could not be delivered (")
                && r.stderr
                    .contains("; cancelled by Ctrl+Break while delivering); ")
                && r.stderr
                    .contains("the run itself ended with exit 4: failed (exit 4): timeout, "),
            "--format {format}, -t {timeout}: {}",
            r.stderr
        );
    }
}

// R10: the console event listeners used to end with the run, before its closing lines were
// delivered. Ctrl+Break during that last wait then reached Windows' default handler (exit
// 0xC000013A, nothing on stderr), and with -t 0 the wait itself had no end. The runtime and
// the listeners now live through delivery, and a signal gives the rest one second.
#[cfg(windows)]
#[test]
fn ctrl_break_during_final_delivery_in_text() {
    for timeout in ["0", "60"] {
        ctrl_break_while_delivering("text", timeout);
    }
}

#[cfg(windows)]
#[test]
fn ctrl_break_during_final_delivery_in_json() {
    for timeout in ["0", "60"] {
        ctrl_break_while_delivering("json", timeout);
    }
}

// csv's closing lines are on stderr, so that is the stream that stalls, and the diagnostic
// has nowhere to go: the exit is still 7, on time.
#[cfg(windows)]
#[test]
fn ctrl_break_during_final_delivery_in_csv() {
    for timeout in ["0", "60"] {
        ctrl_break_while_delivering("csv", timeout);
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

/// What a run with `--max-output` wrote in all: stdout plus stderr.
fn response_bytes(r: &Run) -> usize {
    r.stdout.len() + r.stderr.len()
}

/// The text and csv status line is the last line of the stream that carries the metadata:
/// stdout for text, stderr for csv.
fn status_stream<'a>(format: &str, r: &'a Run) -> &'a str {
    if format == "csv" {
        &r.stderr
    } else {
        &r.stdout
    }
}

// R08: --max-output caps the whole response, not only the body. A guard refusal that quotes
// a 20,000-character identifier used to print about 20 KB under --max-output 4096 in every
// format. The refusal is shortened in its variable part; the exit code, the advice and the
// status (JSON: the outcome fields) are not.
#[test]
fn a_huge_refusal_stays_within_max_output_in_every_format() {
    let dir = scratch("r08_refusal");
    let file = dir.join("huge.sql");
    fs::write(&file, format!("UPDATE [{}] SET x = 1", "a".repeat(20_000))).unwrap();
    let path = file.display().to_string();
    for format in ["text", "json", "csv"] {
        let mut args = DEAD.to_vec();
        args.extend(["-i", &path, "--max-output", "4096", "--format", format]);
        let r = sqlh_args(&args);
        let what = format!("{format}: {} + {} bytes", r.stdout.len(), r.stderr.len());
        assert_eq!(r.code, 5, "{what}");
        assert!(response_bytes(&r) <= 4096, "{what}");
        let refusal = if format == "json" {
            assert_eq!(r.stderr, "", "{what}");
            let doc: serde_json::Value =
                serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(doc["exit_code"], 5, "{what}");
            assert_eq!(doc["ok"], false, "{what}");
            assert!(
                doc["summary"]
                    .as_str()
                    .unwrap()
                    .starts_with("failed (exit 5)"),
                "{what}: {doc}"
            );
            doc["error"].as_str().unwrap().to_owned()
        } else {
            // text keeps everything on stdout, csv puts its metadata on stderr and nothing
            // on stdout.
            let other = if format == "csv" {
                &r.stdout
            } else {
                &r.stderr
            };
            assert_eq!(other, "", "{what}");
            let shown = status_stream(format, &r);
            assert!(
                last_line(shown).starts_with("-- failed (exit 5)"),
                "{what}: {}",
                last_line(shown)
            );
            shown.lines().next().unwrap().to_owned()
        };
        assert!(refusal.contains("…[+"), "{what}: no cut marker");
        assert!(
            refusal.ends_with("nothing was run. Rerun with --allow-write if the write is intended"),
            "{what}: the advice must survive: …{}",
            &refusal[refusal.len().saturating_sub(120)..]
        );
    }
}

// R08: a long closing error is shortened the same way. A server name of 20,000 characters
// can't be resolved, and the connection error quotes it. The run still ends with its own
// exit code, and the status line says the same.
#[test]
fn a_huge_connection_error_stays_within_max_output_in_every_format() {
    let server = "h".repeat(20_000);
    for format in ["text", "json", "csv"] {
        let r = sqlh_args(&[
            "-S",
            &server,
            "-l",
            "1",
            "--max-output",
            "4096",
            "--format",
            format,
            "-Q",
            "SELECT 1",
        ]);
        let what = format!("{format}: {} + {} bytes", r.stdout.len(), r.stderr.len());
        // 2 when the name fails at once; 4 if the resolver is slow enough for the login
        // deadline. Either way the status line must agree with the exit code.
        assert!([2, 4].contains(&r.code), "{what}");
        assert!(response_bytes(&r) <= 4096, "{what}");
        let want = format!("failed (exit {})", r.code);
        if format == "json" {
            let doc: serde_json::Value =
                serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(doc["exit_code"], r.code, "{what}");
            assert!(
                doc["summary"].as_str().unwrap().starts_with(&want),
                "{what}"
            );
            assert!(doc["error"].as_str().unwrap().contains("…[+"), "{what}");
        } else {
            let shown = status_stream(format, &r);
            assert!(
                last_line(shown).starts_with(&format!("-- {want}")),
                "{what}"
            );
            assert!(shown.contains("…[+"), "{what}: no cut marker");
        }
    }
}

// R08: an error found before the run starts keeps to --max-output too, whether it comes
// from the options (a remote pipe named in -S), the input (an unknown $(var)) or -o.
#[test]
fn early_errors_stay_within_max_output_in_every_format() {
    let long = "x".repeat(20_000);
    let pipe = format!("np:{long}");
    let query = format!("SELECT $({long})");
    let out = format!("C:/nonexistent-dir-{long}/out.txt");
    let cases = [
        ("remote pipe", vec!["-S", pipe.as_str(), "-Q", "SELECT 1"]),
        ("unknown variable", vec!["-Q", query.as_str()]),
        ("-o", vec!["-Q", "SELECT 1", "-o", out.as_str()]),
    ];
    for (what, args) in &cases {
        for format in ["text", "json", "csv"] {
            let mut all = args.clone();
            all.extend(["--max-output", "4096", "--format", format]);
            let r = sqlh_args(&all);
            let shown = format!(
                "{what}, {format}: {} + {} bytes",
                r.stdout.len(),
                r.stderr.len()
            );
            assert_eq!(r.code, 3, "{shown}");
            assert!(response_bytes(&r) <= 4096, "{shown}");
            if format == "json" {
                let doc: serde_json::Value =
                    serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{shown}: {e}"));
                assert_eq!(doc["exit_code"], 3, "{shown}");
                assert!(doc["error"].as_str().unwrap().contains("…[+"), "{shown}");
            } else {
                let stream = status_stream(format, &r);
                assert!(
                    last_line(stream).starts_with("-- failed (exit 3)"),
                    "{shown}"
                );
                assert!(stream.contains("…[+"), "{shown}: no cut marker");
            }
        }
    }
}

// PLAN.md 2.11, owner decision 10: the status line says where a run went only when it got
// there. A usage error, a guard refusal and a failed connect end as they always did, in text
// and in csv (where the status line is on stderr), with no server, database or connect time.
#[test]
fn a_run_that_never_connected_has_no_connection_tail() {
    let cases = [
        ("usage error", 3, vec!["-Q", "SELECT $(nosuchvariable)"]),
        ("guard refusal", 5, vec!["-Q", "UPDATE dbo.T SET x = 1"]),
        ("failed connect", 2, [DEAD, &["-Q", "SELECT 1"]].concat()),
    ];
    for (what, code, args) in &cases {
        for format in ["text", "csv"] {
            let mut all = args.clone();
            all.extend(["--format", format]);
            let r = sqlh_args(&all);
            assert_eq!(r.code, *code, "{what}, {format}: {}", r.stdout);
            let status = last_line(status_stream(format, &r));
            assert!(
                status.starts_with(&format!("-- failed (exit {code})")) && status.ends_with(" ms"),
                "{what}, {format}: {status}"
            );
            // Neither the tail's `; server/database, connect N ms` nor anything like it.
            assert!(
                !status.contains("; ") && !status.contains("connect "),
                "{what}, {format}: {status}"
            );
        }
    }
}

// PLAN.md 2.13, owner decision 11, DESIGN.md §5.1: paths in `-- ` lines follow the escape
// rule, and input is never escaped. A path as Explorer's "Copy as path" writes it, in double
// quotes with single backslashes and spaces in the folder and file names, is taken exactly as
// typed after -i and -o in cmd.exe and in PowerShell. The folders are named so that a backslash
// is followed by an escape letter (`\new reports\tab files`, and `\tmp` is in the scratch
// path): a path that went through the output format's decoder would hold a line feed and a
// tab, and the file wouldn't be found. A blocked statement ends the run at the guard without
// connecting, so exit 5 means the input was read and the output file got the refusal. With two
// inputs the refusal names the second by the path as typed, and in the `-- ` line each
// backslash is written `\\`, which a reader decodes like any other field.
#[cfg(windows)]
#[test]
fn a_path_from_copy_as_path_works_after_i_and_o_in_every_shell() {
    let blocked = "UPDATE dbo.T SET x = 1";
    let base = scratch("copy as path");
    let shells: Vec<Shell> = [Shell::Cmd, Shell::PowerShell51, Shell::PowerShell7]
        .into_iter()
        .filter(|shell| {
            let installed = shell.installed();
            if !installed {
                eprintln!("skipped: {} isn't installed here", shell.name());
            }
            installed
        })
        .collect();
    assert!(!shells.is_empty());
    // Each shell in a thread of its own and a folder of its own: starting Windows PowerShell
    // takes a moment.
    let results: Vec<_> = shells
        .iter()
        .map(|&shell| {
            let dir = base.join(format!("shell-{}", shell as u8));
            let folder = dir.join("new reports").join("tab files");
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("in file.sql"), blocked).unwrap();
            fs::write(folder.join("read file.sql"), "SELECT 1").unwrap();
            std::thread::spawn(move || {
                // What Explorer puts on the clipboard: the full path in double quotes.
                let copied = |name: &str| {
                    let path = folder.join(name).display().to_string().replace('/', "\\");
                    (format!("\"{path}\""), path)
                };
                let (input, _) = copied("in file.sql");
                let (output, _) = copied("out file.txt");
                let single = shell.run(&format!("sqlh -i {input} -o {output}"), &dir, &[]);
                let written = fs::read_to_string(folder.join("out file.txt"));
                let left = names(&folder);

                let (read, _) = copied("read file.sql");
                let (_, second) = copied("in file.sql");
                let pair = shell.run(&format!("sqlh -i {read} -i {input}"), &dir, &[]);
                (shell, single, written, left, pair, second)
            })
        })
        .collect();
    for result in results {
        let (shell, single, written, left, pair, second) = result.join().unwrap();
        let name = shell.name();
        // -i and -o: the input was found and read, and the output file got the refusal.
        assert_eq!(single.code, 5, "{name}: {}{}", single.stdout, single.stderr);
        let written = written.unwrap_or_else(|e| panic!("{name}: no output file: {e}"));
        assert!(
            written.starts_with("-- blocked: line 1: UPDATE dbo.T would write"),
            "{name}: {written}"
        );
        assert_eq!(single.stdout, "", "{name}");
        assert_eq!(
            left,
            ["in file.sql", "out file.txt", "read file.sql"],
            "{name}: a .partial file or another file was left"
        );

        // Two inputs: the refusal names the second by its path, written with the escape rule.
        assert_eq!(pair.code, 5, "{name}: {}{}", pair.stdout, pair.stderr);
        let line = pair.stdout.lines().next().unwrap_or("");
        let field = format!(
            "-- blocked: {}:1: UPDATE dbo.T would write",
            text_escaped(&second)
        );
        assert!(line.starts_with(&field), "{name}: {line}\nwanted {field}");
        // The bytes of the input are not the bytes of the output: the path as typed is nowhere
        // in it. A reader decodes the field (`\\` is one backslash) and gets the path back.
        assert!(!pair.stdout.contains(&second), "{name}: {line}");
        let shown = &line["-- blocked: ".len()..line.find(":1: ").unwrap()];
        assert_eq!(shown.replace(r"\\", r"\"), second, "{name}");
    }
}

// R08: --verbose lines go through the output and count against --max-output. Offline the
// connection never succeeds, so no line is produced, but the budget, the exit code and the
// status line must be the same with the flag as without.
#[test]
fn verbose_runs_stay_within_max_output() {
    for format in ["text", "json", "csv"] {
        let mut args = DEAD.to_vec();
        args.extend([
            "--verbose",
            "--max-output",
            "4096",
            "--format",
            format,
            "-Q",
            "SELECT 1",
        ]);
        let r = sqlh_args(&args);
        let what = format!("{format}: {} + {} bytes", r.stdout.len(), r.stderr.len());
        assert_reached_connect(&r, &what);
        assert!(response_bytes(&r) <= 4096, "{what}");
        if format == "json" {
            let doc: serde_json::Value =
                serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(doc["exit_code"], 2, "{what}");
        } else {
            let shown = status_stream(format, &r);
            assert!(
                last_line(shown).starts_with("-- failed (exit 2)"),
                "{what}: {shown}"
            );
        }
    }
}

/// The version the binary under test was built with (`SQLH_VERSION`, else Cargo.toml's), as
/// `src/app.rs` picks it.
fn sqlh_version() -> &'static str {
    option_env!("SQLH_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The JSON contract version `--version` reports: `sqlh 0.1.0 (json contract 2.0)`.
fn contract_version() -> String {
    let r = sqlh_args(&["--version"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let line = r.stdout.trim_end();
    let contract = line
        .strip_prefix(&format!("sqlh {} (json contract ", sqlh_version()))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected --version output: {line:?}"));
    let digits = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit());
    assert!(
        contract
            .split_once('.')
            .is_some_and(|(major, minor)| digits(major) && digits(minor)),
        "the contract version is major.minor: {contract:?}"
    );
    contract.to_owned()
}

// PLAN.md 2.11: the JSON document starts with its contract version, which `--version`
// reports, and a run that never connected (a failed connect, a guard refusal, an error in the
// input, nothing to run) has no description of a connection and no connect time.
#[test]
fn json_documents_start_with_the_contract_version() {
    let contract = contract_version();
    let head = format!("{{\"contract_version\":\"{contract}\",\"batches\":[]");
    let dead = |sql: &'static str| {
        let mut args = DEAD.to_vec();
        args.extend(["-Q", sql]);
        args
    };
    let cases = [
        ("failed connect", dead("SELECT 1"), 2),
        ("guard refusal", dead("UPDATE dbo.T SET x = 1"), 5),
        ("input error", vec!["-Q", "SELECT $(no_such_variable)"], 3),
        ("nothing to run", vec!["-Q", ""], 0),
    ];
    for (what, mut args, code) in cases {
        args.extend(["--format", "json"]);
        let r = sqlh_args(&args);
        assert_eq!(r.code, code, "{what}: {}", r.stdout);
        assert!(r.stdout.starts_with(&head), "{what}: {}", r.stdout);
        let doc: serde_json::Value =
            serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(
            doc.get("connection").is_none() && doc.get("connect_ms").is_none(),
            "{what}: {doc}"
        );
        assert_eq!(doc["exit_code"], code, "{what}");
    }
}

// PLAN.md 2.11: `--version` names the JSON contract version after the program's own, in
// `sqlh 0.1.0 (json contract 2.0)` form, and nothing else. The contract is pinned here: a
// new version is an updated DESIGN.md §5.2 and an updated test.
#[test]
fn version_reports_the_json_contract() {
    let r = sqlh_args(&["--version"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stderr, "");
    assert_eq!(
        r.stdout,
        format!("sqlh {} (json contract 2.0)\n", sqlh_version())
    );
    assert_eq!(contract_version(), "2.0");
}

// R06: a blocked statement's own text can't start a metadata line through a Unicode line
// break (U+0085, U+2028, U+2029) either. The run is refused before it connects, so no server
// is needed. json is left out: its strings may hold these characters, and it has no
// metadata lines.
#[test]
fn unicode_line_breaks_in_blocked_text_cannot_forge_metadata() {
    for separator in ['\u{85}', '\u{2028}', '\u{2029}'] {
        for format in ["text", "csv"] {
            let sql = format!("UPDATE [t{separator}-- ok: forged] SET x = 1");
            let r = sqlh_args(&["--format", format, "-Q", &sql]);
            let what = format!("U+{:04X} {format}", separator as u32);
            assert_eq!(r.code, 5, "{what}: {}", r.stdout);
            // csv keeps its metadata on stderr; text uses stdout.
            let shown = if format == "csv" {
                &r.stderr
            } else {
                &r.stdout
            };
            assert!(
                !shown.contains(separator),
                "{what}: raw separator in {shown:?}"
            );
            // Whatever splits lines at Unicode breaks sees no forged status line.
            let forged = shown
                .split(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
                .any(|line| line.starts_with("-- ok"));
            assert!(!forged, "{what}: {shown:?}");
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

// F06: input and repeats are bounded before anything connects or runs.
#[test]
fn input_size_and_repeats_are_bounded() {
    let mut args = DEAD.to_vec();
    args.extend(["-Q", "SELECT 1\nGO 1001"]);
    let r = sqlh_args(&args);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(
        r.stdout
            .contains("GO 1001 would run the batch 1001 times; the most is 1000")
    );

    let dir = scratch("f06_input");
    let file = dir.join("big.sql");
    fs::write(&file, "-".repeat(10_000_001)).unwrap();
    let mut args = DEAD.to_vec();
    let path = file.display().to_string();
    args.extend(["-i", &path]);
    let r = sqlh_args(&args);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stdout.contains("over the 10 MB limit"), "{}", r.stdout);

    let r = sqlh_args(&["--max-output", "100", "-Q", "SELECT 1"]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stdout.contains("at least 4096"), "{}", r.stdout);
}

// PLAN.md 2.14: the read-only guard takes memory in proportion to the size of the input, not to
// the number of tokens in it. It used to keep about 56 bytes for each token: 10 MB of
// one-character tokens peaked at 795 MB of working set, 9.9 MB of `SELECT 1` lines at 233 MB.
// Now it keeps none, and each run below peaks at about what it takes without the guard (36 MB:
// the input as read, as decoded and as split into batches), 54 MB for the one whose refusal
// quotes a 10 MB name. The guard runs before anything connects, so no server is needed: the
// first two pass it and end at the dead address, the others are refused.
#[cfg(windows)]
#[test]
fn the_guard_takes_memory_in_proportion_to_the_input() {
    // A small multiple of the 10 MB input limit; the old guard needed 23 to 86 times.
    const LIMIT: usize = 80_000_000;
    let dir = scratch("guard_memory");
    let file = dir.join("input.sql");
    let cases = [
        ("one-character tokens", "(".repeat(10_000_000), 2, "exit 2"),
        (
            "SELECT 1 lines",
            "SELECT 1\n".repeat(1_111_111),
            2,
            "exit 2",
        ),
        (
            "a refusal after 9,999,000 one-character tokens",
            format!("{}\nDELETE FROM dbo.T", "(".repeat(9_999_000)),
            5,
            "-- blocked: line 2: DELETE dbo.T would write; nothing was run.",
        ),
        (
            "a refusal quoting a 10 MB name",
            format!("EXEC {}a", "a.".repeat(4_999_990)),
            5,
            "-- blocked: line 1: EXEC a.a.a.a.",
        ),
        // R18: the clauses the classifier remembers are capped, and a call's arguments are
        // read a bounded number of times.
        (
            "10 MB of TOP clauses that never close",
            "DELETE TOP (".repeat(833_000),
            5,
            "-- blocked: line 1: DELETE ? would write; nothing was run.",
        ),
        (
            "9.5 MB of sp_spaceused calls in parentheses",
            "EXEC sp_spaceused (".repeat(500_000),
            5,
            "-- blocked: line 1: this input is too complex for the read-only check: the \
             arguments of sp_spaceused have more than 8 parentheses; nothing was run.",
        ),
    ];
    for (what, sql, code, expected) in cases {
        assert!(sql.len() <= 10_000_000, "{what}: {} bytes", sql.len());
        fs::write(&file, &sql).unwrap();
        let (r, peak) = run_measured(sqlh().args(DEAD).arg("-i").arg(&file));
        assert_eq!(r.code, code, "{what}: {}", r.stdout);
        assert!(
            r.stdout.contains(expected) || r.stderr.contains(expected),
            "{what}: {} {}",
            r.stdout.chars().take(300).collect::<String>(),
            r.stderr.chars().take(300).collect::<String>()
        );
        assert!(peak < LIMIT, "{what}: peak working set {} MB", peak >> 20);
    }
}

// R18: the guard's work is bounded, not only its memory. `UPDATE TOP (` repeated N times, `@t`,
// then `) @t ` N times, is not SQL a server accepts, but it is a few hundred kilobytes that the
// classifier passes, and its lookahead used to read the rest of the input again from every
// UPDATE: 0.2 s at 2,000 deep, 0.9 s at 4,000, 3.4 s at 8,000 and 15 s at 16,000 (release
// build), all before the run could be cancelled or timed. Now the clauses are read in one pass
// (tens of milliseconds at the deepest), and input nested past what the guard remembers is
// refused before anything connects. A refused `DELETE` is appended to each, so a run that was
// classified to the end shows by the guard's refusal of it (exit 5) and not by a connect.
#[test]
fn nested_input_is_classified_or_refused_in_bounded_time() {
    // The bound is the test's, with headroom for a debug build on a busy machine; the guard's
    // own time is in DESIGN.md §11 and notes/memory.md.
    const BOUND: Duration = Duration::from_secs(5);
    let dir = scratch("guard_nested");
    let file = dir.join("input.sql");
    let nested = |n: usize| format!("{}@t{}", "UPDATE TOP (".repeat(n), ") @t ".repeat(n));
    let timed = |sql: &str, extra: &[&str]| {
        fs::write(&file, sql).unwrap();
        let started = Instant::now();
        let r = run(sqlh().args(DEAD).args(extra).arg("-i").arg(&file));
        (r, started.elapsed())
    };
    for n in [2_000, 4_000, 8_000, 16_000] {
        let (r, took) = timed(&format!("{}\nDELETE FROM dbo.T", nested(n)), &[]);
        assert_eq!(r.code, 5, "{n}: {}", r.stdout);
        assert!(
            r.stdout.contains(
                "-- blocked: line 2: DELETE dbo.T would write; nothing was run. Rerun with \
                 --allow-write if the write is intended"
            ),
            "{n}: {}",
            r.stdout
        );
        assert!(took < BOUND, "{n} deep: {took:?}");
    }
    // Nothing after it: classified to the end and let through to the connect (which fails: exit
    // 2, after the connect timeout, so only the deepest passing case runs).
    let (r, took) = timed(&nested(16_000), &[]);
    assert_reached_connect(&r, "16,000 deep");
    assert!(took < BOUND + Duration::from_secs(5), "{took:?}");

    // Past what the guard remembers: refused, with a message that says what to do.
    let (r, took) = timed(&nested(20_000), &[]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "-- blocked: line 1: this input is too complex for the read-only check: TOP ( \
             clauses are nested more than 16384 deep; nothing was run. Simplify it or split it \
             into smaller batches with GO, or rerun with --allow-write if the write is intended"
        ),
        "{}",
        r.stdout
    );
    assert!(took < BOUND, "{took:?}");
    // --allow-write skips the classifier, so it lifts this refusal as it does the others.
    let (r, _) = timed(&nested(20_000), &["--allow-write"]);
    assert_reached_connect(&r, "20,000 deep with --allow-write");

    // A call's arguments in parentheses, one inside the next: refused at the first call.
    let (r, took) = timed(&"EXEC sp_spaceused (".repeat(20_000), &[]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout
            .contains("this input is too complex for the read-only check"),
        "{}",
        r.stdout
    );
    assert!(took < BOUND, "{took:?}");

    // After a GO the refusal's line is the line in the file, not in the batch.
    let (r, _) = timed(&format!("SELECT 1\nGO\nSELECT 2\n{}", nested(20_000)), &[]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout
            .contains("-- blocked: line 4: this input is too complex"),
        "{}",
        r.stdout
    );
}

/// Runs `command` to its end and also returns the child's peak working set in bytes. Windows
/// keeps that with the process handle, which `Child` holds until it is dropped, so it can be
/// read after the child has exited. The pipes are drained in the order a run writes them: a
/// run of sqlh writes little to stderr.
#[cfg(windows)]
fn run_measured(command: &mut Command) -> (Run, usize) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    /// PROCESS_MEMORY_COUNTERS.
    #[repr(C)]
    #[derive(Default)]
    struct MemoryCounters {
        size: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        peak_paged_pool: usize,
        paged_pool: usize,
        peak_non_paged_pool: usize,
        non_paged_pool: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }

    // kernel32 exports GetProcessMemoryInfo under this name since Windows 7.
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut MemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    let (mut stdout, mut stderr) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().expect("sqlh ends");
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let mut counters = MemoryCounters {
        size: std::mem::size_of::<MemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle is the child's own process handle, which `child` keeps open; the
    // counters are a correctly sized, writable struct, and `size` says how big it is.
    let read =
        unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, counters.size) };
    assert_ne!(read, 0, "peak memory: {}", std::io::Error::last_os_error());
    let run = Run {
        code: status.code().expect("an exit code, not a signal"),
        stdout,
        stderr,
    };
    (run, counters.peak_working_set)
}

// PLAN.md 2.7: $(var) substitution is checked against the 10 MB limit while the text is built,
// so a reference repeated 200,000 times to a 3,000-byte value is refused when it reaches the
// limit, not after it has been expanded: that would be 600 MB, and the whole run now takes a
// few megabytes more than the file it reads.
#[cfg(windows)]
#[test]
fn a_runaway_substitution_is_refused_without_being_built() {
    let dir = scratch("f06_expansion");
    let file = dir.join("expand.sql");
    fs::write(&file, "$(big)".repeat(200_000)).unwrap();
    let value = format!("big={}", "x".repeat(3_000));
    let (r, peak) = run_measured(sqlh().args(DEAD).args(["-v", &value, "-i"]).arg(&file));
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(
        r.stdout
            .contains("after $(var) substitution the SQL is over the 10 MB limit; run it in parts"),
        "{}",
        r.stdout
    );
    assert!(peak < 100 << 20, "peak working set {} MB", peak >> 20);
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

// PLAN.md 2.13, Windows shells and paths. The measurements are in docs/notes/windows-shells.md.

/// The text format writes a backslash as two (DESIGN.md §5.1).
fn text_escaped(text: &str) -> String {
    text.replace('\\', r"\\")
}

/// The sentence the Git Bash hint ends with, for the Windows spelling `windows`.
fn git_bash_hint(windows: &str) -> String {
    format!(
        "that is a Git Bash path, which only Git Bash converts; in cmd.exe and PowerShell write \
         {windows} or {}",
        windows.replace('\\', "/")
    )
}

/// The first line of the output of a run that ended in a usage error.
fn error_line(r: &Run) -> &str {
    assert_eq!(r.code, 3, "{}", r.stdout);
    r.stdout.lines().next().unwrap_or("")
}

// cmd.exe and PowerShell don't convert Git Bash's /c/dir paths, and Windows reads them as \c\dir
// on the current drive. A path in that form that isn't found says so and names the spellings
// that work, for -i, -o and SQLH_CONFIG. (Git Bash converts such arguments before sqlh sees
// them, so this is what cmd.exe and PowerShell users get.) The folder named below must not
// exist on the current drive, which the first assertion checks.
#[test]
fn a_git_bash_path_that_is_not_found_names_the_windows_spelling() {
    let gone = Path::new("/c/sqlh-no-such-dir");
    assert!(
        !gone.exists(),
        "this test needs a folder named \\c\\sqlh-no-such-dir to be missing on the current drive"
    );

    let r = sqlh_args(&["-i", "/c/sqlh-no-such-dir/in.sql"]);
    let line = error_line(&r);
    assert!(
        line.starts_with("-- error: -i /c/sqlh-no-such-dir/in.sql: "),
        "{line}"
    );
    let hint = text_escaped(&git_bash_hint(r"C:\sqlh-no-such-dir\in.sql"));
    assert!(line.ends_with(&format!("; {hint}")), "{line}");

    // In json the same text is a string, so a backslash is the JSON escape of one.
    let r = sqlh_args(&["--format", "json", "-i", "/c/sqlh-no-such-dir/in.sql"]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("a json document");
    let error = doc["error"].as_str().expect("an error string");
    assert!(
        error.ends_with(&format!(
            "; {}",
            git_bash_hint(r"C:\sqlh-no-such-dir\in.sql")
        )),
        "{error}"
    );

    // -o: the folder isn't found for the output file either, and isn't created.
    let r = sqlh_args(&["-Q", "SELECT 1", "-o", "/c/sqlh-no-such-dir/out.txt"]);
    let line = error_line(&r);
    assert!(
        line.starts_with("-- error: -o /c/sqlh-no-such-dir/out.txt: "),
        "{line}"
    );
    let hint = text_escaped(&git_bash_hint(r"C:\sqlh-no-such-dir\out.txt"));
    assert!(line.ends_with(&format!("; {hint}")), "{line}");

    // SQLH_CONFIG: a missing profiles file is normally "no profiles yet", and saving one
    // creates its folder, which here would be \c\sqlh-no-such-dir on the current drive. Every
    // command that reads or writes the file refuses it first.
    let config = "/c/sqlh-no-such-dir/profiles.toml";
    let hint = text_escaped(&git_bash_hint(r"C:\sqlh-no-such-dir\profiles.toml"));
    for args in [
        vec!["profile", "list"],
        vec!["profile", "rm", "nothing"],
        vec!["--profile", "dev", "-Q", "SELECT 1"],
    ] {
        let r = run(sqlh().env("SQLH_CONFIG", config).args(&args));
        assert_eq!(
            error_line(&r),
            format!(
                "-- error: SQLH_CONFIG /c/sqlh-no-such-dir/profiles.toml: the folder wasn't \
                 found; {hint}"
            ),
            "{args:?}"
        );
    }
    assert!(!gone.exists(), "a command created the folder");

    // The hint is only for a Git Bash path. Other paths that aren't found get the plain error.
    for path in [
        r"C:\sqlh-no-such-dir\in.sql",
        "C:/sqlh-no-such-dir/in.sql",
        "/tmp/sqlh-no-such-dir/in.sql",
        "//localhost/sqlh-no-such-share/in.sql",
        "sqlh-no-such-dir/in.sql",
    ] {
        let r = sqlh_args(&["-i", path]);
        let line = error_line(&r);
        assert!(
            line.starts_with("-- error: -i ") && !line.contains("Git Bash"),
            "{path}: {line}"
        );
    }
}

/// `dir\rel` as a UNC path to this computer's administrative share (`\\localhost\C$\...`), if it
/// can be reached. It can't on a machine without the share or without the right to use it.
#[cfg(windows)]
fn unc_to_this_machine(dir: &Path, rel: &str) -> Option<String> {
    let native = dir.display().to_string();
    let (drive, rest) = native.split_once(":\\")?;
    let unc = format!(r"\\localhost\{drive}$\{rest}\{rel}");
    // The directory is what has to be reachable; the file may not exist yet.
    let reachable = fs::metadata(Path::new(&unc).parent()?).is_ok();
    reachable.then_some(unc)
}

/// The ways to write the path of `rel` (relative to `dir`, written with backslashes) when the
/// working directory is `dir`. A UNC form comes last when this machine's own share can be
/// reached.
fn spellings(dir: &Path, rel: &str) -> Vec<(&'static str, String)> {
    let native = dir.join(rel).display().to_string();
    let parent = dir.file_name().unwrap().to_string_lossy().into_owned();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut forms = vec![
        ("backslashes", native.clone()),
        ("forward slashes", native.replace('\\', "/")),
        (
            "mixed slashes",
            format!("{}/{}", dir.display(), rel.replace('\\', "/")),
        ),
        ("relative", rel.to_owned()),
        ("relative with .", format!(r".\{rel}")),
        ("relative with ..", format!(r"..\{parent}\{rel}")),
        ("relative, forward slashes", rel.replace('\\', "/")),
    ];
    #[cfg(windows)]
    if let Some(unc) = unc_to_this_machine(dir, rel) {
        forms.push(("UNC", unc));
    }
    forms
}

// PLAN.md 2.13: -i, -o and SQLH_CONFIG take C:\dir, C:/dir, mixed slashes, relative paths
// (with . and ..), UNC paths and paths with spaces. A blocked statement ends the run at the
// guard (exit 5) without connecting, so exit 5 means the input was read, the output file got
// the refusal, and (for SQLH_CONFIG) the profile was found: without it the exit would be 3.
// The folder and the files have a space in their names, so every form is one with spaces too.
// The forms run side by side, each with its own output file: the UNC ones are slow.
#[test]
fn path_forms_work_for_input_output_and_config() {
    let blocked = "UPDATE dbo.T SET x = 1";
    let profiles =
        "[profiles.win]\nserver = \"127.0.0.1,1\"\nauth = \"windows\"\nlogin_timeout = 1\n";
    let dir = scratch("path forms");
    let dir = dir.as_path();
    fs::write(dir.join("in file.sql"), blocked).unwrap();
    fs::write(dir.join("profiles file.toml"), profiles).unwrap();
    let inputs = spellings(dir, "in file.sql");
    let configs = spellings(dir, "profiles file.toml");
    if !inputs.iter().any(|(name, _)| *name == "UNC") {
        eprintln!("skipped: no UNC path to this machine (\\\\localhost\\C$) is reachable");
    }
    std::thread::scope(|scope| {
        for (n, ((name, input_form), (_, config_form))) in inputs.iter().zip(&configs).enumerate() {
            scope.spawn(move || {
                // -i
                let r = run(sqlh().current_dir(dir).args(["-i", input_form]));
                assert_eq!(r.code, 5, "-i {name}: {input_form}: {}", r.stdout);
                assert!(
                    r.stdout.contains("-- blocked: line 1: UPDATE dbo.T"),
                    "-i {name}: {input_form}"
                );

                // -o: the refusal lands in the file, and nothing else is left in the folder.
                let output = format!("out {n}.txt");
                let output_form = &spellings(dir, &output)[n].1;
                let r = run(sqlh()
                    .current_dir(dir)
                    .args(["-Q", blocked, "-o", output_form]));
                assert_eq!(
                    r.code, 5,
                    "-o {name}: {output_form}: {}{}",
                    r.stdout, r.stderr
                );
                let written = fs::read_to_string(dir.join(&output))
                    .unwrap_or_else(|e| panic!("-o {name}: {output_form}: {e}"));
                assert!(
                    written.starts_with("-- blocked: line 1: UPDATE dbo.T"),
                    "-o {name}: {written}"
                );

                // SQLH_CONFIG
                let r = run(sqlh()
                    .current_dir(dir)
                    .env("SQLH_CONFIG", config_form)
                    .args(["--profile", "win", "-Q", blocked]));
                assert_eq!(r.code, 5, "SQLH_CONFIG {name}: {config_form}: {}", r.stdout);

                // The same file spelled two ways is still the same file: -o can't overwrite -i.
                let native = dir.join("in file.sql").display().to_string();
                let r = run(sqlh()
                    .current_dir(dir)
                    .args(["-i", input_form, "-o", &native]));
                assert_eq!(r.code, 3, "-i {name} -o {native}: {}", r.stdout);
                assert!(
                    r.stdout.contains("would overwrite the input file"),
                    "-i {name} -o {native}: {}",
                    r.stdout
                );
            });
        }
    });
    assert_eq!(
        fs::read_to_string(dir.join("in file.sql")).unwrap(),
        blocked
    );
    let left = names(dir);
    assert!(
        !left.iter().any(|n| n.ends_with(".partial")),
        "a .partial file was left: {left:?}"
    );
}

/// Runs `command` with `input` on stdin.
fn run_with_stdin(command: &mut Command, input: &[u8]) -> Run {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlh starts");
    let mut stdin = child.stdin.take().expect("a piped stdin");
    std::io::Write::write_all(&mut stdin, input).expect("input is accepted");
    drop(stdin);
    let output = child.wait_with_output().expect("sqlh ends");
    Run {
        code: output.status.code().expect("an exit code"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

// PLAN.md 2.13: what a shell pipes to sqlh is whatever its encoding makes of the text. UTF-8
// with or without a BOM, and UTF-16 with one (what `>` in Windows PowerShell 5.1 writes), are
// read as they are. Anything else (cmd.exe at an OEM code page, PowerShell with $OutputEncoding
// set to the console's) isn't valid UTF-8 and is refused with what to change. Text already
// turned into "?" can't be told from a question mark; DESIGN.md §4.1 and --help warn about it.
#[test]
fn piped_text_in_another_encoding_is_refused_and_utf8_is_read_as_it_is() {
    // OEM code page 437: e acute is the single byte 0x82, at offset 11.
    let oem = b"UPDATE [Caf\x82] SET x = 1";
    for args in [vec![], vec!["-i", "-"]] {
        let r = run_with_stdin(sqlh().args(&args), oem);
        assert_eq!(
            error_line(&r),
            "-- error: stdin: not valid UTF-8 (byte 11); save it as UTF-8, or UTF-16 with a BOM. \
             Piped text is encoded by the shell: in cmd.exe run chcp 65001 first, in Windows \
             PowerShell run $OutputEncoding = [Text.Encoding]::UTF8 first, or pass the SQL with \
             -Q or -i",
            "{args:?}"
        );
    }
    // The same bytes in a file were saved that way, so the advice is about saving it.
    let dir = scratch("piped_encodings");
    fs::write(dir.join("oem.sql"), oem).unwrap();
    let r = sqlh_args(&["-i", &dir.join("oem.sql").display().to_string()]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stdout.contains("not valid UTF-8 (byte 11);") && !r.stdout.contains("chcp"));

    // UTF-8 (what PowerShell 7 and Git Bash send), with a BOM (what Windows PowerShell 5.1
    // sends after $OutputEncoding = [Text.Encoding]::UTF8), and UTF-16 with a BOM: the
    // identifier arrives intact, which the refusal shows.
    let sql = "UPDATE [Caf\u{e9} \u{2026}] SET x = 1";
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(sql.as_bytes());
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(sql.encode_utf16().flat_map(u16::to_le_bytes));
    for (what, bytes) in [
        ("utf-8", sql.as_bytes().to_vec()),
        ("utf-8 with a BOM", with_bom),
        ("utf-16 with a BOM", utf16),
    ] {
        let r = run_with_stdin(&mut sqlh(), &bytes);
        assert_eq!(r.code, 5, "{what}: {}", r.stdout);
        assert!(
            r.stdout
                .contains("-- blocked: line 1: UPDATE [Caf\u{e9} \u{2026}] would write"),
            "{what}: {}",
            r.stdout
        );
    }
}

// AGENTS.md: models read --help, so it stays short. PLAN.md 2.13 holds it at 62 lines.
#[test]
fn help_stays_within_its_line_budget() {
    let r = sqlh_args(&["--help"]);
    assert_eq!(r.code, 0);
    let lines = r.stdout.lines().count();
    assert!(lines <= 62, "--help is {lines} lines");
}

/// `text` as UTF-16LE in base64, the form PowerShell's `-EncodedCommand` takes.
#[cfg(windows)]
fn base64_of_utf16(text: &str) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let digit = |shift: u32| DIGITS[(group >> shift) as usize & 63] as char;
        encoded.push(digit(18));
        encoded.push(digit(12));
        encoded.push(if chunk.len() > 1 { digit(6) } else { '=' });
        encoded.push(if chunk.len() > 2 { digit(0) } else { '=' });
    }
    encoded
}

/// A shell a person types commands in.
#[cfg(windows)]
#[derive(Clone, Copy)]
enum Shell {
    Cmd,
    PowerShell51,
    PowerShell7,
}

#[cfg(windows)]
impl Shell {
    fn name(self) -> &'static str {
        match self {
            Shell::Cmd => "cmd.exe",
            Shell::PowerShell51 => "Windows PowerShell 5.1",
            Shell::PowerShell7 => "PowerShell 7",
        }
    }

    /// Whether the shell is installed. cmd.exe and Windows PowerShell 5.1 come with Windows.
    fn installed(self) -> bool {
        match self {
            Shell::Cmd | Shell::PowerShell51 => true,
            Shell::PowerShell7 => Command::new("pwsh.exe")
                .args(["-NoProfile", "-Command", "exit 0"])
                .output()
                .is_ok(),
        }
    }

    /// Runs `line` the way it is typed at the prompt, in `dir`, with `sqlh` resolving to the
    /// binary under test, in a console of its own so no window opens and no earlier
    /// code page applies.
    fn run(self, line: &str, dir: &Path, env: &[(&str, String)]) -> Run {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = match self {
            Shell::Cmd => {
                // /s takes off exactly the outer quotes, as typing at a prompt would, and /d
                // skips the AutoRun commands some machines have in the registry.
                let mut command = Command::new("cmd.exe");
                command.raw_arg(format!("/d /s /c \"{line}\""));
                command
            }
            Shell::PowerShell51 | Shell::PowerShell7 => {
                // -EncodedCommand (the script as base64 of UTF-16) puts the line in front of
                // the parser unchanged, and unlike a script file no execution policy can
                // block it.
                let script = format!("{line}\r\nexit $LASTEXITCODE\r\n");
                let exe = if matches!(self, Shell::PowerShell7) {
                    "pwsh.exe"
                } else {
                    "powershell.exe"
                };
                let mut command = Command::new(exe);
                command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand"]);
                command.arg(base64_of_utf16(&script));
                command
            }
        };
        let sqlh_dir = Path::new(env!("CARGO_BIN_EXE_sqlh")).parent().unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        // Isolated from the machine's profiles and SQLCMD* variables, as `sqlh()` is.
        for var in [
            "SQLH_PROFILE",
            "SQLCMDSERVER",
            "SQLCMDDBNAME",
            "SQLCMDUSER",
            "SQLCMDPASSWORD",
        ] {
            command.env_remove(var);
        }
        command
            .current_dir(dir)
            .env("PATH", format!("{};{path}", sqlh_dir.display()))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        run(&mut command)
    }
}

// PLAN.md 2.13: the examples in --help run as written in cmd.exe, Windows PowerShell 5.1 and
// PowerShell 7, the shells a person (or an agent's harness) is likely to be in. (Git Bash is
// checked by hand: docs/notes/windows-shells.md.) A listener that closes every connection
// stands in for the server, and `myserver` is swapped for it, so the run gets as far as the
// connect (exit 2) only if the shell handed sqlh the arguments the help meant: the input file
// was found and read, the options parsed, the profile found, and the server named is the one
// in the line. The examples use no other name for a server, so nothing else is rewritten.
#[cfg(windows)]
#[test]
fn the_help_examples_run_as_written_in_every_shell() {
    let help = sqlh_args(&["--help"]);
    assert_eq!(help.code, 0);
    let examples: Vec<String> = help
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("EXAMPLES"))
        .skip(1)
        .take_while(|l| l.starts_with("  sqlh "))
        .map(|l| {
            let command = l.trim().split("   (").next().unwrap_or("");
            command.to_owned()
        })
        .collect();
    assert!(
        examples.len() >= 4,
        "--help has {} examples",
        examples.len()
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("127.0.0.1,{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            drop(connection);
        }
    });

    let base = scratch("help_examples");
    let config = base.join("profiles.toml");
    fs::write(
        &config,
        format!("[profiles.dev]\nserver = \"{server}\"\nauth = \"windows\"\nlogin_timeout = 10\n"),
    )
    .unwrap();
    let env = [
        ("SQLH_CONFIG", config.display().to_string()),
        // Only the example that names a login needs a password; this one is never sent.
        ("SQLCMDPASSWORD", "not-a-real-password".to_owned()),
    ];

    let shells: Vec<Shell> = [Shell::Cmd, Shell::PowerShell51, Shell::PowerShell7]
        .into_iter()
        .filter(|shell| {
            let installed = shell.installed();
            if !installed {
                eprintln!("skipped: {} isn't installed here", shell.name());
            }
            installed
        })
        .collect();
    // Each shell in its own thread, as starting Windows PowerShell takes a moment, and in a
    // working directory of its own, since one example writes report.txt.
    let results: Vec<_> = shells
        .iter()
        .map(|&shell| {
            let dir = base.join(format!("shell-{}", shell as u8));
            fs::create_dir_all(dir.join("my queries")).unwrap();
            fs::write(dir.join("report.sql"), "SELECT 1").unwrap();
            fs::write(dir.join("my queries").join("report.sql"), "SELECT 1").unwrap();
            let (env, server, examples) = (env.clone(), server.clone(), examples.clone());
            std::thread::spawn(move || {
                examples
                    .iter()
                    .enumerate()
                    .map(|(n, example)| {
                        let line = example
                            .replace("myserver,1433", &server)
                            .replace("myserver", &server);
                        // Each example writes its own report.txt, so clear the last one's.
                        let _ = fs::remove_file(dir.join("report.txt"));
                        let r = shell.run(&line, &dir, &env);
                        let output = fs::read_to_string(dir.join("report.txt")).unwrap_or_default();
                        (n, line, r, output)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for (shell, result) in shells.iter().zip(results) {
        for (n, line, r, report) in result.join().unwrap() {
            let what = format!("{}, example {}: {line}", shell.name(), n + 1);
            // -o sends the output to report.txt; the others print it.
            let text = format!("{}{report}", r.stdout);
            assert_eq!(r.code, 2, "{what}\n{text}\n{}", r.stderr);
            assert!(
                text.contains(&format!("-- error: cannot connect to {server}")),
                "{what}\n{text}\n{}",
                r.stderr
            );
        }
    }
}

// PLAN.md 2.13, DESIGN.md §4.1: `-Q "..."` with single quotes inside reaches sqlh as typed in
// every shell, a space, brackets and a non-ASCII letter included. A blocked statement shows the
// name it writes to, so the refusal proves what arrived. The same line with a character that
// belongs to a shell would not: DESIGN.md lists them, and docs/notes/windows-shells.md has the
// measurements, so they aren't re-run here.
#[cfg(windows)]
#[test]
fn the_documented_q_form_reaches_sqlh_intact_in_every_shell() {
    let line = "sqlh -Q \"UPDATE [it's a caf\u{e9}] SET x = 'it''s'\"";
    let base = scratch("q_form");
    let results: Vec<_> = [Shell::Cmd, Shell::PowerShell51, Shell::PowerShell7]
        .into_iter()
        .filter(|shell| shell.installed())
        .map(|shell| {
            let dir = base.join(format!("shell-{}", shell as u8));
            fs::create_dir_all(&dir).unwrap();
            std::thread::spawn(move || (shell, shell.run(line, &dir, &[])))
        })
        .collect();
    assert!(!results.is_empty());
    for result in results {
        let (shell, r) = result.join().unwrap();
        assert_eq!(r.code, 5, "{}: {}{}", shell.name(), r.stdout, r.stderr);
        assert!(
            r.stdout
                .contains("-- blocked: line 1: UPDATE [it's a caf\u{e9}] would write"),
            "{}: {}",
            shell.name(),
            r.stdout
        );
    }
}

// PLAN.md 2.13: sqlh leaves the console's code page as it found it. It used to switch the
// console to UTF-8 whenever stdout was a console and never switch back, so every program run
// afterwards in that console (and every later PowerShell 5.1 session) inherited UTF-8. The
// switch was never needed: Rust writes a console as UTF-16. The check needs a console that
// sqlh's stdout is, so a hidden one is made, started at code page 437 whatever this machine's
// default is, and the code page is read back after sqlh has run in it.
#[cfg(windows)]
#[test]
fn sqlh_leaves_the_console_code_page_alone() {
    let exe = env!("CARGO_BIN_EXE_sqlh");
    let dir = scratch("console_code_page");
    let result = dir.join("code-page.txt");
    // `chcp` sets both code pages; reading the output one needs the API, here through
    // PowerShell, because `chcp` and `mode con` report the input page.
    let inner = format!(
        "chcp 437>nul & \"{exe}\" --help & \"{exe}\" -Q \"UPDATE dbo.T SET x = 1\" \
         & powershell.exe -NoProfile -Command [Console]::OutputEncoding.CodePage > \"{}\"",
        result.display()
    );
    // Started without redirected handles, the child's stdout is its own hidden console, and
    // so is sqlh's inside it. Windows PowerShell does that through .NET.
    let launcher = "$i = New-Object System.Diagnostics.ProcessStartInfo; \
        $i.FileName = 'cmd.exe'; $i.Arguments = '/d /s /c \"' + $env:SQLH_TEST_LINE + '\"'; \
        $i.UseShellExecute = $false; $i.CreateNoWindow = $true; \
        $p = [System.Diagnostics.Process]::Start($i); $p.WaitForExit()";
    let r = run(Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", launcher])
        .env("SQLH_TEST_LINE", inner)
        .stdin(Stdio::null()));
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    let page = fs::read_to_string(&result).expect("the code page was written");
    assert_eq!(
        page.trim(),
        "437",
        "the console's output code page after sqlh ran (437 is what it was set to)"
    );
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

// PLAN.md 2.8: --lock-timeout is whole seconds, 0 means no limit, and the most that
// SET LOCK_TIMEOUT can hold is 2,147,483 s. Anything else is a usage error found before
// anything connects (exit 3, where the dead address would give 2).
#[test]
fn a_bad_lock_timeout_is_a_usage_error() {
    for (value, reason) in [
        ("abc", "expected a non-negative whole number"),
        ("-1", "expected a non-negative whole number"),
        ("2.5", "expected a non-negative whole number"),
        ("2147484", "at most 2147483 seconds; use 0 for no limit"),
    ] {
        let mut args = DEAD.to_vec();
        args.extend(["--lock-timeout", value, "-Q", "SELECT 1"]);
        let r = sqlh_args(&args);
        assert_eq!(
            error_line(&r),
            format!("-- error: --lock-timeout {value}: {reason}")
        );
        assert!(
            last_line(&r.stdout).starts_with("-- failed (exit 3): usage error"),
            "{}",
            r.stdout
        );
    }
    let r = sqlh_args(&["--lock-timeout"]);
    assert!(error_line(&r).contains("--lock-timeout"), "{}", r.stdout);
    // The largest value, and 0, are accepted: the run goes on to the connect.
    for value in ["2147483", "0", "1"] {
        let mut args = DEAD.to_vec();
        args.extend(["--lock-timeout", value, "-Q", "SELECT 1"]);
        assert_reached_connect(&sqlh_args(&args), value);
    }
}

// PLAN.md 2.8, owner decision 7: READ UNCOMMITTED is for read-only runs. Asked for on the
// command line in a run that can write, it is a usage error that says why; saved in a
// profile, it is ignored in such a run. Switched off explicitly, it is no conflict.
#[test]
fn read_uncommitted_with_writes_is_a_usage_error() {
    let mut with_writes = DEAD.to_vec();
    with_writes.extend(["--read-uncommitted", "--allow-write", "-Q", "SELECT 1"]);
    let r = sqlh_args(&with_writes);
    let expected = "-- error: --read-uncommitted can't be used with --allow-write: a write \
                    could be based on rows that were never committed. Drop one of the two";
    assert_eq!(error_line(&r), expected);
    assert!(
        last_line(&r.stdout).starts_with("-- failed (exit 3): usage error"),
        "{}",
        r.stdout
    );
    // The order of the flags doesn't matter, and every format says it.
    let r = sqlh_args(&[
        "--allow-write",
        "--format",
        "json",
        "--read-uncommitted=true",
        "-S",
        "127.0.0.1,1",
        "-Q",
        "SELECT 1",
    ]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("a json document");
    assert_eq!(doc["error"], expected.trim_start_matches("-- error: "));
    assert_eq!(doc["exit_code"], 3);

    // Off, or not given, is no conflict: these go on to connect.
    for flags in [
        vec!["--read-uncommitted=false", "--allow-write"],
        vec!["--read-uncommitted"],
        vec!["--allow-write"],
    ] {
        let mut args = DEAD.to_vec();
        args.extend(flags.iter().copied());
        args.extend(["-Q", "SELECT 1"]);
        assert_reached_connect(&sqlh_args(&args), &format!("{flags:?}"));
    }
    let r = sqlh_args(&["--read-uncommitted=sometimes", "-Q", "SELECT 1"]);
    assert_eq!(
        error_line(&r),
        "-- error: --read-uncommitted=sometimes: expected true or false"
    );

    // Profiles: the saved setting never conflicts, and a profile that lets every run write
    // refuses the flag.
    let dir = scratch("read_uncommitted_profiles");
    let config = dir.join("profiles.toml");
    fs::write(
        &config,
        "[profiles.dirty]\nserver = \"127.0.0.1,1\"\nauth = \"windows\"\nlogin_timeout = 1\n\
         read_uncommitted = true\n\
         [profiles.writer]\nserver = \"127.0.0.1,1\"\nauth = \"windows\"\nlogin_timeout = 1\n\
         write_policy = \"allow\"\nread_uncommitted = true\n",
    )
    .unwrap();
    let profiled = |args: &[&str]| {
        run(sqlh()
            .args(args)
            .args(["-Q", "SELECT 1"])
            .env("SQLH_CONFIG", &config))
    };
    assert_reached_connect(&profiled(&["--profile", "dirty"]), "saved setting");
    assert_reached_connect(
        &profiled(&["--profile", "dirty", "--allow-write"]),
        "saved setting, writes allowed",
    );
    assert_reached_connect(
        &profiled(&["--profile", "writer"]),
        "saved setting, a profile that lets every run write",
    );
    let r = profiled(&["--profile", "writer", "--read-uncommitted"]);
    assert_eq!(
        error_line(&r),
        "-- error: --read-uncommitted applies to read-only runs, and profile writer has \
         write_policy = \"allow\", so this run can write. Use a profile whose write_policy \
         is \"flag\" or \"deny\""
    );
    assert_reached_connect(
        &profiled(&["--profile", "writer", "--read-uncommitted=false"]),
        "switched off",
    );
}

// PLAN.md 2.8: the profile keys are checked like the others, with the line in the message.
#[test]
fn a_profile_with_a_bad_lock_setting_is_a_usage_error() {
    let dir = scratch("lock_setting_profiles");
    let config = dir.join("profiles.toml");
    let shown = text_escaped(&config.display().to_string());
    for (line, reason) in [
        (
            "lock_timeout = 99999999999",
            "lock_timeout 99999999999 is out of range: at most 2147483 seconds; 0 means no limit",
        ),
        (
            "lock_timeout = 2147484",
            "lock_timeout 2147484 is out of range: at most 2147483 seconds; 0 means no limit",
        ),
        (
            "lock_timeout = \"10\"",
            "lock_timeout must be a whole number",
        ),
        (
            "lock_timeout = -1",
            "unsupported value -1; use \"text\", a whole number, true or false",
        ),
        (
            "read_uncommitted = \"yes\"",
            "read_uncommitted must be true or false",
        ),
        (
            "read_uncommitted = 1",
            "read_uncommitted must be true or false",
        ),
        (
            "lock_timeout = 5\nlock_timeout = 6",
            "lock_timeout is set twice",
        ),
    ] {
        fs::write(
            &config,
            format!("[profiles.p]\nserver = \"127.0.0.1,1\"\n{line}\n"),
        )
        .unwrap();
        let r = run(sqlh()
            .args(["--profile", "p", "-Q", "SELECT 1"])
            .env("SQLH_CONFIG", &config));
        let line_no = if line.contains('\n') { 4 } else { 3 };
        assert_eq!(
            error_line(&r),
            format!("-- error: {shown}:{line_no}: {reason}"),
            "{line}"
        );
    }
}

// PLAN.md 2.8: `profile add` saves both settings, a later `add` without them keeps them,
// `--read-uncommitted=false` and `--lock-timeout 0` are values that replace them, and
// `profile list` shows what a run with each profile gets (10 and 0 when nothing is saved).
// The profile points at a dead address, so the connection test at the end of `add` fails
// (exit 2) after the profile is saved. Windows auth: no password is asked for or stored.
#[test]
fn profile_add_saves_the_lock_settings_and_list_shows_them() {
    let dir = scratch("profile_lock_settings");
    let config = dir.join("profiles.toml");
    let profiles = |args: &[&str]| run(sqlh().args(args).env("SQLH_CONFIG", &config));
    let saved = || fs::read_to_string(&config).expect("the profiles file");
    let listed = || {
        let r = profiles(&["profile", "list"]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        let mut lines = r.stdout.lines().filter(|l| !l.starts_with("-- "));
        let header = lines.next().expect("a header").to_owned();
        assert!(
            header.ends_with(
                "\twrite_policy:text\tlock_timeout:int\tread_uncommitted:bit\texact_count:bit"
            ),
            "{header}"
        );
        lines.map(str::to_owned).collect::<Vec<_>>()
    };

    let r = profiles(&[
        "profile",
        "add",
        "sqlh-ut-locks",
        "-S",
        "127.0.0.1,1",
        "-l",
        "1",
        "--lock-timeout",
        "5",
        "--read-uncommitted",
        "--exact-count",
    ]);
    assert_eq!(
        r.code, 2,
        "saved, then the connection test failed: {}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("-- note: saved profile sqlh-ut-locks in "),
        "{}",
        r.stdout
    );
    let file = saved();
    assert!(
        file.contains("\nlock_timeout = 5\n")
            && file.contains("\nread_uncommitted = true\n")
            && file.contains("\nexact_count = true\n"),
        "{file}"
    );
    let rows = listed();
    assert_eq!(rows.len(), 1, "{rows:?}");
    // A new profile refuses writes unless told otherwise (owner decision 12).
    assert!(rows[0].ends_with("\tdeny\t5\t1\t1"), "{}", rows[0]);

    // Left out, they stay.
    let r = profiles(&["profile", "add", "sqlh-ut-locks", "-d", "AppDb"]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    let file = saved();
    assert!(
        file.contains("\nlock_timeout = 5\n") && file.contains("\nread_uncommitted = true\n"),
        "{file}"
    );

    // Given, they replace: 0 (no limit) and false are values, not "unset".
    let r = profiles(&[
        "profile",
        "add",
        "sqlh-ut-locks",
        "--lock-timeout",
        "0",
        "--read-uncommitted=false",
        "--exact-count=false",
    ]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    let file = saved();
    assert!(
        file.contains("\nlock_timeout = 0\n")
            && file.contains("\nread_uncommitted = false\n")
            && file.contains("\nexact_count = false\n"),
        "{file}"
    );
    let rows = listed();
    assert!(rows[0].ends_with("\tdeny\t0\t0\t0"), "{}", rows[0]);

    // A profile that never mentioned them shows what its runs get.
    let r = profiles(&[
        "profile",
        "add",
        "sqlh-ut-plain",
        "-S",
        "127.0.0.1,1",
        "-l",
        "1",
    ]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    let file = saved();
    let plain = file
        .split("[profiles.sqlh-ut-plain]")
        .nth(1)
        .expect("the second table");
    assert!(
        !plain.contains("lock_timeout")
            && !plain.contains("read_uncommitted")
            && !plain.contains("exact_count"),
        "{file}"
    );
    let rows = listed();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[1].starts_with("sqlh-ut-plain\t"), "{}", rows[1]);
    assert!(rows[1].ends_with("\tdeny\t10\t0\t0"), "{}", rows[1]);

    // The out-of-range value is refused before anything is saved.
    let before = saved();
    let r = profiles(&[
        "profile",
        "add",
        "sqlh-ut-locks",
        "--lock-timeout",
        "2147484",
    ]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert_eq!(saved(), before);
}

// Owner decision 12 and PLAN.md 2.16 (agent trial 2): a new profile refuses writes unless
// `--write-policy` says otherwise, and says so when saved; a refusal under a deny profile never
// offers --allow-write, which that profile refuses. An existing profile keeps its policy.
#[test]
fn a_new_profile_refuses_writes_and_its_refusals_say_so() {
    let dir = scratch("profile_deny_default");
    let config = dir.join("profiles.toml");
    let with = |args: &[&str]| run(sqlh().args(args).env("SQLH_CONFIG", &config));
    let section = |name: &str| {
        let file = fs::read_to_string(&config).expect("the profiles file");
        let start = file
            .find(&format!("[profiles.{name}]"))
            .unwrap_or_else(|| panic!("{name} in {file}"));
        let rest = &file[start + 1..];
        rest[..rest.find("\n[").unwrap_or(rest.len())].to_owned()
    };

    // New, with nothing said: deny, and the note says how to change it.
    let r = with(&[
        "profile",
        "add",
        "sqlh-ut-deny",
        "-S",
        "127.0.0.1,1",
        "-l",
        "1",
    ]);
    assert_eq!(
        r.code, 2,
        "saved, then the connection test failed: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "-- note: sqlh-ut-deny refuses writes (write_policy deny, the default for a new \
             profile); sqlh profile add sqlh-ut-deny --write-policy flag lets --allow-write \
             through\n"
        ),
        "{}",
        r.stdout
    );
    assert!(section("sqlh-ut-deny").contains("\nwrite_policy = \"deny\"\n"));
    // Its refusals, found before anything connects, keep their alternative and never offer
    // --allow-write.
    let r = with(&["--profile", "sqlh-ut-deny", "-Q", "DELETE FROM dbo.T"]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "-- blocked: line 1: DELETE dbo.T would write; nothing was run. Profile \
             sqlh-ut-deny doesn't allow writes (write_policy = \"deny\")\n"
        ),
        "{}",
        r.stdout
    );
    let r = with(&[
        "--profile",
        "sqlh-ut-deny",
        "-Q",
        "INSERT ##shared VALUES (1)",
    ]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "nothing was run. Use a #temp table; profile sqlh-ut-deny doesn't allow writes \
             (write_policy = \"deny\")\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("--allow-write"), "{}", r.stdout);

    // New, with the policy given: that policy, no note, and the advice offers the flag.
    let r = with(&[
        "profile",
        "add",
        "sqlh-ut-flag",
        "-S",
        "127.0.0.1,1",
        "-l",
        "1",
        "--write-policy",
        "flag",
    ]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    assert!(!r.stdout.contains("refuses writes"), "{}", r.stdout);
    assert!(section("sqlh-ut-flag").contains("\nwrite_policy = \"flag\"\n"));
    let r = with(&["--profile", "sqlh-ut-flag", "-Q", "DELETE FROM dbo.T"]);
    assert_eq!(r.code, 5, "{}", r.stdout);
    assert!(
        r.stdout
            .contains("nothing was run. Rerun with --allow-write if the write is intended\n"),
        "{}",
        r.stdout
    );

    // Updated without the option: it keeps its policy, and nothing is said about it.
    let r = with(&["profile", "add", "sqlh-ut-flag", "-d", "AppDb"]);
    assert_eq!(r.code, 2, "{}", r.stdout);
    assert!(!r.stdout.contains("refuses writes"), "{}", r.stdout);
    assert!(section("sqlh-ut-flag").contains("\nwrite_policy = \"flag\"\n"));
}

// The rename to sqlh (DESIGN.md §8): profiles saved under %APPDATA%\sqlharness move to
// %APPDATA%\sqlh the first time sqlh reads them, comments and all, and the old file goes. A
// file already at the new place is never overwritten, and an SQLH_CONFIG file is never
// moved. Windows-auth profiles only, so no password is involved; the move of stored passwords
// has its own test against the real Credential Manager (`credman_moves_a_legacy_credential`).
#[test]
fn profiles_from_before_the_rename_move_to_the_new_folder() {
    let appdata = scratch("rename_migration");
    let (old_dir, new_dir) = (appdata.join("sqlharness"), appdata.join("sqlh"));
    let old_text = "# saved by an earlier sqlh\ndefault = \"dev\"\n\n[profiles.dev]\n\
                    server = \"127.0.0.1,1\"\nauth = \"windows\"\nwrite_policy = \"flag\"\n";
    let list = || {
        let mut command = sqlh();
        command
            .env_remove("SQLH_CONFIG")
            .env("APPDATA", &appdata)
            .args(["profile", "list"]);
        run(&mut command)
    };

    fs::create_dir_all(&old_dir).unwrap();
    fs::write(old_dir.join("profiles.toml"), old_text).unwrap();
    let r = list();
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("\ndev\t1\t127.0.0.1,1\t"), "{}", r.stdout);
    assert_eq!(
        fs::read_to_string(new_dir.join("profiles.toml")).unwrap(),
        old_text,
        "copied as it was"
    );
    assert!(
        !old_dir.join("profiles.toml").exists(),
        "the old file is gone"
    );
    assert!(!old_dir.exists(), "and its empty folder");

    // Once the new file exists, an old one is left alone and not read.
    fs::create_dir_all(&old_dir).unwrap();
    fs::write(
        old_dir.join("profiles.toml"),
        "[profiles.stale]\nserver = \"x\"\nauth = \"windows\"\n",
    )
    .unwrap();
    let r = list();
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(!r.stdout.contains("stale"), "{}", r.stdout);
    assert!(old_dir.join("profiles.toml").exists());

    // An SQLH_CONFIG file is used where it is, with no move.
    let elsewhere = scratch("rename_migration_config");
    let config = elsewhere.join("profiles.toml");
    fs::write(&config, old_text).unwrap();
    let mut command = sqlh();
    command
        .env("SQLH_CONFIG", &config)
        .env("APPDATA", &elsewhere)
        .args(["profile", "list"]);
    let r = run(&mut command);
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(config.exists() && !elsewhere.join("sqlh").exists());
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

    // F06 / PLAN.md 2.2: a loop's row counts are one line, not 300.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_loop_of_inserts_prints_one_row_count_line() {
        let (p, db) = target();
        let sql = "CREATE TABLE #t (i int);\nDECLARE @i int = 0;\n\
                   WHILE @i < 300 BEGIN INSERT #t VALUES (@i); SET @i += 1; END";
        let r = live(&p, &db, &["-Q", sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        let counts: Vec<&str> = r
            .stdout
            .lines()
            .filter(|l| l.contains("affected"))
            .collect();
        assert_eq!(
            counts,
            ["-- 300 statements, 300 rows affected"],
            "{}",
            r.stdout
        );
    }

    // F06 / PLAN.md 2.2: a run over the output budget ends with a valid status and a note,
    // in text and in json, and stays within the budget.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_run_over_the_output_budget_ends_cleanly() {
        let (p, db) = target();
        let sql = "SELECT TOP (500) o.name, o.object_id, o.type_desc FROM sys.all_objects AS o \
                   ORDER BY o.object_id; PRINT 'after';";
        let r = live(&p, &db, &["--max-output", "4096", "-Q", sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(r.stdout.len() <= 4096, "{} bytes", r.stdout.len());
        assert!(
            r.stdout.contains(" of 500 rows shown (--max-output)"),
            "{}",
            r.stdout
        );
        assert!(
            r.stdout.contains("weren't shown: 1 message"),
            "{}",
            r.stdout
        );
        assert!(last_line(&r.stdout).starts_with("-- ok: "), "{}", r.stdout);

        let r = live(
            &p,
            &db,
            &["--max-output", "4096", "--format", "json", "-Q", sql],
        );
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(r.stdout.len() <= 4096, "{} bytes", r.stdout.len());
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["row_count"], 500, "{doc}");
        assert_eq!(result["truncated"], true);
        assert!(
            doc["notes"][0]
                .as_str()
                .unwrap()
                .contains("--max-output limit"),
            "{doc}"
        );
        assert_eq!(doc["ok"], true);
    }

    // F07 / PLAN.md 2.3: a reader that holds stdout open and never reads can't make sqlh
    // hang. With much output, the batch stops waiting after -t seconds. With output that
    // fits sqlh's queue (256 KiB) but not the pipe (64 KiB), the run finishes and the last
    // wait for delivery gives up instead. Either way: exit 7.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_stalled_reader_cannot_hang_the_run() {
        let (p, db) = target();
        let sql = "SELECT TOP (200000) a.object_id, a.name FROM sys.all_objects AS a \
                   CROSS JOIN sys.all_objects AS b";
        for limits in [
            &["--max-rows", "0", "--max-output", "0"][..],
            &["--max-rows", "0", "--max-output", "200000"],
        ] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_sqlh"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args(["--profile", &p, "-d", &db, "-t", "2"])
                .args(limits)
                .args(["-Q", sql])
                .spawn()
                .expect("sqlh starts");
            let _held_open = child.stdout.take();
            let started = Instant::now();
            let status = wait_within(&mut child, Duration::from_secs(30), &format!("{limits:?}"));
            let elapsed = started.elapsed();
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert_eq!(status.code(), Some(7), "{limits:?}: {stderr}");
            assert!(
                stderr.contains("the reader stopped reading for 2 s (-t)"),
                "{limits:?}: {stderr}"
            );
            assert!(elapsed < Duration::from_secs(15), "{limits:?}: {elapsed:?}");
        }
    }

    // PLAN.md 2.7: a large value shown with --max-cell is cut as it arrives, so memory stays
    // flat. The values are built on the server, 60 MB each; before the change a run held two
    // copies of one (about 120 MB at the peak, and 2 GB for a value of 1 GB), and now it holds
    // the 500 characters it shows, plus the few buffers it reads through. The count of what was
    // cut stays exact: the text has a surrogate pair that chunk boundaries fall in, and a
    // double-byte code page.
    #[cfg(windows)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_large_value_is_cut_as_it_arrives() {
        let (p, db) = target();
        let cases = [
            // varchar(max): 60,000,000 characters, one byte each.
            (
                "SELECT REPLICATE(CAST('x' AS varchar(max)), 60000000) AS big",
                format!("{}…[+59999500]", "x".repeat(500)),
            ),
            // nvarchar(max): 20,000,000 characters, 60,000,000 bytes, half of them in pairs.
            (
                "SELECT REPLICATE(CAST(N'é😀' AS nvarchar(max)), 10000000) AS big",
                format!("{}…[+19999500]", "é😀".repeat(250)),
            ),
            // varchar(max) in Japanese code page 932: 20,000,000 characters of two bytes.
            // Built from code points, so that nothing on the way to the server can turn the
            // text into question marks.
            (
                "SELECT REPLICATE(CAST((NCHAR(26085) + NCHAR(26412)) COLLATE Japanese_CI_AS \
                 AS varchar(max)), 10000000) AS big",
                format!("{}…[+19999500]", "日本".repeat(250)),
            ),
            // varbinary(max): 60,000,000 bytes; 500 hex digits are 250 bytes. (REPLICATE
            // turns binary into varchar, so the bytes are made from text.)
            (
                "SELECT CAST(REPLICATE(CAST('ab' AS varchar(max)), 30000000) AS varbinary(max)) \
                 AS big",
                format!("0x{}…[+59999750 bytes]", "6162".repeat(125)),
            ),
        ];
        for (sql, shown) in cases {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
            command.stdin(Stdio::null()).args([
                "--profile",
                &p,
                "-d",
                &db,
                "--max-cell",
                "500",
                "-Q",
                sql,
            ]);
            let (r, peak) = run_measured(&mut command);
            assert_eq!(r.code, 0, "{sql}: {}{}", r.stdout, r.stderr);
            let lines: Vec<&str> = r.stdout.lines().collect();
            assert_eq!(lines[2], shown, "{sql}");
            assert_eq!(lines[3], "-- 1 row; 1 value cut (--max-cell 500)", "{sql}");
            assert!(
                peak < 40 << 20,
                "{sql}: peak working set {} MB for a value of 60 MB",
                peak >> 20
            );
        }
        // With no --max-cell, the output budget is the limit: a value of 40,000,000 characters
        // can't fit in 1,000,000 bytes, so the row is withheld as it was before, and what the
        // run held is still a few megabytes, not 80 MB of text and the same again to write it.
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command.stdin(Stdio::null()).args([
            "--profile",
            &p,
            "-d",
            &db,
            "--max-cell",
            "0",
            "-Q",
            "SELECT REPLICATE(CAST(N'x' AS nvarchar(max)), 40000000) AS big",
        ]);
        let (r, peak) = run_measured(&mut command);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout.contains("\n-- 0 of 1 rows shown (--max-output)\n"),
            "{}",
            r.stdout
        );
        assert!(peak < 40 << 20, "peak working set {} MB", peak >> 20);
        // The same in json, which builds its rows in a buffer of its own.
        let sql = "SELECT REPLICATE(CAST('x' AS varchar(max)), 60000000) AS big";
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command.stdin(Stdio::null()).args([
            "--profile",
            &p,
            "-d",
            &db,
            "--max-cell",
            "500",
            "--format",
            "json",
            "-Q",
            sql,
        ]);
        let (r, peak) = run_measured(&mut command);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(
            result["rows"][0][0],
            format!("{}…[+59999500]", "x".repeat(500))
        );
        assert_eq!(result["cells_cut"], 1);
        assert!(peak < 40 << 20, "peak working set {} MB", peak >> 20);
    }

    // PLAN.md 2.7: rows past --max-rows are counted without decoding their values. The first
    // row is small and shown; the five after it hold 40 MB each, and are skipped chunk by
    // chunk. Reading each into memory first took 40 MB or more at the peak.
    #[cfg(windows)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn hidden_rows_are_not_materialized() {
        let (p, db) = target();
        let sql = "SELECT n, REPLICATE(CAST('x' AS varchar(max)), CASE WHEN n = 1 THEN 10 \
                   ELSE 40000000 END) AS v FROM (VALUES (1), (2), (3), (4), (5), (6)) AS t(n)";
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command.stdin(Stdio::null()).args([
            "--profile",
            &p,
            "-d",
            &db,
            "--max-rows",
            "1",
            // Skipping 200 MB takes more than the 250 ms a bounded count would give it.
            "--exact-count",
            "-Q",
            sql,
        ]);
        let (r, peak) = run_measured(&mut command);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout
                .contains("\n1\txxxxxxxxxx\n-- 1 of 6 rows shown (--max-rows 1)\n"),
            "{}",
            r.stdout
        );
        assert!(peak < 25 << 20, "peak working set {} MB", peak >> 20);
    }

    // PLAN.md 2.5 (docs/notes/harness-kill.md): a harness that kills sqlh outright, here with
    // TerminateProcess, leaves no work running on the server. The session is gone within
    // seconds, for a waiting query and for a CPU-bound one that sends nothing. The note measured
    // 46 runs of this kind, the slowest 0.14 s; the bound here is wide because the observer is
    // a fresh connection that takes a few hundred ms to log in. Sessions are matched on id and
    // login time, since the server hands the freed id to the next login.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_killed_run_leaves_no_work_on_the_server() {
        use std::io::{BufRead, BufReader};
        use std::sync::mpsc;

        /// Kills the run if the test fails before it does.
        struct Victim(std::process::Child);
        impl Drop for Victim {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let (p, db) = target();
        // The sessions with that id and login time, and the requests of that id, on the server.
        let watch = |spid: &str, login: &str| -> (u32, u32) {
            let sql = format!(
                "SELECT (SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE session_id = {spid} \
                 AND login_time = CONVERT(datetime, '{login}', 126)) AS sessions, \
                 (SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = {spid}) AS requests"
            );
            let r = live(&p, &db, &["-Q", &sql]);
            assert_eq!(r.code, 0, "{}", r.stdout);
            r.stdout
                .lines()
                .find_map(|line| {
                    let (a, b) = line.split_once('\t')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                })
                .unwrap_or_else(|| panic!("no counts in {}", r.stdout))
        };
        let ident = "SELECT @@SPID AS spid, CONVERT(varchar(27), (SELECT login_time FROM \
                     sys.dm_exec_sessions WHERE session_id = @@SPID), 126) AS lt";
        // About 28 s on one scheduler, with no waits and no output until it ends.
        let cpu = "SELECT COUNT_BIG(*) AS n, SUM(CONVERT(bigint, a.object_id) * (b.object_id % 7) \
                   + c.column_id) AS s FROM sys.all_objects AS a CROSS JOIN sys.all_objects AS b \
                   CROSS JOIN (SELECT TOP (20) column_id FROM sys.all_columns ORDER BY object_id, \
                   column_id) AS c OPTION (MAXDOP 1)";
        // WAITFOR needs --allow-write; the CPU-bound query runs read-only, in the wrapper.
        for (what, work, write) in [
            ("waiting", "WAITFOR DELAY '00:01:00'", true),
            ("CPU-bound", cpu, false),
        ] {
            let script = format!("{ident}\nGO\n{work}");
            let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args(["--profile", &p, "-d", &db, "-t", "0"]);
            if write {
                command.arg("--allow-write");
            }
            let mut victim = Victim(command.args(["-Q", &script]).spawn().expect("sqlh starts"));
            // The first batch's row says which session to watch; it is written when that batch
            // ends, long before the second one does.
            let stdout = victim.0.stdout.take().unwrap();
            let (lines, rows) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if lines.send(line).is_err() {
                        break;
                    }
                }
            });
            let (spid, login) = loop {
                let line = rows
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap_or_else(|_| panic!("{what}: sqlh didn't report its session"));
                if let Some((spid, login)) = line.split_once('\t')
                    && spid.parse::<u32>().is_ok()
                    && login.contains('T')
                {
                    break (spid.to_string(), login.to_string());
                }
            };
            // The server is doing the work before the kill.
            let waiting = Instant::now();
            while watch(&spid, &login) != (1, 1) {
                assert!(
                    waiting.elapsed() < Duration::from_secs(20),
                    "{what}: the work never showed on the server"
                );
            }
            std::thread::sleep(Duration::from_millis(500));
            victim.0.kill().expect("TerminateProcess");
            let killed = Instant::now();
            let _ = victim.0.wait();
            while watch(&spid, &login).0 != 0 {
                assert!(
                    killed.elapsed() < Duration::from_secs(10),
                    "{what}: session {spid} was still on the server {:?} after the kill",
                    killed.elapsed()
                );
            }
            let gone = killed.elapsed();
            assert!(gone < Duration::from_secs(5), "{what}: {gone:?}");
        }
    }

    // PLAN.md 2.4: Ctrl+Break cancels a running statement. The run says what sqlh saw, that the
    // connection stayed usable, and not that the server confirmed the cancel: the driver gives
    // no receipt for its attention (see `Session::failure`).
    #[cfg(windows)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn ctrl_break_cancels_a_running_query() {
        use std::io::BufRead;
        let (p, db) = target();
        let mut child = spawn_in_group(
            Command::new(env!("CARGO_BIN_EXE_sqlh"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args(["--profile", &p, "-d", &db, "--verbose", "--allow-write"])
                .args(["-Q", "WAITFOR DELAY '00:00:30'"]),
        );
        // --verbose reports the connection just before the batch is sent.
        let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        while !line.starts_with("sqlh: profile ") {
            line.clear();
            let read = stderr.read_line(&mut line).unwrap();
            assert_ne!(read, 0, "sqlh ended before it connected");
        }
        // Quiet for longer than the 1 s the closing lines get after a cancel: time with
        // nothing to write must not count as a stalled reader (that was exit 7).
        std::thread::sleep(Duration::from_millis(1500));
        let started = Instant::now();
        ctrl_break(&child);
        let status = wait_within(&mut child, Duration::from_secs(10), "Ctrl+Break");
        let elapsed = started.elapsed();
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let mut rest = String::new();
        stderr.read_to_string(&mut rest).unwrap();
        assert_eq!(status.code(), Some(6), "{stdout}{rest}");
        assert!(
            stdout.contains(
                "-- error: cancelled by Ctrl+Break: the statement was cancelled; the connection \
                 stayed usable\n"
            ),
            "{stdout}"
        );
        assert!(!stdout.contains("confirmed"), "{stdout}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    // PLAN.md 2.4: Ctrl+Break also ends a run that waits for a stalled reader, without waiting
    // out -t. What was written can't be delivered, so the exit is 7, and stderr says that the
    // run itself was cancelled.
    #[cfg(windows)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn ctrl_break_cancels_a_run_waiting_for_its_reader() {
        let (p, db) = target();
        let sql = "SELECT TOP (200000) a.object_id, a.name FROM sys.all_objects AS a \
                   CROSS JOIN sys.all_objects AS b";
        let mut child = spawn_in_group(
            Command::new(env!("CARGO_BIN_EXE_sqlh"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args(["--profile", &p, "-d", &db, "-t", "60"])
                .args(["--max-rows", "0", "--max-output", "0", "-Q", sql]),
        );
        let _held_open = child.stdout.take();
        // Time to connect and fill both the pipe and sqlh's queue; -t is far off.
        std::thread::sleep(Duration::from_secs(4));
        let started = Instant::now();
        ctrl_break(&child);
        let status = wait_within(&mut child, Duration::from_secs(10), "Ctrl+Break");
        let elapsed = started.elapsed();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert_eq!(status.code(), Some(7), "{stderr}");
        assert!(
            stderr.contains("the run itself ended with exit 6: failed (exit 6): cancelled, ")
                && stderr.contains(
                    "(cancelled by Ctrl+Break: cancelled while waiting for the reader; the \
                     connection was closed)"
                ),
            "{stderr}"
        );
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    // R12: in text and json, --verbose lines go to stderr on a thread of their own, so an open
    // stderr that nobody reads can't hold up the response, even with -t 0. The run keeps its
    // outcome and ends within the diagnostics' own short wait.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn an_unread_stderr_cannot_hold_up_a_verbose_run() {
        let (p, db) = target();
        for format in ["text", "json"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
            command.stdin(Stdio::null()).args([
                "--profile",
                &p,
                "-d",
                &db,
                "--verbose",
                "-t",
                "0",
                "--format",
                format,
                "-Q",
                "SELECT 1 AS x",
            ]);
            let readers = stall(&mut command, false, true);
            let started = Instant::now();
            let mut child = command.spawn().expect("sqlh starts");
            let status = wait_within(&mut child, Duration::from_secs(20), format);
            let elapsed = started.elapsed();
            let r = finished(&mut child, status);
            drop(readers);
            assert_eq!(r.code, 0, "{format}: {}", r.stdout);
            if format == "json" {
                let doc: serde_json::Value =
                    serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{e}: {}", r.stdout));
                assert_eq!(doc["exit_code"], 0, "{}", r.stdout);
            } else {
                assert!(
                    last_line(&r.stdout).starts_with("-- ok: 1 batch"),
                    "{}",
                    r.stdout
                );
            }
            assert!(elapsed < Duration::from_secs(10), "{format}: {elapsed:?}");
        }
    }

    /// `(what comes before the tail, server, database, connect_ms)` of a status line that ends
    /// `; server/database, connect N ms`. The status of the runs below holds no other `; `.
    fn split_tail(status: &str) -> (String, String, String, u64) {
        let (head, tail) = status
            .rsplit_once("; ")
            .unwrap_or_else(|| panic!("no tail on the status line: {status}"));
        let (names, ms) = tail
            .strip_suffix(" ms")
            .and_then(|tail| tail.rsplit_once(", connect "))
            .unwrap_or_else(|| panic!("not a tail: {tail}"));
        let (server, database) = names
            .split_once('/')
            .unwrap_or_else(|| panic!("no server/database in {names}"));
        let ms = ms.parse().unwrap_or_else(|_| panic!("not a time: {ms}"));
        (head.to_owned(), server.into(), database.into(), ms)
    }

    // PLAN.md 2.11, owner decision 10, DESIGN.md §5.1 and §5.3: in text and csv the status line
    // ends with the server, the database the run started in and the connect time, so the first
    // response shows where it ran (agent trial 1 spent a call finding out it was in master).
    // The database is the profile's own, not the one a later USE moved the session to; csv has
    // the same tail on stderr and nothing of it on stdout; json agrees on the server and the
    // database and doesn't repeat the tail in its `summary`; a run that fails after connecting
    // has it too.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_status_line_names_the_profiles_database() {
        let (p, db) = target();
        let sql = "SELECT 1 AS a\nGO\nUSE master\nGO\nSELECT DB_NAME() AS db";

        let r = live(&p, &db, &["-Q", sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        // The session ended in master, and the status line still says where the run began.
        assert!(r.stdout.contains("\nmaster\n"), "{}", r.stdout);
        let (head, server, database, connect_ms) = split_tail(last_line(&r.stdout));
        assert!(head.starts_with("-- ok: 3 batches, "), "{head}");
        assert_eq!(database, db);
        assert!(!server.is_empty() && server != "?", "{server}");
        // The run's own time is the one before the tail, and it includes the connect.
        let elapsed: u64 = head
            .strip_suffix(" ms")
            .and_then(|head| head.rsplit(", ").next())
            .and_then(|ms| ms.parse().ok())
            .unwrap_or_else(|| panic!("no elapsed time in {head}"));
        assert!(connect_ms <= elapsed, "{connect_ms} > {elapsed}");

        // csv: the metadata stream, and only it.
        let csv = live(&p, &db, &["--format", "csv", "-Q", sql]);
        assert_eq!(csv.code, 0, "{}", csv.stderr);
        let (csv_head, csv_server, csv_database, _) = split_tail(last_line(&csv.stderr));
        assert!(csv_head.starts_with("-- ok: 3 batches, "), "{csv_head}");
        assert_eq!(
            (csv_server.as_str(), csv_database.as_str()),
            (server.as_str(), db.as_str())
        );
        assert!(!csv.stdout.contains("connect "), "{}", csv.stdout);

        // json: the same server and database in `connection`; the summary is the status
        // without the tail.
        let json = live(&p, &db, &["--format", "json", "-Q", sql]);
        assert_eq!(json.code, 0, "{}", json.stdout);
        let doc: serde_json::Value = serde_json::from_str(&json.stdout).expect("valid JSON");
        let json_server = doc["connection"]["server"].as_str().expect("a server");
        assert_eq!(text_escaped(json_server), server);
        assert_eq!(doc["connection"]["database"], db);
        let summary = doc["summary"].as_str().expect("a summary");
        assert!(summary.starts_with("ok: 3 batches, ") && !summary.contains("connect "));

        // A failure after connecting is told where it happened too.
        let r = live(&p, &db, &["-Q", "SELECT * FROM dbo.sqlh_no_such_table"]);
        assert_eq!(r.code, 1, "{}", r.stdout);
        let (head, _, database, _) = split_tail(last_line(&r.stdout));
        assert!(head.starts_with("-- failed (exit 1): 1 error"), "{head}");
        assert_eq!(database, db);

        // With no -d the run starts in the profile's database, whatever that is: the one the
        // same session reports for itself.
        let r = run(Command::new(env!("CARGO_BIN_EXE_sqlh"))
            .stdin(Stdio::null())
            .args(["--profile", &p, "-Q", "SELECT DB_NAME() AS db"]));
        assert_eq!(r.code, 0, "{}", r.stdout);
        let (_, _, database, _) = split_tail(last_line(&r.stdout));
        assert!(
            r.stdout.contains(&format!("\n{database}\n")),
            "the status line says {database}\n{}",
            r.stdout
        );
    }

    // R10: Ctrl+Break during the last wait of a run that completed. `SELECT 1` has run and
    // its few bytes wait for a reader that stopped reading; -t 0 puts no limit on that. The
    // signal ends the wait: exit 7 (not Windows' 0xC000013A), promptly, and stderr says the
    // run itself ended with exit 0.
    #[cfg(windows)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn ctrl_break_cancels_the_final_delivery_of_a_completed_run() {
        let (p, db) = target();
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command.stdin(Stdio::null()).args([
            "--profile",
            &p,
            "-d",
            &db,
            "-t",
            "0",
            "-Q",
            "SELECT 1 AS x",
        ]);
        let readers = stall(&mut command, true, false);
        let mut child = spawn_in_group(&mut command);
        // Time to connect, run, and block on delivering the result.
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            child.try_wait().unwrap().is_none(),
            "sqlh should be waiting for its reader"
        );
        let started = Instant::now();
        ctrl_break(&child);
        let status = wait_within(&mut child, Duration::from_secs(10), "Ctrl+Break");
        let elapsed = started.elapsed();
        let r = finished(&mut child, status);
        drop(readers);
        assert_eq!(r.code, 7, "exit {:#x}\n{}", r.code, r.stderr);
        assert!(
            r.stderr
                .contains("; cancelled by Ctrl+Break while delivering); ")
                && r.stderr
                    .contains("the run itself ended with exit 0: ok: 1 batch, "),
            "{}",
            r.stderr
        );
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    // PLAN.md 2.1: --verbose says where the time to the first batch went; a build with
    // `--features timing` also names the connect steps.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn verbose_reports_connect_timing() {
        let (p, db) = target();
        let r = live(&p, &db, &["--verbose", "-Q", "SELECT 1 AS x"]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        let line = r
            .stderr
            .lines()
            .find(|l| l.starts_with("sqlh: profile "))
            .unwrap_or_else(|| panic!("no --verbose line: {}", r.stderr));
        assert!(
            line.contains("; connected in ") && line.contains(", session setup "),
            "{line}"
        );
        if cfg!(feature = "timing") {
            for step in [" (tcp ", ", tls ", ", login "] {
                assert!(line.contains(step), "{step}: {line}");
            }
        } else {
            assert!(!line.contains("(tcp "), "{line}");
        }
    }

    // PLAN.md 2.8, owner decision 7: what a connection looks like to the server. The lock
    // timeout and the deadlock priority are in every session, writes allowed or not;
    // READ UNCOMMITTED (isolation level 1, against the default READ COMMITTED, 2) only when
    // asked for, with its note once.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn every_session_gets_the_agent_defaults() {
        let (p, db) = target();
        let sql = "SELECT @@LOCK_TIMEOUT AS lock_timeout, s.deadlock_priority, \
                   s.transaction_isolation_level AS isolation \
                   FROM sys.dm_exec_sessions AS s WHERE s.session_id = @@SPID";
        let note = "-- note: reads were uncommitted (--read-uncommitted): rows may include \
                    uncommitted, duplicated or missing data\n";
        // (flags, @@LOCK_TIMEOUT in ms, isolation level, whether the note is said)
        let cases: [(&[&str], &str, &str, bool); 9] = [
            (&[], "10000", "2", false),
            (&["--lock-timeout", "3"], "3000", "2", false),
            (&["--lock-timeout", "0"], "-1", "2", false),
            (&["--allow-write"], "10000", "2", false),
            (
                &["--allow-write", "--lock-timeout", "4"],
                "4000",
                "2",
                false,
            ),
            (&["--read-uncommitted"], "10000", "1", true),
            (
                &["--read-uncommitted", "--lock-timeout", "0"],
                "-1",
                "1",
                true,
            ),
            (&["--read-uncommitted=false"], "10000", "2", false),
            // Nothing is read, so there is nothing to make uncommitted or to report.
            (&["--check", "--read-uncommitted"], "", "", false),
        ];
        for (flags, lock_ms, isolation, noted) in cases {
            let mut args = flags.to_vec();
            args.extend(["-Q", sql]);
            let r = live(&p, &db, &args);
            assert_eq!(r.code, 0, "{flags:?}: {}", r.stdout);
            // -5 is DEADLOCK_PRIORITY LOW.
            if !lock_ms.is_empty() {
                assert!(
                    r.stdout
                        .contains(&format!("\n{lock_ms}\t-5\t{isolation}\n-- 1 row\n")),
                    "{flags:?}: {}",
                    r.stdout
                );
            }
            assert_eq!(r.stdout.contains(note), noted, "{flags:?}: {}", r.stdout);
            assert_eq!(
                r.stdout.matches("reads were uncommitted").count(),
                usize::from(noted),
                "{flags:?}: {}",
                r.stdout
            );
        }

        // json carries the note with the other notes, once.
        let r = live(
            &p,
            &db,
            &["--format", "json", "--read-uncommitted", "-Q", sql],
        );
        assert_eq!(r.code, 0, "{}", r.stdout);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("a json document");
        assert_eq!(
            doc["notes"],
            serde_json::json!([
                "reads were uncommitted (--read-uncommitted): rows may include uncommitted, \
                 duplicated or missing data"
            ])
        );
        assert_eq!(
            doc["batches"][0]["events"][0]["rows"][0],
            serde_json::json!([10000, -5, 1])
        );
        // csv carries it on stderr, the metadata stream.
        let r = live(
            &p,
            &db,
            &["--format", "csv", "--read-uncommitted", "-Q", sql],
        );
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert_eq!(r.stderr.matches(note).count(), 1, "{}", r.stderr);
        assert!(!r.stdout.contains("uncommitted"), "{}", r.stdout);
    }

    /// A sqlh run started in the background.
    fn spawn_live(profile: &str, database: &str, args: &[&str]) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_sqlh"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["--profile", profile, "-d", database])
            .args(args)
            .spawn()
            .expect("sqlh starts")
    }

    /// Waits for a run started with `spawn_live`.
    fn collect(child: std::process::Child) -> Run {
        let output = child.wait_with_output().expect("sqlh finishes");
        Run {
            code: output.status.code().expect("an exit code, not a signal"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// A background run that is killed when it goes out of scope, so a failed assertion
    /// doesn't leave a transaction open on the server. Killing the process drops its
    /// connection, and the server rolls the transaction back.
    struct Reaped(Option<std::process::Child>);

    impl Reaped {
        fn child(&mut self) -> &mut std::process::Child {
            self.0.as_mut().expect("not yet collected")
        }
    }

    impl Drop for Reaped {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Starts a run that inserts into `table` and keeps its transaction open for a minute,
    /// and waits until a read of the table shows that the write is there: it blocks, and
    /// fails with error 1222 after the 1 s timeout it is given.
    fn hold_a_write(profile: &str, database: &str, table: &str) -> Reaped {
        let hold =
            format!("BEGIN TRAN; INSERT {table} VALUES (1); WAITFOR DELAY '00:01:00'; ROLLBACK");
        let mut blocker = Reaped(Some(spawn_live(
            profile,
            database,
            &["--allow-write", "-Q", &hold],
        )));
        let read = format!("SELECT COUNT(*) AS n FROM {table}");
        let give_up = Instant::now() + Duration::from_secs(40);
        loop {
            let r = live(profile, database, &["--lock-timeout", "1", "-Q", &read]);
            if r.code == 1 && r.stdout.contains("-- error 1222 ") {
                return blocker;
            }
            if let Some(status) = blocker.child().try_wait().expect("child status") {
                panic!(
                    "the blocking run ended early ({status}); the last read: {}",
                    r.stdout
                );
            }
            assert!(
                Instant::now() < give_up,
                "the write never blocked the read; the last read: {}",
                r.stdout
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // PLAN.md 2.8: a read that waits behind another session's open write gives up after
    // --lock-timeout (default 10 s, here 2) with error 1222: an ordinary SQL error, exit 1,
    // on the line of the statement that waited. The same read with --read-uncommitted takes no
    // shared lock, so it doesn't wait: it returns at once, with the uncommitted row, and says
    // so. (sqlh_test has no row versioning, so READ COMMITTED reads take locks.)
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_blocked_read_fails_after_the_lock_timeout_unless_it_reads_uncommitted() {
        let (p, db) = target();
        let probe = Probe::create(&p, &db);
        let t = &probe.name;
        let _blocker = hold_a_write(&p, &db, t);

        let read = format!("SELECT COUNT(*) AS n FROM {t}");
        let started = Instant::now();
        let r = live(&p, &db, &["--lock-timeout", "2", "-Q", &read]);
        let waited = started.elapsed();
        assert_eq!(r.code, 1, "{}", r.stdout);
        assert!(
            r.stdout.contains("\n-- error 1222 (severity 16, state ")
                && r.stdout
                    .contains(", line 1): Lock request time out period exceeded.\n"),
            "{}",
            r.stdout
        );
        assert!(
            last_line(&r.stdout).starts_with("-- failed (exit 1): 1 error, first in batch 1"),
            "{}",
            r.stdout
        );
        assert!(
            waited >= Duration::from_millis(1900) && waited < Duration::from_secs(8),
            "the read waited {waited:?} for a 2 s lock timeout"
        );

        // In a file, the error is on the line of the statement, in the right batch.
        let dir = scratch("lock_timeout_lines");
        let input = dir.join("blocked.sql");
        fs::write(
            &input,
            format!("SELECT 1 AS a\nGO\n-- waits here\nSELECT COUNT(*) AS n\nFROM {t}\nGO\nSELECT 2 AS b\n"),
        )
        .unwrap();
        let r = live(
            &p,
            &db,
            &[
                "--lock-timeout",
                "1",
                "--continue-on-error",
                "-i",
                input.to_str().unwrap(),
            ],
        );
        assert_eq!(r.code, 1, "{}", r.stdout);
        assert!(
            r.stdout.contains(
                "-- batch 2 (line 3)\n-- result 2\nn:int\n-- 0 rows so far (interrupted)\n-- error 1222 (severity 16, state "
            ) && r.stdout.contains(", line 4): Lock request time out period exceeded.\n"),
            "{}",
            r.stdout
        );
        // Later batches still run, with the session intact: an error 1222 doesn't roll the
        // wrapper transaction back.
        assert!(
            r.stdout
                .contains("-- batch 3 (line 7)\n-- result 3\nb:int\n2\n"),
            "{}",
            r.stdout
        );

        // The same read, dirty, with the default lock timeout: nothing to wait for.
        let started = Instant::now();
        let r = live(&p, &db, &["--read-uncommitted", "-Q", &read]);
        let elapsed = started.elapsed();
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(
            r.stdout.starts_with(
                "-- note: reads were uncommitted (--read-uncommitted): rows may include \
                 uncommitted, duplicated or missing data\n-- result 1\nn:int\n1\n-- 1 row\n-- ok: "
            ),
            "{}",
            r.stdout
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the dirty read took {elapsed:?}"
        );
    }

    // The note counts against --max-output like any note: the same rows, the same cap, fewer
    // rows fit when the run read uncommitted, and the cap still holds.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_read_uncommitted_note_counts_against_the_output_cap() {
        let (p, db) = target();
        // 51 bytes a row.
        let sql = "SELECT TOP 1000 REPLICATE('x', 50) AS c FROM sys.all_objects";
        let shown = |r: &Run| r.stdout.lines().filter(|l| l.starts_with("xxxx")).count();
        let clean = live(
            &p,
            &db,
            &["--max-output", "4096", "--max-rows", "0", "-Q", sql],
        );
        let dirty = live(
            &p,
            &db,
            &[
                "--max-output",
                "4096",
                "--max-rows",
                "0",
                "--read-uncommitted",
                "-Q",
                sql,
            ],
        );
        for r in [&clean, &dirty] {
            assert_eq!(r.code, 0, "{}", r.stdout);
            assert!(response_bytes(r) <= 4096, "{} bytes", response_bytes(r));
            assert!(
                r.stdout.contains("rows shown (--max-output)"),
                "{}",
                r.stdout
            );
        }
        assert!(
            dirty.stdout.starts_with("-- note: reads were uncommitted"),
            "{}",
            dirty.stdout
        );
        let note_bytes = dirty.stdout.lines().next().unwrap().len() + 1;
        let (clean_rows, dirty_rows) = (shown(&clean), shown(&dirty));
        assert!(
            dirty_rows < clean_rows && (clean_rows - dirty_rows) * 51 >= note_bytes - 51,
            "{clean_rows} rows fit without the note and {dirty_rows} with it ({note_bytes} bytes)"
        );
    }

    // PLAN.md 2.8: DEADLOCK_PRIORITY LOW makes the agent's session the victim when it
    // deadlocks with ordinary work. The two sessions update the same two tables in opposite
    // order; the other one (NORMAL, the server's default) wins, and sqlh's run ends with
    // error 1205, an ordinary SQL error (exit 1) on the line of the statement that was chosen.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_agent_session_loses_a_deadlock() {
        let (p, db) = target();
        let one = Probe::create(&p, &db);
        let two = Probe::create(&p, &db);
        let (a, b) = (&one.name, &two.name);
        let seed = format!("INSERT {a} VALUES (1); INSERT {b} VALUES (1)");
        assert_eq!(live(&p, &db, &["--allow-write", "-Q", &seed]).code, 0);

        let agent = format!(
            "BEGIN TRAN;\nUPDATE {a} SET id = id;\nWAITFOR DELAY '00:00:02';\nUPDATE {b} SET id = id;\nROLLBACK;"
        );
        let production = format!(
            "SET DEADLOCK_PRIORITY NORMAL;\nBEGIN TRAN;\nUPDATE {b} SET id = id;\nWAITFOR DELAY '00:00:02';\nUPDATE {a} SET id = id;\nROLLBACK;"
        );
        let started = Instant::now();
        let agent = spawn_live(&p, &db, &["--allow-write", "-Q", &agent]);
        let production = spawn_live(&p, &db, &["--allow-write", "-Q", &production]);
        let (agent, production) = (collect(agent), collect(production));
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "{:?}",
            started.elapsed()
        );

        assert_eq!(agent.code, 1, "{}", agent.stdout);
        assert!(
            agent
                .stdout
                .contains("\n-- error 1205 (severity 13, state ")
                && agent.stdout.contains(", line 4): Transaction (Process ID ")
                && agent
                    .stdout
                    .contains("has been chosen as the deadlock victim. Rerun the transaction.\n"),
            "{}",
            agent.stdout
        );
        assert_eq!(production.code, 0, "{}", production.stdout);
    }

    // PLAN.md 2.12 / DESIGN.md §5.1 and §5.2: where a message lands. No query was found that
    // makes SQL Server send a message between two rows of a result: warnings raised per row
    // (an aggregate that drops a NULL, an arithmetic warning) are held until the end of the
    // statement, even in results of 50,000 and of 1.2 million rows (a parallel plan), and
    // PRINT and RAISERROR can't run inside a query. The nearest cases are what this pins: a
    // warning the server sends after the last row but before the statement is over, and an
    // error that ends the rows. Each lands after the result, never among its rows, in every
    // format.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_message_sent_during_a_result_lands_after_it() {
        use serde_json::Value;
        let (p, db) = target();
        // Dropping a NULL in an aggregate warns once. Rows 4 to 6 have one.
        let warning = "SELECT x.n, (SELECT SUM(t.v) FROM (SELECT CASE WHEN x.n >= 4 THEN NULL \
                       ELSE 1 END AS v UNION ALL SELECT 1) AS t) AS s \
                       FROM (VALUES (1), (2), (3), (4), (5), (6)) AS x(n)";
        // The division fails on the third row, ending the statement and its rows.
        let failing = "SELECT n, 10 / (n - 3) AS q FROM (VALUES (1), (2), (3), (4), (5)) AS x(n)";
        for (what, sql, code, then) in [
            ("a warning", warning, 0, "-- msg: "),
            ("an error", failing, 1, "-- error 8134 "),
        ] {
            for format in ["text", "csv"] {
                let r = live(&p, &db, &["--format", format, "-Q", sql]);
                assert_eq!(r.code, code, "{what}, {format}: {}{}", r.stdout, r.stderr);
                // text keeps everything on stdout; csv has the rows there and the rest on stderr.
                let lines: Vec<&str> = if format == "csv" {
                    &r.stderr
                } else {
                    &r.stdout
                }
                .lines()
                .collect();
                let footer = lines
                    .iter()
                    .position(|l| {
                        l.starts_with("-- ")
                            && (l.ends_with(" rows") || l.ends_with("(interrupted)"))
                    })
                    .unwrap_or_else(|| panic!("{what}, {format}: no footer: {lines:?}"));
                // R17: the rows an error cut short are not the result, and the footer says so.
                assert_eq!(
                    lines[footer].ends_with("rows so far (interrupted)"),
                    code == 1,
                    "{what}, {format}: {lines:?}"
                );
                assert!(
                    lines[footer + 1].starts_with(then),
                    "{what}, {format}: {lines:?}"
                );
                assert!(
                    !lines[..footer]
                        .iter()
                        .any(|l| l.starts_with("-- msg:") || l.starts_with("-- error")),
                    "{what}, {format}: {lines:?}"
                );
            }
            let r = live(&p, &db, &["--format", "json", "-Q", sql]);
            assert_eq!(r.code, code, "{what}, json: {}", r.stdout);
            let doc: Value = serde_json::from_str(&r.stdout).expect("valid JSON");
            let events = doc["batches"][0]["events"].as_array().expect("events");
            assert_eq!(events.len(), 2, "{what}, json: {doc}");
            assert_eq!(events[0]["type"], "result", "{what}, json: {doc}");
            // R17: the warning's result is complete and exact; the one an error cut short is
            // marked interrupted and its count is not the total.
            assert_eq!(
                events[0].get("interrupted").and_then(Value::as_bool),
                (code == 1).then_some(true),
                "{what}, json: {doc}"
            );
            assert_eq!(
                events[0]["row_count_exact"],
                code == 0,
                "{what}, json: {doc}"
            );
            assert_eq!(
                events[0]["rows"].as_array().map(Vec::len),
                events[0]["row_count"].as_u64().map(|n| n as usize),
                "{what}, json: {doc}"
            );
            let kind = if code == 0 { "msg" } else { "error" };
            assert_eq!(events[1]["type"], kind, "{what}, json: {doc}");
        }
    }

    // R17: the incomplete state is the failed result's own. A statement-terminating error
    // (the divide by zero) leaves the batch running, so a later result in the same batch is
    // exact, and so is a result in a later batch under --continue-on-error. The sink unit
    // tests cover the closure itself in each format.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn an_incomplete_result_does_not_make_the_later_results_incomplete() {
        use serde_json::Value;
        let (p, db) = target();
        let sql = "SELECT 1 AS first_ok\n\
                   SELECT n, 10 / (n - 3) AS q FROM (VALUES (1), (2), (3), (4), (5)) AS x(n)\n\
                   SELECT 7 AS later\n\
                   GO\n\
                   SELECT 8 AS next";
        let r = live(
            &p,
            &db,
            &["--format", "json", "--continue-on-error", "-Q", sql],
        );
        assert_eq!(r.code, 1, "{}", r.stdout);
        let doc: Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let batches = doc["batches"].as_array().expect("batches");
        assert_eq!(batches.len(), 2, "{doc}");
        let events = batches[0]["events"].as_array().expect("events");
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| e["type"].as_str().expect("a type"))
            .collect();
        assert_eq!(kinds, ["result", "result", "error", "result"], "{doc}");
        let exact: Vec<bool> = [0, 1, 3]
            .iter()
            .map(|&i| events[i]["row_count_exact"].as_bool().expect("a bool"))
            .collect();
        assert_eq!(exact, [true, false, true], "{doc}");
        assert_eq!(events[1]["interrupted"], true, "{doc}");
        for i in [0, 3] {
            assert!(events[i].get("interrupted").is_none(), "{doc}");
        }
        assert_eq!(events[3]["rows"], serde_json::json!([[7]]), "{doc}");
        let next = &batches[1]["events"][0];
        assert_eq!(next["rows"], serde_json::json!([[8]]), "{doc}");
        assert_eq!(next["row_count_exact"], true, "{doc}");
        assert!(next.get("interrupted").is_none(), "{doc}");

        // The same input in text: the footers, in order, around the error.
        let r = live(&p, &db, &["--continue-on-error", "-Q", sql]);
        assert_eq!(r.code, 1, "{}", r.stdout);
        let footers: Vec<&str> = r
            .stdout
            .lines()
            .filter(|l| l.starts_with("-- ") && (l.contains(" row")))
            .filter(|l| !l.contains("affected"))
            .collect();
        assert_eq!(footers.len(), 4, "{}", r.stdout);
        assert_eq!(footers[0], "-- 1 row", "{}", r.stdout);
        assert!(
            footers[1].ends_with(" rows so far (interrupted)")
                || footers[1].ends_with(" row so far (interrupted)"),
            "{}",
            r.stdout
        );
        assert_eq!(&footers[2..], ["-- 1 row", "-- 1 row"], "{}", r.stdout);
    }

    // PLAN.md 2.11: a JSON document starts with the contract version and says where the run
    // went: the connection, taken from the login so the first response shows the database;
    // how long connecting took; and how long each batch took. The database is the one the run
    // started in, not the one a later USE moved the session to.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn json_says_where_the_run_went_and_how_long_it_took() {
        let (p, db) = target();
        let contract = contract_version();
        let sql = "SELECT 1 AS a\nGO\nUSE master\nGO\nSELECT DB_NAME() AS db";
        let r = live(&p, &db, &["--format", "json", "-Q", sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(
            r.stdout.starts_with(&format!(
                "{{\"contract_version\":\"{contract}\",\"connection\":{{\"server\":"
            )),
            "{}",
            r.stdout
        );
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let connection = doc["connection"].as_object().expect("a connection");
        let mut keys: Vec<&str> = connection.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["auth", "database", "encrypted", "server", "server_version"]
        );
        assert_eq!(connection["database"], db);
        assert!(connection["server"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(matches!(
            connection["auth"].as_str(),
            Some("windows" | "sql")
        ));
        assert!(connection["encrypted"].is_boolean());
        let version = connection["server_version"].as_str().expect("a version");
        assert!(
            version.split('.').count() == 3 && version.split('.').all(|n| n.parse::<u16>().is_ok()),
            "{version}"
        );
        // The session ended in master, and the connection still says where it started.
        assert_eq!(
            doc["batches"][2]["events"][0]["rows"],
            serde_json::json!([["master"]])
        );
        // Connecting, then three batches, all inside the run.
        let connect_ms = doc["connect_ms"].as_u64().expect("connect_ms");
        let batches = doc["batches"].as_array().unwrap();
        assert_eq!(batches.len(), 3);
        let in_batches: u64 = batches
            .iter()
            .map(|b| b["elapsed_ms"].as_u64().expect("elapsed_ms per batch"))
            .sum();
        let run = doc["elapsed_ms"].as_u64().unwrap();
        assert!(
            connect_ms + in_batches <= run,
            "{connect_ms} + {in_batches} > {run}"
        );
    }

    // PLAN.md 2.11: a run cut short by --max-output still has its connection, its times and
    // its outcome, and is valid JSON no longer than the cap.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_cut_json_run_keeps_its_connection_and_times() {
        let (p, db) = target();
        let sql = "SELECT TOP (500) o.name, o.object_id, o.type_desc FROM sys.all_objects AS o \
                   ORDER BY o.object_id; PRINT 'after';";
        let r = live(
            &p,
            &db,
            &["--max-output", "4096", "--format", "json", "-Q", sql],
        );
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(r.stdout.len() <= 4096, "{} bytes", r.stdout.len());
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        assert_eq!(doc["contract_version"], contract_version());
        assert_eq!(doc["connection"]["database"], db);
        assert!(doc["connect_ms"].is_u64());
        assert_eq!(doc["batches"][0]["events"][0]["truncated"], true);
        assert!(doc["batches"][0]["elapsed_ms"].is_u64(), "{doc}");
        assert_eq!(doc["ok"], true);
    }

    // PLAN.md 2.15 (decision 6, docs/notes/drain-experiment.md). The tests below need a result
    // of millions of narrow rows, far more than a 250 ms count can read, whose full count takes
    // seconds. They used to read StackOverflow2010's `dbo.Votes`; since PLAN.md 3.4 the server
    // makes the rows up: a cross join of the catalog view `sys.all_columns` (thousands of rows
    // on any server, so millions of pairs), which streams as fast as a table scan, stores
    // nothing, and needs no database beyond the scratch one.

    const SO: &str = "sqlh_test";

    /// `n` narrow rows (`Id:int`), made up by the server. The values are 7 digits wide, like the
    /// ids of the table this replaced, so the 1 MB output budget holds about as many rows
    /// (about 125,000; a one-digit column filled it with half a million, which a debug build
    /// can't format within `-t 1` under load).
    fn votes(n: u32) -> String {
        format!(
            "SELECT TOP ({n}) 1000000 + a.column_id * 1000 + b.column_id AS Id \
             FROM sys.all_columns AS a \
             CROSS JOIN sys.all_columns AS b"
        )
    }

    /// The lower bound in a stopped count's footer: `-- 100 of ≥800000 rows shown (...)`.
    fn bound(text: &str) -> u64 {
        let footer = text
            .lines()
            .find(|l| l.contains("rows shown") && l.contains("counting stopped"))
            .unwrap_or_else(|| panic!("no stopped footer in {text}"));
        let digits: String = footer
            .split('≥')
            .nth(1)
            .unwrap_or_else(|| panic!("no bound in {footer}"))
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().expect("a number")
    }

    /// One significant digit: a digit followed by zeros.
    fn one_digit(n: u64) -> bool {
        let s = n.to_string();
        s[1..].bytes().all(|b| b == b'0')
    }

    // A large result is cut after about 250 ms of counting, in a second or so rather than
    // the seconds the whole count takes. The footer, the note and the status line say so, the
    // bound has one significant digit, and the batch after it runs on the same session (the
    // read-only wrapper transaction survived the cancel).
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_large_result_is_cut_within_about_a_second() {
        let (p, _) = target();
        let sql = format!(
            "{}; PRINT 'after'\nGO\nSELECT @@TRANCOUNT AS open_trans",
            votes(5_000_000)
        );
        let started = Instant::now();
        let r = live(&p, SO, &["-Q", &sql]);
        let elapsed = started.elapsed();
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            elapsed < Duration::from_secs(6),
            "{elapsed:?}\n{}",
            r.stdout
        );
        let n = bound(&r.stdout);
        assert!(
            (100..=5_000_000).contains(&n) && one_digit(n),
            "{n}\n{}",
            r.stdout
        );
        assert!(
            r.stdout.contains(&format!(
                "-- 100 of ≥{n} rows shown (--max-rows 100; counting stopped)\n"
            )),
            "{}",
            r.stdout
        );
        assert!(
            r.stdout.contains(
                "-- note: counting stopped in batch 1: the rest of that batch was cancelled, \
                 so its later statements did not run; --exact-count counts every row\n"
            ),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("-- msg: after"), "{}", r.stdout);
        // The next batch ran, in the transaction the wrapper opened.
        assert!(r.stdout.contains("open_trans:int\n1\n"), "{}", r.stdout);
        let status = last_line(&r.stdout);
        assert!(
            status.starts_with("-- ok: 2 batches, counting stopped in 1, "),
            "{status}"
        );

        let r = live(&p, SO, &["--format", "json", "-Q", &votes(5_000_000)]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["row_count_exact"], false, "{doc}");
        assert!(one_digit(result["row_count"].as_u64().unwrap()), "{doc}");
        assert_eq!(result["truncated"], true);
        assert_eq!(doc["batches"][0]["counting_stopped"], true, "{doc}");
        assert!(
            doc["notes"][0]
                .as_str()
                .unwrap()
                .starts_with("counting stopped in batch 1: "),
            "{doc}"
        );
        assert!(
            doc["summary"]
                .as_str()
                .unwrap()
                .starts_with("ok: 1 batch, counting stopped in 1, "),
            "{doc}"
        );

        let r = live(&p, SO, &["--format", "csv", "-Q", &votes(5_000_000)]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(bound(&r.stderr) >= 100, "{}", r.stderr);
        assert!(
            r.stderr.contains("; counting stopped)\n")
                && r.stderr.contains("-- note: counting stopped"),
            "{}",
            r.stderr
        );
        assert!(
            last_line(&r.stderr).starts_with("-- ok: 1 batch, counting stopped in 1, "),
            "{}",
            r.stderr
        );
    }

    // A result that ends inside the budget stays exact, with the statements after it run, and
    // says nothing about counting. The same query gives the same output.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_result_inside_the_budget_stays_exact() {
        let (p, _) = target();
        let sql = format!("{}; PRINT 'after'", votes(5_000));
        let first = live(&p, SO, &["-Q", &sql]);
        assert_eq!(first.code, 0, "{}{}", first.stdout, first.stderr);
        assert!(
            first
                .stdout
                .contains("-- 100 of 5000 rows shown (--max-rows 100)\n-- msg: after\n"),
            "{}",
            first.stdout
        );
        assert!(
            !first.stdout.contains("counting stopped"),
            "{}",
            first.stdout
        );
        let doc = live(&p, SO, &["--format", "json", "-Q", &sql]);
        let doc: serde_json::Value = serde_json::from_str(&doc.stdout).expect("valid JSON");
        assert_eq!(doc["batches"][0]["events"][0]["row_count"], 5000, "{doc}");
        assert_eq!(doc["batches"][0]["events"][0]["row_count_exact"], true);
        assert_eq!(doc["batches"][0]["counting_stopped"], false);
    }

    // --exact-count, and input that says XACT_ABORT, count every row. Three million rows are
    // more than 250 ms reads, so a bounded run would stop.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn exact_counting_on_request_and_for_xact_abort() {
        let (p, _) = target();
        let exact = "-- 100 of 3000000 rows shown (--max-rows 100)\n-- msg: after\n";
        let sql = format!("{}; PRINT 'after'", votes(3_000_000));
        let r = live(&p, SO, &["--exact-count", "-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(exact), "{}", r.stdout);
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);

        let sql = format!("SET XACT_ABORT ON\nGO\n{}; PRINT 'after'", votes(3_000_000));
        let r = live(&p, SO, &["-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(exact), "{}", r.stdout);
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
    }

    // With writes allowed, counting is exact: a stopped count would skip the COMMIT. The row
    // written before the big result is committed, and visible to a new session.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn allow_write_counts_exactly_and_the_commit_runs() {
        let (p, db) = target();
        let probe = Probe::create(&p, &db);
        let t = &probe.name;
        let sql = format!(
            "BEGIN TRAN; INSERT {t} VALUES (7); {}; COMMIT",
            votes(3_000_000)
        );
        let r = live(&p, &db, &["--allow-write", "-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("-- 1 row affected\n"), "{}", r.stdout);
        assert!(
            r.stdout
                .contains("-- 100 of 3000000 rows shown (--max-rows 100)\n"),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
        let seen = live(&p, &db, &["-Q", &format!("SELECT COUNT(*) AS n FROM {t}")]);
        assert!(
            seen.stdout.contains("n:int\n1\n"),
            "the commit was lost: {}",
            seen.stdout
        );
    }

    // `GO n`: each repeat is its own batch with its own count; the note names them.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_repeated_batch_stops_counting_in_each_run() {
        let (p, _) = target();
        let sql = format!("{}; PRINT 'after'\nGO 3", votes(5_000_000));
        let r = live(&p, SO, &["-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert_eq!(
            r.stdout.matches("; counting stopped)").count(),
            3,
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("-- msg: after"), "{}", r.stdout);
        assert!(
            r.stdout.contains(
                "-- note: counting stopped in batches 1, 2, 3: the rest of each was cancelled"
            ),
            "{}",
            r.stdout
        );
        assert!(
            last_line(&r.stdout).starts_with("-- ok: 3 batches, counting stopped in 3, "),
            "{}",
            r.stdout
        );
    }

    // Rows the --max-output budget has no room for are hidden rows too: they are counted for a
    // bounded time, and the footer names the budget as the limit. The run stays within the cap.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn rows_hidden_by_max_output_are_counted_for_a_bounded_time() {
        let (p, _) = target();
        let r = live(
            &p,
            SO,
            &[
                "--max-rows",
                "0",
                "--max-output",
                "4096",
                "-Q",
                &votes(5_000_000),
            ],
        );
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.len() <= 4096, "{} bytes", r.stdout.len());
        assert!(
            r.stdout
                .contains(" rows shown (--max-output; counting stopped)\n"),
            "{}",
            r.stdout
        );
        assert!(
            last_line(&r.stdout).starts_with("-- ok: 1 batch, counting stopped in 1, "),
            "{}",
            r.stdout
        );
        let r = live(
            &p,
            SO,
            &[
                "--max-rows",
                "0",
                "--max-output",
                "4096",
                "--format",
                "json",
                "-Q",
                &votes(5_000_000),
            ],
        );
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.len() <= 4096, "{} bytes", r.stdout.len());
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        assert_eq!(
            doc["batches"][0]["events"][0]["row_count_exact"], false,
            "{doc}"
        );
        assert_eq!(doc["batches"][0]["counting_stopped"], true, "{doc}");
    }

    /// The server answers with 20,000 rows at once and then searches 15 billion combinations
    /// for a match that does not exist: a wait with no row coming, for the count to end or for
    /// `-t` to. (`-t` is sqlh's own deadline for the batch since R15, so it would also expire
    /// for a result that is read without any wait; a stall is what makes the wait certain.)
    const STALL_AFTER_20000_ROWS: &str = "SELECT TOP (20000) 1 AS n FROM sys.all_objects AS x \
        CROSS JOIN sys.all_objects AS y UNION ALL SELECT 2 FROM sys.all_objects AS a \
        CROSS JOIN sys.all_objects AS b CROSS JOIN sys.all_objects AS c \
        WHERE CONVERT(bigint, a.object_id) + b.object_id + c.object_id = -1";

    /// 154 rows of 400 characters (about 60 KB, so the server sends them before it stalls), and
    /// then the same stall as above.
    const STALL_AFTER_154_WIDE_ROWS: &str = "SELECT TOP (154) REPLICATE('x', 400) AS s \
        FROM sys.all_objects AS x CROSS JOIN sys.all_objects AS y UNION ALL SELECT 'y' \
        FROM sys.all_objects AS a CROSS JOIN sys.all_objects AS b \
        CROSS JOIN sys.all_objects AS c \
        WHERE CONVERT(bigint, a.object_id) + b.object_id + c.object_id = -1";

    // A count stopped after only a few hidden rows must not read as if nothing was hidden:
    // 154 rows counted floor to 100 with one significant digit, which is the 100 rows shown. The
    // bound is one more than the rows shown, and the result is `truncated`. (The server sent at
    // least 101 of the 154 rows before it stalled, and fewer than 200; the bound is 101 for all
    // of them.)
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_stop_after_a_few_hidden_rows_still_says_that_rows_were_hidden() {
        let (p, db) = target();
        let args = [
            "--max-rows",
            "100",
            "--max-cell",
            "5",
            "-t",
            "10",
            "-Q",
            STALL_AFTER_154_WIDE_ROWS,
        ];
        let r = live(&p, &db, &args);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout.contains(
                "-- 100 of ≥101 rows shown (--max-rows 100; counting stopped); 100 values cut \
                 (--max-cell 5)\n"
            ),
            "{}",
            r.stdout
        );
        let mut json_args = vec!["--format", "json"];
        json_args.extend(args);
        let r = live(&p, &db, &json_args);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["rows"].as_array().unwrap().len(), 100, "{doc}");
        assert_eq!(result["row_count"], 101, "{doc}");
        assert_eq!(result["row_count_exact"], false, "{doc}");
        assert_eq!(result["truncated"], true, "{doc}");
        assert_eq!(doc["batches"][0]["counting_stopped"], true, "{doc}");
    }

    // `-t` expiring while only hidden rows are counted is no failure of a read-only run: the
    // rows shown are complete, so the run reports a lower bound and ends with its own exit
    // code, and the note advises raising `-t` (not `--exact-count`, which is already on).
    // With writes allowed it stays exit 4, because the stop would skip what follows. The
    // bounded count ends such a wait on its own, after 250 ms, and then advises
    // `--exact-count`.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_timeout_during_the_count_reports_a_bound_not_exit_4() {
        let (p, db) = target();
        let sql = STALL_AFTER_20000_ROWS;
        let stopped = " rows shown (--max-rows 5; counting stopped)\n";
        let started = Instant::now();
        let args = ["--exact-count", "-t", "1", "--max-rows", "5", "-Q", sql];
        let r = live(&p, &db, &args);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        assert!(r.stdout.contains(stopped), "{}", r.stdout);
        assert!([10_000, 20_000].contains(&bound(&r.stdout)), "{}", r.stdout);
        assert!(
            r.stdout.contains(
                "-- note: counting stopped in batch 1: the rest of that batch was cancelled, \
                 so its later statements did not run; raise -t to count longer\n"
            ),
            "{}",
            r.stdout
        );
        assert!(
            last_line(&r.stdout).starts_with("-- ok: 1 batch, counting stopped in 1, "),
            "{}",
            r.stdout
        );

        let started = Instant::now();
        let r = live(
            &p,
            &db,
            &["--allow-write", "-t", "1", "--max-rows", "5", "-Q", sql],
        );
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);

        // Without --exact-count the 250 ms budget is what ends the wait, long before -t.
        let started = Instant::now();
        let r = live(&p, &db, &["--max-rows", "5", "-Q", sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        assert!(r.stdout.contains(stopped), "{}", r.stdout);
        assert!([10_000, 20_000].contains(&bound(&r.stdout)), "{}", r.stdout);
        assert!(
            r.stdout
                .contains("so its later statements did not run; --exact-count counts every row\n"),
            "{}",
            r.stdout
        );

        // The same stop in JSON: a bound is not an exact count, the batch says it was stopped,
        // and `truncated` follows the rows that were counted past the ones shown.
        let r = live(&p, &db, &["--format", "json", "--max-rows", "5", "-Q", sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["row_count_exact"], false, "{doc}");
        assert_eq!(result["truncated"], true, "{doc}");
        assert!(
            [10_000, 20_000].contains(&result["row_count"].as_u64().unwrap()),
            "{doc}"
        );
        assert_eq!(doc["batches"][0]["counting_stopped"], true, "{doc}");
    }

    // The other side of the `-t` rule: it is the rows shown being complete that makes the stop
    // no failure. A wait for a row that would be shown (here `--max-rows 0`, which shows every
    // row) is still exit 4, with the rows that arrived and no stopped count. So is a wait with
    // XACT_ABORT in the input: the attention that stops a count would roll the read-only
    // wrapper transaction back, which ends the run with exit 1 (sqlh's own invariant) and hides
    // the timeout, so that run counts exactly and `-t` is a failure as it always was.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_timeout_that_is_not_in_hidden_rows_is_still_exit_4() {
        let (p, db) = target();
        let with_xact_abort = format!("SET XACT_ABORT ON\nGO\n{STALL_AFTER_20000_ROWS}");
        // What the run is called, its flags, its input and the batch that times out.
        let cases: [(&str, &[&str], &str, u32); 3] = [
            // Every row is shown, so the wait is for a row that would be shown.
            (
                "max-rows 0",
                &["--max-rows", "0"],
                STALL_AFTER_20000_ROWS,
                1,
            ),
            // Rows 6 and on are hidden, but the attention would roll the wrapper back.
            ("XACT_ABORT", &["--max-rows", "5"], &with_xact_abort, 2),
            // Both at once.
            ("both", &["--max-rows", "0"], &with_xact_abort, 2),
        ];
        for (what, flags, sql, batch) in cases {
            let started = Instant::now();
            let r = live(&p, &db, &[flags, &["-t", "2", "-Q", sql]].concat());
            assert_eq!(r.code, 4, "{what}: {}{}", r.stdout, r.stderr);
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "{what}: {:?}",
                started.elapsed()
            );
            let timeout = format!(
                "-- error: query timeout after 2 s in batch {batch}; raise -t or narrow the query\n"
            );
            assert!(r.stdout.contains(&timeout), "{what}: {}", r.stdout);
            assert!(
                !r.stdout.contains("counting stopped"),
                "{what}: {}",
                r.stdout
            );
            assert!(
                last_line(&r.stdout).starts_with("-- failed (exit 4): timeout, "),
                "{what}: {}",
                r.stdout
            );
        }
        // In JSON the result that was cut short is not exact, and says it was interrupted.
        let r = live(
            &p,
            &db,
            &[
                "--format",
                "json",
                "--max-rows",
                "0",
                "-t",
                "2",
                "-Q",
                STALL_AFTER_20000_ROWS,
            ],
        );
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["interrupted"], true, "{doc}");
        assert_eq!(result["row_count_exact"], false, "{doc}");
        assert_eq!(doc["batches"][0]["counting_stopped"], false, "{doc}");
    }

    // Ctrl+Break while a stopped count is being cancelled ends the run as a cancel, exit 6,
    // not as a stopped count (DESIGN.md §4.4, §6): the user's cancel wins, though the budget's
    // own cancel got there first.
    //
    // A debug build can hold the run between the moment the budget's cancel has been
    // acknowledged and the moment the run decides whose cancel it was
    // (SQLH_DEBUG_STOP_HOLD_MS), and it says when the hold begins (a line on stderr), which
    // makes the moment certain. The break lands in the middle of a 2 s hold, which then ends
    // 1 s later, inside the 2 s that a break gives the work. The run therefore ends through the
    // real decision, and says so: `the statement was cancelled; the connection stayed usable`.
    // (A break that landed after the grace would end the run by the grace, with `nothing
    // confirmed the cancel`, without reaching that decision, and the test could not tell
    // whether the user's cancel is honored. A build without debug assertions has no hold, so
    // the test exists only in a build that has one.)
    #[cfg(all(windows, debug_assertions))]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn ctrl_break_while_a_count_is_being_cancelled_is_exit_6() {
        use std::io::BufRead;
        let (p, _) = target();
        let mut child = spawn_in_group(
            Command::new(env!("CARGO_BIN_EXE_sqlh"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("SQLH_DEBUG_STOP_HOLD_MS", "2000")
                .args(["--profile", &p, "-d", SO, "-Q", &votes(5_000_000)]),
        );
        // The hold begins when the count has been stopped and the cancel acknowledged.
        let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        while !line.starts_with("sqlh: debug: holding 2000 ms") {
            line.clear();
            let read = stderr.read_line(&mut line).unwrap();
            assert_ne!(read, 0, "sqlh ended before it stopped counting");
        }
        std::thread::sleep(Duration::from_millis(1000));
        let started = Instant::now();
        ctrl_break(&child);
        let status = wait_within(&mut child, Duration::from_secs(10), "Ctrl+Break");
        let elapsed = started.elapsed();
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let mut rest = String::new();
        stderr.read_to_string(&mut rest).unwrap();
        assert_eq!(status.code(), Some(6), "{stdout}{rest}");
        assert!(
            stdout.contains(
                "-- error: cancelled by Ctrl+Break: the statement was cancelled; the connection \
                 stayed usable\n"
            ),
            "{stdout}"
        );
        assert!(!stdout.contains("nothing confirmed"), "{stdout}");
        assert!(!stdout.contains("counting stopped"), "{stdout}");
        assert!(
            last_line(&stdout).starts_with("-- failed (exit 6): cancelled, "),
            "{stdout}"
        );
        // The hold had a second left, and the closing lines get another.
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    // R14. A session that starts with XACT_ABORT ON, as a server's `user options` setting
    // would make every session, without the word in the user's SQL and without touching the
    // server: a debug build sends `SQLH_DEBUG_SESSION_PREFIX` ahead of the session setup, in
    // the same batch. The setup's `SET XACT_ABORT OFF` has to undo it before the wrapper
    // transaction begins; otherwise the attention that stops a bounded count rolls the wrapper
    // back, the next batch fails sqlh's invariant, and a last batch ends in a success with the
    // wrapper gone. (Checked to fail with the OFF taken out of the setup: exit 1, `read-only
    // guard: an error rolled back the wrapper transaction`, for the first run below.)

    /// Like `live`, with environment variables set for the run.
    fn live_with(profile: &str, database: &str, env: &[(&str, &str)], args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
        command
            .stdin(Stdio::null())
            .args(["--profile", profile, "-d", database])
            .args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        run(&mut command)
    }

    #[cfg(debug_assertions)]
    const XACT_ABORT_ON: [(&str, &str); 1] = [("SQLH_DEBUG_SESSION_PREFIX", "SET XACT_ABORT ON;")];

    #[cfg(debug_assertions)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_session_setup_turns_xact_abort_off() {
        let (p, db) = target();
        let probe = "SELECT CASE WHEN (@@OPTIONS & 16384) = 16384 THEN 1 ELSE 0 END AS xact_abort";
        // The prefix alone would leave it ON; the setup after it decides, in both kinds of run.
        for flags in [&[][..], &["--allow-write"][..]] {
            let r = live_with(&p, &db, &XACT_ABORT_ON, &[flags, &["-Q", probe]].concat());
            assert_eq!(r.code, 0, "{flags:?}: {}{}", r.stdout, r.stderr);
            assert!(
                r.stdout.contains("xact_abort:int\n0\n"),
                "{flags:?}: {}",
                r.stdout
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_stopped_count_keeps_the_wrapper_in_a_session_that_started_with_xact_abort_on() {
        let (p, _) = target();
        // A bounded stop (5 million rows are more than 250 ms reads), then a batch that runs.
        let sql = format!("{}\nGO\nSELECT @@TRANCOUNT AS open_trans", votes(5_000_000));
        let r = live_with(&p, SO, &XACT_ABORT_ON, &["-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("counting stopped)\n"), "{}", r.stdout);
        assert!(r.stdout.contains("open_trans:int\n1\n"), "{}", r.stdout);
        assert!(
            last_line(&r.stdout).starts_with("-- ok: 2 batches, counting stopped in 1, "),
            "{}",
            r.stdout
        );
        // The stopped count is the last batch: the wrapper is intact at the end, and the run is
        // a success because of it, not in spite of its absence.
        let r = live_with(&p, SO, &XACT_ABORT_ON, &["-Q", &votes(5_000_000)]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("counting stopped)\n"), "{}", r.stdout);
        assert!(!r.stdout.contains("read-only guard"), "{}", r.stdout);
        // The same in JSON, which says the batch stopped and the run is ok.
        let json = ["--format", "json", "-Q", &votes(5_000_000)];
        let r = live_with(&p, SO, &XACT_ABORT_ON, &json);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        assert_eq!(doc["ok"], true, "{doc}");
        assert_eq!(doc["batches"][0]["counting_stopped"], true, "{doc}");
    }

    // The explicit-input cases are unchanged: XACT_ABORT in the SQL still counts exactly (and
    // `-t` over it is still exit 4: `a_timeout_that_is_not_in_hidden_rows_is_still_exit_4`),
    // also in a session that started with it ON.
    #[cfg(debug_assertions)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn xact_abort_in_the_input_still_counts_exactly_in_such_a_session() {
        let (p, _) = target();
        let sql = format!("SET XACT_ABORT ON\nGO\n{}; PRINT 'after'", votes(3_000_000));
        let r = live_with(&p, SO, &XACT_ABORT_ON, &["-Q", &sql]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout
                .contains("-- 100 of 3000000 rows shown (--max-rows 100)\n-- msg: after\n"),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
    }

    // R15: `-t` is sqlh's own elapsed deadline for a batch, from when it is sent to when its
    // last result is read, whatever the batch is doing: reading rows that are already there,
    // formatting them, waiting for the reader. The driver's own `-t` adds up only the time it
    // waits for the server, so a result that the client read as fast as the server sent it ran
    // to the end (`--allow-write -t 1` on 10 million narrow rows: 7.4 s, in
    // docs/notes/drain-experiment.md). After the deadline the statement is cancelled with an
    // attention, which the server acknowledges after the data in flight: tens of milliseconds
    // in a release build, up to 2 s measured in a debug build that can't keep up with the
    // stream. The driver waits for it 5 s at most (DESIGN.md §4.4). These tests allow `-t`
    // plus those 5 s, which a run that reads to the end (7.4 s and up) does not meet; 4 s was
    // exceeded by a debug build with the whole live suite running in parallel.
    const GRACE: Duration = Duration::from_secs(5);

    /// A run with `-t 1` that has to be over within `-t` plus the grace.
    fn timed(p: &str, db: &str, flags: &[&str], sql: &str) -> (Run, Duration) {
        let started = Instant::now();
        let r = live(p, db, &[flags, &["-t", "1", "-Q", sql]].concat());
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1) + GRACE,
            "{flags:?}: {elapsed:?}\n{}{}",
            r.stdout,
            r.stderr
        );
        (r, elapsed)
    }

    const TIMEOUT_1: &str =
        "-- error: query timeout after 1 s in batch 1; raise -t or narrow the query\n";

    // A fast, narrow result: 10 million rows of one int. Each mode ends within the deadline
    // plus the grace, with the exit code and the output its rule gives.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_fast_narrow_result_is_bounded_by_t_in_every_mode() {
        let (p, _) = target();
        let sql = &votes(10_000_000);
        // Writes allowed: exact, and a failure when the time is up (a stop could skip a COMMIT).
        let (r, _) = timed(&p, SO, &["--allow-write"], sql);
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(TIMEOUT_1), "{}", r.stdout);
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
        assert!(
            last_line(&r.stdout).starts_with("-- failed (exit 4): timeout, "),
            "{}",
            r.stdout
        );
        // --exact-count in a read-only run: only hidden rows were being read, the rows shown
        // are complete, so the count stops with the run's own exit code (owner decision 6).
        let (r, _) = timed(&p, SO, &["--exact-count"], sql);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        let n = bound(&r.stdout);
        assert!(n >= 101, "{n}\n{}", r.stdout);
        assert!(
            r.stdout.contains(&format!(
                "-- 100 of ≥{n} rows shown (--max-rows 100; counting stopped)\n"
            )),
            "{}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains("so its later statements did not run; raise -t to count longer\n"),
            "{}",
            r.stdout
        );
        assert!(
            last_line(&r.stdout).starts_with("-- ok: 1 batch, counting stopped in 1, "),
            "{}",
            r.stdout
        );
        // Every row shown, no output limit: the rows being produced are rows to show, so it is
        // a failure. (The default read-only mode, with its 250 ms count, has nothing hidden.)
        let (r, _) = timed(&p, SO, &["--max-rows", "0", "--max-output", "0"], sql);
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(TIMEOUT_1), "{}", r.stdout);
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
        assert!(
            last_line(&r.stdout).starts_with("-- failed (exit 4): timeout, "),
            "{}",
            r.stdout
        );
        // The same in JSON: the result that was cut is not exact, and says so.
        let json = ["--format", "json", "--max-rows", "0", "--max-output", "0"];
        let (r, _) = timed(&p, SO, &json, sql);
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(result["interrupted"], true, "{doc}");
        assert_eq!(result["row_count_exact"], false, "{doc}");
        // --max-rows 0 with the default output budget: the rows past the budget are hidden,
        // and the 250 ms count ends the run long before -t.
        let (r, elapsed) = timed(&p, SO, &["--max-rows", "0"], sql);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout
                .contains("rows shown (--max-output; counting stopped)\n"),
            "{}",
            r.stdout
        );
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
        // The ordinary bounded mode: the budget, not -t, ends it.
        let (r, elapsed) = timed(&p, SO, &[], sql);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(
            r.stdout
                .contains("so its later statements did not run; --exact-count counts every row\n"),
            "{}",
            r.stdout
        );
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    // A single large value that is shown (cut to --max-cell as it arrives), 256 MB built on
    // the server. The value is a row to show, so the deadline is a failure, in a read-only run
    // and with writes allowed alike, and the value is not read to its end.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_large_visible_value_is_bounded_by_t() {
        let (p, db) = target();
        // Doubled in steps, not one REPLICATE: a step ends in well under a second, where one
        // call for 300 MB cannot be interrupted, and then the server would take its time to
        // answer the attention (the driver waits for it 5 s) however quickly sqlh gives it.
        let sql = "DECLARE @v varchar(max) = REPLICATE(CAST('x' AS varchar(max)), 4000000); \
                   SET @v += @v; SET @v += @v; SET @v += @v; SET @v += @v; SET @v += @v; \
                   SET @v += @v; SELECT @v AS v";
        for flags in [
            &["--max-cell", "10"][..],
            &["--max-cell", "10", "--allow-write"][..],
            &["--max-cell", "10", "--exact-count"][..],
        ] {
            let (r, _) = timed(&p, &db, flags, sql);
            assert_eq!(r.code, 4, "{flags:?}: {}{}", r.stdout, r.stderr);
            assert!(r.stdout.contains(TIMEOUT_1), "{flags:?}: {}", r.stdout);
            assert!(
                last_line(&r.stdout).starts_with("-- failed (exit 4): timeout, "),
                "{flags:?}: {}",
                r.stdout
            );
        }
    }

    // Many small result sets: a loop that selects a row per iteration. The count budget starts
    // again with each result, so no result is long, and `--max-results` stops showing them
    // after the first few; only the deadline bounds the batch. With writes allowed it is exit
    // 4. In a read-only run the deadline can land while one of the hidden rows is being read,
    // which is the owner's exception (exit 0, a stopped count), or between results, which is a
    // failure (exit 4): either ends the run inside the bound.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn many_small_results_are_bounded_by_t() {
        let (p, db) = target();
        let sql = "DECLARE @i int = 0; WHILE @i < 20000000 BEGIN SELECT @i AS i; SET @i += 1; END";
        let (r, _) = timed(&p, &db, &["--allow-write"], sql);
        assert_eq!(r.code, 4, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains(TIMEOUT_1), "{}", r.stdout);
        assert!(
            last_line(&r.stdout).starts_with("-- failed (exit 4): timeout, "),
            "{}",
            r.stdout
        );
        let (r, _) = timed(&p, &db, &[], sql);
        match r.code {
            4 => assert!(r.stdout.contains(TIMEOUT_1), "{}", r.stdout),
            0 => assert!(
                last_line(&r.stdout).starts_with("-- ok: 1 batch, counting stopped in 1, "),
                "{}",
                r.stdout
            ),
            code => panic!("exit {code}: {}{}", r.stdout, r.stderr),
        }
    }

    // R15, through the real loops. In a debug build `SQLH_DEBUG_ROW_WORK_US` makes `count_rows`
    // and `large_value` do that much work of their own per row and per chunk, after the read,
    // so the stream is ready at once but the batch takes far longer than `-t`: the case the
    // driver's clock (waiting time only) never sees. Taking the `pace.tick` calls out of
    // `count_rows` makes the first test last as long as the work (8 s), where the deadline ends
    // it within `-t` plus the grace.
    #[cfg(debug_assertions)]
    fn timed_with_work(p: &str, db: &str, work_us: &str, flags: &[&str], sql: &str) -> Run {
        let started = Instant::now();
        let r = live_with(
            p,
            db,
            &[("SQLH_DEBUG_ROW_WORK_US", work_us)],
            &[flags, &["-t", "1", "-Q", sql]].concat(),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1) + GRACE,
            "{flags:?}: {elapsed:?}\n{}{}",
            r.stdout,
            r.stderr
        );
        r
    }

    #[cfg(debug_assertions)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_row_loop_keeps_the_deadline_when_the_work_is_sqlhs_own() {
        let (p, _) = target();
        // 20,000 rows of 400 us is 8 s of work against a 1 s deadline: a run that ignores the
        // deadline fails the bound in 8 s, where 10 million rows would take over an hour.
        let sql = &votes(20_000);
        // Shown rows, and writes allowed: a failure.
        for flags in [
            &["--allow-write"][..],
            &["--max-rows", "0", "--max-output", "0"][..],
        ] {
            let r = timed_with_work(&p, SO, "400", flags, sql);
            assert_eq!(r.code, 4, "{flags:?}: {}{}", r.stdout, r.stderr);
            assert!(r.stdout.contains(TIMEOUT_1), "{flags:?}: {}", r.stdout);
        }
        // Hidden rows only, counted exactly in a read-only run: the owner's exception.
        let r = timed_with_work(&p, SO, "400", &["--exact-count"], sql);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("counting stopped"), "{}", r.stdout);
    }

    // Not mutation-sensitive, as measured: with the `pace.tick` taken out of `large_value` this
    // still passes, because every 32 KB chunk takes several socket reads and tokio's own budget
    // for those hands the runtime back often enough for the timer to run. It pins the outcome
    // (exit 4, the message, within the bound); the tick itself is pinned by the `Pace` unit tests
    // in `src/db/mod.rs`, and the row loop by the test above (mutation-checked: it fails, with
    // the run lasting its 8 s of work, when the ticks come out of `count_rows`).
    #[cfg(debug_assertions)]
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_chunk_loop_keeps_the_deadline_when_the_work_is_sqlhs_own() {
        let (p, db) = target();
        // 16 MB, built in two steps on the server (well under a second), read in 32 KB chunks:
        // 512 chunks of 5 ms of work is 2.5 s against a 1 s deadline.
        let sql = "DECLARE @v varchar(max) = REPLICATE(CAST('x' AS varchar(max)), 4000000); \
                   SET @v += @v; SET @v += @v; SELECT @v AS v";
        for flags in [
            &["--max-cell", "10"][..],
            &["--max-cell", "10", "--allow-write"][..],
        ] {
            let r = timed_with_work(&p, &db, "5000", flags, sql);
            assert_eq!(r.code, 4, "{flags:?}: {}{}", r.stdout, r.stderr);
            assert!(r.stdout.contains(TIMEOUT_1), "{flags:?}: {}", r.stdout);
        }
    }

    // PLAN.md 3.4: tests/smoke.sql against the server, compared whole with the expected output
    // in tests/golden/: every data type's rendering, escapes, cut values, the row limit, temp
    // tables in read-only mode, `GO n`, messages, a result an error cut short, errors mapped to
    // their lines and the status line. Only what changes from run to run is normalized: the
    // times, the server's name and version, and the database's name. After a deliberate change
    // to the output, rerun with SQLH_BLESS=1 to rewrite the golden files, and review the diff.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_smoke_script_matches_its_golden_output() {
        let (p, db) = target();
        let smoke = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/smoke.sql");
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
        let bless = std::env::var_os("SQLH_BLESS").is_some();

        let r = live(&p, &db, &["-i", smoke, "--continue-on-error"]);
        assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
        let text = normalize_status(&r.stdout, &db);
        let r = live(
            &p,
            &db,
            &["-i", smoke, "--continue-on-error", "--format", "json"],
        );
        assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
        let mut doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("valid JSON");
        normalize_json(&mut doc, &db);
        let json = serde_json::to_string_pretty(&doc).unwrap() + "\n";

        for (name, actual) in [("smoke.txt", &text), ("smoke.json", &json)] {
            let path = golden.join(name);
            if bless {
                fs::create_dir_all(&golden).unwrap();
                fs::write(&path, actual).unwrap();
                continue;
            }
            let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "{}: {e}; run with SQLH_BLESS=1 to create it",
                    path.display()
                )
            });
            // A checkout may have turned the golden file's line ends into CRLF.
            let expected = expected.replace("\r\n", "\n");
            if &expected != actual {
                let line = expected
                    .lines()
                    .zip(actual.lines())
                    .position(|(e, a)| e != a)
                    .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
                panic!(
                    "{name} differs from {} from line {}:\n  expected: {:?}\n  actual:   {:?}\n\
                     (after a deliberate change, rerun with SQLH_BLESS=1 and review the diff)",
                    path.display(),
                    line + 1,
                    expected.lines().nth(line),
                    actual.lines().nth(line)
                );
            }
        }
    }

    /// The text output with the status line's volatile parts replaced: `, 273 ms; db01/sqlh_test,
    /// connect 216 ms` becomes `, N ms; <server>/<database>, connect N ms`.
    fn normalize_status(text: &str, db: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            let line = if line.starts_with("-- ok: ") || line.starts_with("-- failed (") {
                let (status, tail) = line
                    .split_once("; ")
                    .map_or((line, None), |(s, t)| (s, Some(t)));
                let tail_db = format!("/{db}, connect ");
                let tail = tail.map(|t| {
                    assert!(t.contains(&tail_db), "{t}");
                    "<server>/<database>, connect N ms".to_owned()
                });
                let status = match status.rsplit_once(", ") {
                    Some((head, ms)) if ms.ends_with(" ms") => format!("{head}, N ms"),
                    _ => status.to_owned(),
                };
                match tail {
                    Some(tail) => format!("{status}; {tail}"),
                    None => status,
                }
            } else {
                line.to_owned()
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// The JSON document with its volatile values replaced: times, and the connection's
    /// server, version and database.
    fn normalize_json(doc: &mut serde_json::Value, db: &str) {
        use serde_json::Value;
        fn walk(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    for (key, v) in map.iter_mut() {
                        if key.ends_with("_ms") && v.is_u64() {
                            *v = Value::from("N");
                        } else {
                            walk(v);
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(walk),
                _ => {}
            }
        }
        walk(doc);
        let connection = &mut doc["connection"];
        assert_eq!(connection["database"], db, "{connection}");
        connection["database"] = Value::from("<database>");
        connection["server"] = Value::from("<server>");
        connection["server_version"] = Value::from("<version>");
        if let Some(summary) = doc["summary"].as_str() {
            let summary = match summary.rsplit_once(", ") {
                Some((head, ms)) if ms.ends_with(" ms") => format!("{head}, N ms"),
                _ => summary.to_owned(),
            };
            doc["summary"] = Value::from(summary);
        }
    }

    /// Objects for one test, made after `Probe` has checked the scratch marker, each named with
    /// the probe's unique name as a prefix; dropped in reverse order when this goes out of
    /// scope, whether the test passed or not (PLAN.md 3.4).
    struct Objects {
        probe: Probe,
        /// `DROP ...` statements, in the order the objects were made.
        drops: Vec<String>,
    }

    impl Objects {
        fn new(profile: &str, database: &str) -> Objects {
            Objects {
                probe: Probe::create(profile, database),
                drops: Vec::new(),
            }
        }

        /// A name for a new object: the probe's, plus `suffix`.
        fn name(&self, suffix: &str) -> String {
            format!("{}_{suffix}", self.probe.name)
        }

        /// Runs `create` with writes allowed, and remembers `drop` for the end.
        fn make(&mut self, create: &str, drop: String) {
            let r = live(
                &self.probe.profile,
                &self.probe.database,
                &["--allow-write", "-Q", create],
            );
            assert_eq!(r.code, 0, "{create}\n{}", r.stdout);
            self.drops.push(drop);
        }
    }

    impl Drop for Objects {
        fn drop(&mut self) {
            for sql in self.drops.iter().rev() {
                let _ = live(
                    &self.probe.profile,
                    &self.probe.database,
                    &["--allow-write", "-Q", sql],
                );
            }
        }
    }

    // F03 (PLAN.md 1.5), end to end: every shape the review got past the guard is refused by
    // the real binary before anything runs, and a sequence it would have advanced is where it
    // was. (Unit tests in src/guard.rs cover the classifier; this is the regression against a
    // server.)
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn the_guard_refuses_what_the_review_got_past_it() {
        let (p, db) = target();
        let mut objects = Objects::new(&p, &db);
        let seq = objects.name("seq");
        objects.make(
            &format!("CREATE SEQUENCE {seq} AS bigint START WITH 1"),
            format!("DROP SEQUENCE {seq}"),
        );
        let t = objects.probe.name.clone();
        for sql in [
            format!("SELECT NEXT VALUE FOR {seq} AS n"),
            format!("EXEC sys.sp_spaceused N'{t}', N'true'"),
            "EXEC Lnk.master.sys.sp_who".to_owned(),
            "EXEC sales.sp_who".to_owned(),
            "INSERT ##shared_sqlh VALUES (1)".to_owned(),
        ] {
            let r = live(&p, &db, &["-Q", &sql]);
            assert_eq!(r.code, 5, "{sql}\n{}", r.stdout);
            assert!(
                r.stdout.contains("; nothing was run. "),
                "{sql}\n{}",
                r.stdout
            );
        }
        let current = live(
            &p,
            &db,
            &[
                "-Q",
                &format!(
                    "SELECT CAST(current_value AS bigint) AS v FROM sys.sequences \
                     WHERE object_id = OBJECT_ID(N'{seq}')"
                ),
            ],
        );
        assert!(
            current.stdout.contains("v:bigint\n1\n"),
            "{}",
            current.stdout
        );
    }

    // PLAN.md 3.4: a write reaches a table with a filtered index (which refuses writes from a
    // session whose SET options are wrong, error 1934) and fires its trigger; a foreign key's
    // refusal is an ordinary SQL error mapped to its line; without --allow-write none of it
    // runs.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn writes_reach_filtered_indexes_triggers_and_foreign_keys() {
        let (p, db) = target();
        let mut objects = Objects::new(&p, &db);
        let (parent, child, log) = (
            objects.name("parent"),
            objects.name("child"),
            objects.name("log"),
        );
        let (ix, trg) = (objects.name("ix"), objects.name("trg"));
        objects.make(
            &format!("CREATE TABLE {log} (id int NOT NULL)"),
            format!("DROP TABLE {log}"),
        );
        objects.make(
            &format!("CREATE TABLE {parent} (id int NOT NULL PRIMARY KEY, active bit NOT NULL)"),
            format!("DROP TABLE {parent}"),
        );
        objects.make(
            &format!("CREATE TABLE {child} (parent_id int NOT NULL REFERENCES {parent} (id))"),
            format!("DROP TABLE {child}"),
        );
        let ix_name = ix.trim_start_matches("dbo.");
        objects.make(
            &format!("CREATE INDEX {ix_name} ON {parent} (id) WHERE active = 1"),
            format!("DROP INDEX {ix_name} ON {parent}"),
        );
        objects.make(
            &format!(
                "CREATE TRIGGER {trg} ON {parent} AFTER INSERT AS \
                 INSERT {log} (id) SELECT id FROM inserted"
            ),
            format!("DROP TRIGGER {trg}"),
        );

        let insert = format!("INSERT {parent} VALUES (1, 1)");
        let refused = live(&p, &db, &["-Q", &insert]);
        assert_eq!(refused.code, 5, "{}", refused.stdout);

        let written = live(&p, &db, &["--allow-write", "-Q", &insert]);
        assert_eq!(written.code, 0, "{}", written.stdout);
        let logged = live(
            &p,
            &db,
            &["-Q", &format!("SELECT COUNT(*) AS n FROM {log}")],
        );
        assert!(logged.stdout.contains("n:int\n1\n"), "{}", logged.stdout);

        let orphan = format!("PRINT 'first'\nINSERT {child} VALUES (99)");
        let failed = live(&p, &db, &["--allow-write", "-Q", &orphan]);
        assert_eq!(failed.code, 1, "{}", failed.stdout);
        assert!(
            failed
                .stdout
                .contains("-- error 547 (severity 16, state 0, line 2): "),
            "{}",
            failed.stdout
        );
    }

    // The review's open item (Phase 2 completion review): a procedure's large result. Under
    // --allow-write a count is exact, inside a procedure as anywhere, and the statement after
    // the procedure runs.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_procedures_large_result_is_counted_exactly_with_writes_allowed() {
        let (p, db) = target();
        let mut objects = Objects::new(&p, &db);
        let proc = objects.name("rows");
        objects.make(
            &format!("CREATE PROCEDURE {proc} AS {}", votes(300_000)),
            format!("DROP PROCEDURE {proc}"),
        );
        let r = live(
            &p,
            &db,
            &[
                "--allow-write",
                "-Q",
                &format!("EXEC {proc}; PRINT 'after'"),
            ],
        );
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(
            r.stdout
                .contains("-- 100 of 300000 rows shown (--max-rows 100)\n-- msg: after\n"),
            "{}",
            r.stdout
        );
        // Read-only, the guard refuses a procedure of the user's before anything runs.
        let r = live(&p, &db, &["-Q", &format!("EXEC {proc}")]);
        assert_eq!(r.code, 5, "{}", r.stdout);
    }

    // The review's open item: a cursor's FETCHes are result sets of one row each; past
    // --max-results they are hidden and counted, and the run ends normally.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_cursors_fetches_past_max_results_are_counted() {
        let (p, db) = target();
        let sql = format!(
            "DECLARE c CURSOR LOCAL FAST_FORWARD FOR {};\n\
             OPEN c;\n\
             DECLARE @i int = 0;\n\
             WHILE @i < 30 BEGIN FETCH NEXT FROM c; SET @i += 1; END\n\
             CLOSE c; DEALLOCATE c;\n\
             PRINT 'after'",
            votes(1_000)
        );
        let r = live(&p, &db, &["--max-results", "5", "-Q", &sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert_eq!(r.stdout.matches("-- 1 row\n").count(), 5, "{}", r.stdout);
        assert!(r.stdout.contains("-- msg: after\n"), "{}", r.stdout);
        assert!(
            r.stdout.contains(
                "-- note: 25 more result sets not shown (--max-results 5)
"
            ),
            "{}",
            r.stdout
        );
    }

    // The review's open item: SET ROWCOUNT ends a result early on the server's side, so the
    // count is the server's, exact, and the footer says how many rows the limit showed.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn set_rowcount_gives_an_exact_count() {
        let (p, db) = target();
        let sql = format!("SET ROWCOUNT 500;\n{};\nSET ROWCOUNT 0;", votes(5_000_000));
        let r = live(&p, &db, &["--max-rows", "10", "-Q", &sql]);
        assert_eq!(r.code, 0, "{}", r.stdout);
        assert!(
            r.stdout
                .contains("-- 10 of 500 rows shown (--max-rows 10)\n"),
            "{}",
            r.stdout
        );
        assert!(!r.stdout.contains("counting stopped"), "{}", r.stdout);
    }

    /// A low-privilege login for one test (owner decision 5, PLAN.md 3.4): `db_datareader` in
    /// the scratch database and `VIEW DEFINITION`, nothing else. It is made through the test
    /// profile with a random password that never leaves this process and the sqlh processes it
    /// starts: the SQL that sets it reaches sqlh on stdin (never a command line or a file), and
    /// runs as the login get it from `SQLCMDPASSWORD` in their own environment (the test
    /// profile's stored password is for its own login only, so sqlh uses the variable). The
    /// login and its user are dropped when this goes out of scope.
    struct Reader {
        probe: Probe,
        login: String,
        password: String,
    }

    impl Reader {
        fn create(profile: &str, database: &str) -> Reader {
            // The probe checks the scratch marker before anything is written.
            let probe = Probe::create(profile, database);
            let login = format!("sqlh_reader_{}", std::process::id());
            let password = random_password();
            let sql = format!(
                "USE master;\n\
                 CREATE LOGIN [{login}] WITH PASSWORD = N'{password}';\n\
                 USE [{database}];\n\
                 CREATE USER [{login}] FOR LOGIN [{login}];\n\
                 ALTER ROLE db_datareader ADD MEMBER [{login}];\n\
                 GRANT VIEW DEFINITION TO [{login}];"
            );
            let r = Reader::admin(profile, database, &sql);
            assert_eq!(r.code, 0, "creating the reader login failed:\n{}", r.stdout);
            Reader {
                probe,
                login,
                password,
            }
        }

        /// Runs `sql` through the test profile with writes allowed, the SQL on stdin.
        fn admin(profile: &str, database: &str, sql: &str) -> Run {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
            command.args([
                "--profile",
                profile,
                "-d",
                database,
                "--allow-write",
                "-i",
                "-",
            ]);
            run_with_stdin(&mut command, sql.as_bytes())
        }

        /// sqlh as the reader: the test profile's server, this login, `args`.
        fn run(&self, args: &[&str]) -> Run {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sqlh"));
            command
                .stdin(Stdio::null())
                .env("SQLCMDPASSWORD", &self.password)
                .args(["--profile", &self.probe.profile, "-d", &self.probe.database])
                .args(["-U", &self.login])
                .args(args);
            run(&mut command)
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            let (login, database) = (&self.login, &self.probe.database);
            let sql = format!(
                "USE [{database}];\n\
                 IF USER_ID(N'{login}') IS NOT NULL DROP USER [{login}];\n\
                 USE master;\n\
                 IF SUSER_ID(N'{login}') IS NOT NULL DROP LOGIN [{login}];"
            );
            let _ = Reader::admin(&self.probe.profile, database, &sql);
        }
    }

    /// 128 random bits as a password that meets SQL Server's complexity policy: the keys of
    /// std's `RandomState` come from the operating system's random source.
    fn random_password() -> String {
        use std::hash::BuildHasher;
        let state = std::collections::hash_map::RandomState::new();
        let (a, b) = (state.hash_one(1u8), state.hash_one(2u8));
        format!("Aa1!{a:016x}{b:016x}")
    }

    // Owner decision 5 (PLAN.md 3.4): the database's permissions are the boundary, as DESIGN.md
    // §7 says. A login with only read rights reads; asked to write, with --allow-write getting
    // past sqlh's guard, the server refuses (error 229) and nothing changes; without
    // --allow-write, sqlh's guard refuses first.
    #[test]
    #[ignore = "live: needs SQLH_TEST_PROFILE; run with --ignored"]
    fn a_read_only_login_reads_and_the_server_refuses_its_writes() {
        let (p, db) = target();
        let reader = Reader::create(&p, &db);
        let t = reader.probe.name.clone();

        let r = reader.run(&["-Q", &format!("SELECT COUNT(*) AS n FROM {t}")]);
        assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
        assert!(r.stdout.contains("n:int\n0\n"), "{}", r.stdout);

        let insert = format!("INSERT {t} VALUES (1)");
        let refused = reader.run(&["-Q", &insert]);
        assert_eq!(refused.code, 5, "{}", refused.stdout);

        let denied = reader.run(&["--allow-write", "-Q", &insert]);
        assert_eq!(denied.code, 1, "{}", denied.stdout);
        assert!(
            denied.stdout.contains("-- error 229 (severity 14, "),
            "{}",
            denied.stdout
        );
        let count = live(&p, &db, &["-Q", &format!("SELECT COUNT(*) AS n FROM {t}")]);
        assert!(count.stdout.contains("n:int\n0\n"), "{}", count.stdout);
        assert!(
            !denied.stdout.contains(&reader.password) && !denied.stderr.contains(&reader.password),
            "the password never appears in the output"
        );
    }
}
