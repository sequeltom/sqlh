# Windows shells and paths (PLAN.md 2.13)

Measured 2026-10-11 on the dev machine: Windows 11 Pro 10.0.26200 (en-US, OEM code page 437, ANSI 1252), cmd.exe, Windows PowerShell 5.1.26100.9444, PowerShell 7.6.6 and Git for Windows 2.55.0 (bash 5.3.15), against sqlh at `f12b041` plus this change. The server was the dev server through the `test` profile, for the runs that needed one.

## What came out of it

| Question | Answer | Where it lives |
|---|---|---|
| A Git Bash path in cmd.exe or PowerShell | Not found, with no hint. Now a usage error that names `C:\dir\f.sql` and `C:/dir/f.sql`, for `-i`, `-o` and `SQLH_CONFIG`. | `console::git_bash_hint`; DESIGN.md §4.2, §8 |
| `--help` examples | The heredoc is gone. Every example runs unchanged in cmd.exe, Windows PowerShell 5.1, PowerShell 7 and Git Bash. A test runs them. | `src/help.txt`; test `the_help_examples_run_as_written_in_every_shell` |
| Piped non-ASCII text in Windows PowerShell 5.1 | Arrives as `?`, silently. `--help` and DESIGN.md §4.1 say to set `$OutputEncoding` or use `-Q`/`-i`. Verified live. | `src/help.txt`, DESIGN.md §4.1 |
| Piped text in another encoding (cmd.exe at an OEM page, PowerShell with the console's encoding) | Refused as not valid UTF-8 (it already was), now with the fix named. | `decode_source` in `main.rs` |
| Forward slashes, UNC, relative paths, spaces for `-i`, `-o`, `SQLH_CONFIG` | All worked already. Now pinned by a test. | test `path_forms_work_for_input_output_and_config` |
| A path from Explorer's "Copy as path" (double-quoted, single backslashes, spaces) after `-i` and `-o` | Works as is in cmd.exe, Windows PowerShell 5.1, PowerShell 7 and Git Bash. Input is never escaped; the same path in a `-- ` line has its backslashes doubled (owner decision 11). | test `a_path_from_copy_as_path_works_after_i_and_o_in_every_shell`; DESIGN.md §5.1 |
| `-Q` quoting | Works in every shell for one line without `"`, `$`, `%` or a backtick, with single quotes inside. Table below. | DESIGN.md §4.1; test `the_documented_q_form_reaches_sqlh_intact_in_every_shell` |
| PowerShell capturing output (`$r = sqlh ...`) at a non-UTF-8 console | Garbled in 5.1 and 7 alike. Documented, with the two fixes. | DESIGN.md §4.1 |
| The console code page that `console::use_utf8` set and never restored | **Removed, not restored.** The switch was never needed, so there is nothing to put back on any exit path. | `console.rs`; DESIGN.md §11; test `sqlh_leaves_the_console_code_page_alone` |

## Method

- **What a program receives.** `echoargs.exe` (a few lines of Rust) prints each argument as Rust's Windows parser sees it, which is the parser sqlh uses, and the bytes on its stdin. cmd.exe lines ran as `cmd /s /c "line"`, which is a prompt without the quote-stripping surprises (`%name%` is replaced when the variable exists and left alone when it doesn't, as at a prompt). PowerShell code ran through `-EncodedCommand`, so no quoting layer sits between the test and the parser. Git Bash ran scripts.
- **Fresh consoles.** A process started with `CREATE_NO_WINDOW` gets a console of its own at the system default code page (437 here), not the page of the session that started it (65001 in the tool's shell). That is how the non-UTF-8 cases were made repeatable. The screen was read back with `ReadConsoleOutputCharacterW` and the output code page with `GetConsoleOutputCP`. `chcp` and `mode con` report the *input* code page, not the output one, so they can't show what `SetConsoleOutputCP` changed.
- **Live runs** used `--profile test` with read-only literals (`SELECT N'...'`).

To repeat the argument measurements, `rustc -O --edition 2024 -C target-feature=+crt-static echoargs.rs` on this (with `--stdin` it also prints the bytes of stdin), then call it from each shell as `sqlh` would be:

```rust
use std::io::Read;

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    for (n, arg) in args.iter().enumerate() {
        let mut shown = String::new();
        for c in arg.to_string_lossy().chars() {
            match c {
                '\\' => shown.push_str("\\\\"),
                '"' => shown.push_str("\\\""),
                c if !(' '..='~').contains(&c) => shown.push_str(&format!("<U+{:04X}>", c as u32)),
                c => shown.push(c),
            }
        }
        println!("arg{}: [{shown}]", n + 1);
    }
    if args.first().is_some_and(|a| a == "--stdin") {
        let mut bytes = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut bytes);
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        println!("stdin {} bytes: {}", bytes.len(), hex.join(" "));
    }
}
```

Backslashes and quotes in its output are escaped, so `[C:\\dir]` is `C:\dir`.

## Git Bash

Git Bash converts an argument that looks like a POSIX path before a Windows program sees it (MSYS path conversion). What sqlh receives:

| Typed in Git Bash | Received |
|---|---|
| `-i /c/code/x.sql` | `C:/code/x.sql` (forward slashes) |
| `/c`, `/C/Users` | `C:/`, `C:/Users` |
| `-i=/c/code/x.sql`, `-i/c/code/x.sql` | `-i=C:/code/x.sql`, `-iC:/code/x.sql` |
| `SQLH_CONFIG=/c/Users/me/p.toml sqlh ...` | `C:/Users/me/p.toml` (environment variables are converted too) |
| `//c/code` | unchanged, which Windows reads as the UNC path `\\c\code` |
| `/cygdrive/c/code`, `/mnt/c/code` | `C:/Program Files/Git/cygdrive/c/code`, `.../mnt/c/code` (not drive paths; nothing to hint) |
| `MSYS_NO_PATHCONV=1 ... -i /c/code/x.sql` | unchanged: sqlh gets `/c/code/x.sql` |
| `-i C:\code\x.sql` (unquoted) | `C:codex.sql`: bash removed the backslashes |
| `-i "C:\code\x.sql"`, `-i 'C:\code\x.sql'` | `C:\code\x.sql` |

In cmd.exe, Windows PowerShell and PowerShell 7 the argument is passed as typed (`/c/code/x.sql`), and Windows reads it as `\c\code\x.sql`, a path on the current drive. Before this change the error for `-i` and `-o` was `-i /c/code/x.sql: The system cannot find the path specified. (os error 3)` and nothing else. For `SQLH_CONFIG` there was no error at all: a missing file means "no profiles yet", and the next `profile add` would have gone on to create `\c\...` at the root of the drive (`config::save` calls `create_dir_all`; read, not run, so as not to create the folder).

Now, when the path isn't found and has the form `/<letter>` followed by `/` or the end:

```
-- error: -i /c/code/x.sql: The system cannot find the path specified. (os error 3); that is a Git Bash path, which only Git Bash converts; in cmd.exe and PowerShell write C:\\code\\x.sql or C:/code/x.sql
```

The backslashes are doubled by the text format (DESIGN.md §5.1; owner decision 11, settled 2026-10-11: a path in a `-- ` line keeps the escape rule, see "Copy as path" below), which is why the forward-slash spelling is offered too: it reads the same in text, json and csv, and every Windows API and sqlh accept it. A path that is found is used as written. `//server/share` (a UNC path), `/tmp/x` and `/cygdrive/c/...` get no hint. For `SQLH_CONFIG` the check is on the folder, because a missing file is normal: `SQLH_CONFIG /c/Users/me/profiles.toml: the folder wasn't found; that is a Git Bash path, ...`.

## Paths

Every form below worked for `-i` and for `-o` (the file is created as a `.partial` file and renamed, with none left behind) before this change, run by hand, and for `SQLH_CONFIG` the first time it was tried, by the new test. The test runs the forms side by side; the folder and the files have spaces in their names.

| Form | Example |
|---|---|
| backslashes | `C:\dir\in file.sql` |
| forward slashes | `C:/dir/in file.sql` |
| mixed | `C:\dir/in file.sql` |
| relative | `in file.sql`, `.\in file.sql`, `..\dir\in file.sql`, `in file.sql` with `/` |
| UNC | `\\localhost\C$\dir\in file.sql` |
| also tried by hand | `//localhost/C$/dir/in file.sql`, `\\?\C:\dir\in file.sql`, drive-relative `C:work\in.sql` |

- **UNC.** This machine's administrative share, `\\localhost\C$` (reachable under the computer's own name and as `\\127.0.0.1\c$` too), is open to this user for reading and for writing, so UNC input and output were exercised for real. The test skips the UNC form, with a message, where the share can't be reached. An `-o` run over the share is the slowest form (69 ms against 43-55 ms locally on a quiet machine; it creates a delete-on-close file and renames it over SMB, loopback included).
- **Same file, other spelling.** `-o` onto the input is refused for every pair tried (backslashes vs forward slashes, relative vs UNC, different case): the check compares file identity, not text.
- **Spaces** need quotes in every shell (`-i "C:\my dir\a b.sql"`). Single quotes work in PowerShell and Git Bash, not in cmd.exe, which passes them through. A directory with a trailing backslash inside quotes (`"C:\out dir\"`) swallows the rest of the line in cmd.exe and Windows PowerShell 5.1 (the `\"` is an escaped quote); nothing in sqlh takes a directory.
- **`~`** is expanded to a native program only by PowerShell 7; cmd.exe and Windows PowerShell 5.1 pass `~\a.sql` as is, and sqlh reports it as not found.
- **`%TEMP%`, `$env:TEMP`** are expanded by the shell, as expected.

