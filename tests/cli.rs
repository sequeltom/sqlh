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
}