### Copy as path (owner decision 11, 2026-10-11)

Explorer's "Copy as path" puts the full path on the clipboard in double quotes with single backslashes: `"C:\code\new reports\tab files\in file.sql"`. Pasted after `-i` or `-o` it is the argument as it stands. **Input is never escaped**: the escape rule (a backslash is `\\`, `\n` is a line feed) is for what sqlh writes, and a path is taken exactly as typed. The folders in the test are named so that a decoder would show: `\new reports` and `\tab files` (and `\tmp` in the scratch path) would be read as a line feed and a tab.

- **By test**, `a_path_from_copy_as_path_works_after_i_and_o_in_every_shell`, in cmd.exe, Windows PowerShell 5.1 and PowerShell 7: `sqlh -i "<path>" -o "<path>"` with a blocked statement, so the run ends at the guard without connecting. Exit 5, the refusal in the output file, no `.partial` file left; and with two inputs, `sqlh -i "<a>" -i "<b>"`, exit 5 and the refusal names the second path.
- **By hand, in Git Bash**, against the release build, the same paths. Double quotes and single quotes both give exit 5 (the input was read, the output file got the refusal). **Unquoted** backslashes are removed by bash: `-i C:\code\sqlharness\x.sql` reaches sqlh as `C:codesqlharnessx.sql`, which is not found (`-- error: -i C:codesqlharnessx.sql: The system cannot find the file specified. (os error 2)`). That is the shell's rule, not sqlh's, and the quotes Explorer puts around the path avoid it.
- **In a `-- ` line the path has its backslashes doubled.** With two inputs the refusal reads `-- blocked: C:\\code\\...\\new reports\\tab files\\in file.sql:1: UPDATE dbo.T would write; ...`. A reader decodes the field like any other (`\\` is `\`) and gets the path back. Copied back as it is, a local path still works (Windows folds `\\`), a UNC path does not (`\\\\server\\share`).
- **Rejected** (decision 11): writing forward slashes in `-- ` lines (it rewrites what was typed, and some cmd built-ins refuse forward slashes), and escaping a backslash only before an escape letter (a second reader rule, and `C:\new` would still be doubled).

## `-S`

| Typed | cmd.exe | Windows PowerShell 5.1 | PowerShell 7 | Git Bash |
|---|---|---|---|---|
| `-S myserver,1433` | `myserver,1433` | same | same | same |
| `-S myserver\inst` | ok | ok | ok | `myserverinst` |
| `-S (local)`, `-S (localdb)\MSSQLLocalDB` | ok | error: `local` is not a command | same | syntax error |
| `-S "myserver\inst"`, `-S "(localdb)\MSSQLLocalDB"` | ok | ok | ok | ok |
| `-S '(localdb)\MSSQLLocalDB'` | the quotes are kept | ok | ok | ok |
| `-S "[2001:db8::5],1433"` | ok | ok | ok | not tried |

So double quotes are the one form that works everywhere, and `host,port` needs none (PowerShell keeps `a,1433` as one argument for a native program).

## `-Q`

What `-Q` receives. "ok" means the SQL arrived as intended.

| SQL as typed | cmd.exe | Windows PowerShell 5.1 | PowerShell 7 | Git Bash |
|---|---|---|---|---|
| `-Q "SELECT ... WHERE s = 'it''s'"` | ok | ok | ok | ok |
| `& \| < > ^ !` inside double quotes | ok | ok | ok | ok |
| non-ASCII inside the argument | ok | ok | ok | ok |
| `'%temp%'` inside double quotes | replaced by the folder if the variable exists | ok | ok | ok |
| `$(db)` (sqlh's variable) inside double quotes | ok | **PowerShell runs it**: an error on stderr, and sqlh gets `''` | same | **bash runs it** as a command substitution |
| `$(db)` inside single quotes | the quotes are kept and the text splits at spaces | ok | ok | ok |
| `$x` inside double quotes | ok | replaced by the (empty) variable | same | same |
| `"` inside, as `\"` in double quotes | ok | breaks the string (garbage arguments) | same | ok |
| `"` inside, raw in single quotes | the quotes are kept and the text splits | **the quotes are dropped**, silently | ok | ok |
| `"` inside, as `\"` in single quotes | the quotes are kept and the text splits | ok | `\"` arrives with its backslashes | `\"` arrives with its backslashes |
| `"` inside, as `""` in double quotes | ok | **the quotes are dropped** | ok | **the quotes are dropped** |
| `"` inside, as `` `" `` in double quotes | not applicable | **the quotes are dropped** | ok | not applicable |
| a backtick | ok | PowerShell's escape character | same | **bash runs it** as a command substitution |
| a line break | cannot be typed | ok | ok | ok |
| unquoted `-Q SELECT 1` | two arguments: sqlh refuses ("SQL was already given") | same | same | same |

A double quote inside the SQL has no spelling that works in all four, and in Windows PowerShell 5.1 the wrong one changes the SQL without an error (`"a"` becomes `a`). The rule that does work everywhere, in DESIGN.md §4.1: double quotes outside, single quotes inside, no `"`, `$`, `%` or backtick, one line; otherwise a file and `-i`.

### Percent signs in cmd.exe

At a prompt (and in `cmd /c`) `%name%` is replaced when the variable exists and left alone when it doesn't. In a batch file it differs: an undefined `%b%` disappears, `%%` is one `%`, a lone `%` disappears (`100 % 7` became `100  7`), and `%1` is the first argument.

| Typed | Prompt | Batch file |
|---|---|---|
| `LIKE '%b%'` (no variable `b`) | `%b%` | empty |
| `LIKE '%temp%'` | the temp folder's path | the temp folder's path |
| `100 % 7` | `100 % 7` | `100  7` |
| `100 %% 7` | `100 %% 7` | `100 % 7` |

`LIKE '%temp%'` is an ordinary query, and cmd.exe turns it into a path without a word.

## Piped input

What sqlh gets on stdin, for the string `SELECT N'Café …'` (e acute, ellipsis; CJK behaves like the ellipsis):

| Sender | Bytes | sqlh |
|---|---|---|
| Windows PowerShell 5.1, default `$OutputEncoding` (US-ASCII) | `é` and `…` both `3F` (`?`), then `0D 0A` | runs it with `?`: **silent** |
| Windows PowerShell 5.1, `Get-Content file.sql \| sqlh` (UTF-8 file, no BOM) | the file is read as ANSI (`Ã©`), then each character is `?` | runs it with `??`: **silent** |
| Windows PowerShell 5.1, `$OutputEncoding = [Text.Encoding]::UTF8` | `EF BB BF`, then UTF-8 | ok (the BOM is accepted) |
| Windows PowerShell 5.1, `$OutputEncoding = [Text.UTF8Encoding]::new($false)` | UTF-8 | ok |
| Windows PowerShell 5.1, `$OutputEncoding = [Console]::OutputEncoding` (OEM 437) | `é` is `82`, `…` is `2E` (`.`) | `é`: refused, not valid UTF-8; `…` alone: **silent** |
| PowerShell 7, default | UTF-8, no BOM | ok |
| cmd.exe, `echo ... \| sqlh` at code page 437 | `é` is `82`; `…` is `2E`, CJK is `3F 3F` | `é`: refused; the others: **silent** |
| cmd.exe after `chcp 65001` | UTF-8 | ok |
| cmd.exe, `type file.sql \| sqlh` and `sqlh < file.sql` | the file's bytes | ok |
| PowerShell (either), `sqlh < file.sql` | "The '<' operator is reserved for future use" | not run |
| bash, heredoc or pipe | UTF-8 | ok |

Live, from Windows PowerShell 5.1 at code page 437 with the same SQL: the default `$OutputEncoding` ran `SELECT N'Caf? ? ??'`; `[Text.Encoding]::UTF8` returned `Café … 日本` from the server; the OEM encoding was refused with exit 3 and the message `stdin: not valid UTF-8 (byte 12); save it as UTF-8, or UTF-16 with a BOM. Piped text is encoded by the shell: in cmd.exe run chcp 65001 first, in Windows PowerShell run $OutputEncoding = [Text.Encoding]::UTF8 first, or pass the SQL with -Q or -i`.

The silent cases can't be detected: a `?` in the input is a legal character, and replacing it changes what a query matches. That is why `--help` and DESIGN.md say to use `-Q` or `-i`, or to set the encoding first. `-Q` is safe because a command line is UTF-16 end to end (the non-ASCII case in the `-Q` table), and so is `-i` with a UTF-8 or UTF-16 file.

## Captured output

sqlh writes UTF-8 bytes. A program that captures them decodes them itself. In a console at code page 437, with the output `Café … 日本`:

| Capture | Result |
|---|---|
| `$r = sqlh ...` in Windows PowerShell 5.1 or PowerShell 7 | decoded as 437: `Caf├⌐ ΓÇª µùÑµ£¼` |
| the same after `[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)` | correct (this also changes the console's code page to 65001 until the encoding is set back) |
| `sqlh ... -o file`, then `Get-Content file` in Windows PowerShell 5.1 | decoded as ANSI (1252): `CafÃ© â€¦ æ—¥æœ¬` |
| the same with `Get-Content -Encoding UTF8 file` | correct |
| the same in PowerShell 7 (UTF-8 is its default) | correct |
| `--format json`, then `ConvertFrom-Json` on the captured text | garbled the same way as the first row |

A harness that reads PowerShell's own stdout as UTF-8 is a different case. PowerShell decodes sqlh's bytes with the console's page and encodes them again with the same page when it writes, and that gives the bytes back: with `chcp` 437, 850, 1252 and 65001, a JSON cell holding `Café … 日本` left Windows PowerShell 5.1 and PowerShell 7 (stdout redirected to a file, as a harness has it) byte for byte as sqlh wrote it. So the text survives when PowerShell only passes it on. It is garbled when a script handles the text, or writes it with `Out-File`, `>` or `Set-Content`, which use other encodings.

Previously an earlier interactive run of sqlh in the same console would sometimes have made this work, by accident: it left the console at UTF-8, so a *new* PowerShell started there decoded correctly. A PowerShell session that was already running kept its old `[Console]::OutputEncoding` (it reads the code page once), and did not benefit. Now the result doesn't depend on what ran before.

### csv and stderr

csv sends its `-- ` lines to stderr. Windows PowerShell 5.1 turns each stderr line of a native program into an error record: with `2>&1` the first line is decorated (`sqlh.exe : -- blocked: ...` plus the script position), `2> file` writes ten lines of decoration for two lines of sqlh, and `$ErrorActionPreference = 'Stop'` throws on the first line. Sending stderr to a file through cmd (`cmd /c "sqlh ... 2> file"`) is clean. PowerShell 7 shows plain text and doesn't throw. text and json keep everything on stdout.

## The console code page

`console::use_utf8` ran `SetConsoleOutputCP(65001)` at startup whenever stdout was a console, and nothing put the page back.

**Measured** in a fresh console at 437, with `GetConsoleOutputCP` read after sqlh had exited:

| Build | Output code page after `sqlh --help` | A new `powershell.exe` in that console sees |
|---|---|---|
| before | 65001 | `[Console]::OutputEncoding.CodePage` = 65001 |
| after | 437 | 437 |

**Was the switch needed?** No. Rust's `Stdout` writes a console with `WriteConsoleW` (UTF-16), whatever the page. The console's screen buffer after `sqlh --help` and after a query returning `Café … 日本` was identical with and without the switch, character for character (checked with `ReadConsoleOutputCharacterW`), at 437. The switch mattered only for bytes written to a console device with the narrow API.

**Why remove rather than restore.** Restoring would have needed a put-back on every exit path (a normal return, `process::exit` in the panic hook, an early Ctrl+C before the signal listeners exist, a kill) and would have changed the console under a program that was still writing to it, or under a second sqlh run at the same time in the same console. Not changing it needs none of that, and leaves nothing behind in the shell's console.

**The trade-off.** `-o CON` (the console device as an output file) now shows the UTF-8 bytes in the console's page: `Café …` appears as `Caf├⌐ ΓÇª` at 437. Before, the switch made it appear right by accident when stdout was also the console. Nobody has a reason to write `-o CON`, since stdout is the console already, and any program that writes bytes to `CON` has the same behavior. Not worth code.

Ctrl+C, Ctrl+Break, a close and a crash have nothing to restore. The test starts a hidden console at 437, runs `--help` and a refused statement in it (exit 0 and exit 5), and reads the output page back. Against the old code it reads 65001.

## `--help` examples

The EXAMPLES section is four runnable lines and four lines of notes (`--help` is 62 lines, as before; a test holds it). Each runs unchanged in cmd.exe, Windows PowerShell 5.1, PowerShell 7 and Git Bash:

- by test, in the first three: `the_help_examples_run_as_written_in_every_shell` takes the lines from `--help`, swaps `myserver` for a listener that closes every connection, and runs them with `sqlh` resolved from `PATH`. Exit 2 with `cannot connect to <listener>` means the shell passed the options, the file with a space in its folder name, the profile and the login through (a `-o` example's output is read from its file). A deliberately broken example (`-Q "SELECT \"a\" FROM t"`) fails it in both PowerShells.
- by hand, in Git Bash, with the same substitution against a refused address: all four examples reached the connect (exit 2).

Forms that don't run in every shell, and so are kept out of the examples: the heredoc (`<<'SQL'`, which the old help had) in cmd.exe and PowerShell; `-S (local)` in PowerShell; `-i C:\dir\f.sql` unquoted in Git Bash; `sqlh < file.sql` in PowerShell.

## Not checked

- Windows Terminal, VS Code's terminal and ConPTY hosts (the console API is the same underneath, and Rust's console detection is `GetConsoleMode`).
- The Windows setting "Beta: Use Unicode UTF-8 for worldwide language support", which makes 65001 the OEM page and removes most of the encoding problems above except Windows PowerShell 5.1's ASCII `$OutputEncoding`.
- OEM pages other than 437, Windows in another language (the OS error text in the not-found message is localized; the tests don't depend on it), and a console font without the glyphs.
- Batch files containing non-ASCII text (cmd.exe reads them in the console's code page).
- `ISE`, `pwsh` on non-Windows, WSL.
- Paths over 260 characters.
