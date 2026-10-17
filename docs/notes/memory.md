# Memory (PLAN.md 2.7)

Measured 2026-10-11 on the dev machine (Windows 11, 12 logical cores, 31 GB) against the owner's dev/test SQL Server 2025 over Wi-Fi, with release builds of two trees and mssql-tds 0.2.0: **before** is `f12b041`, **after** is the 2.7 change. Large values are built on the server and read-only, for example `REPLICATE(CAST('x' AS varchar(max)), 1073741824)`. Peak working set and peak commit come from the exited child's process handle (`GetProcessMemoryInfo`), CPU time from `GetProcessTimes`, so nothing is sampled. The wrapper that did this was a short program kept outside the repository; `run_measured` in `tests/cli.rs` does the same for the tests. A run of `sqlh --version` peaks at 7 MB; a run that connects peaks at 9.6 MB of working set and 1.9 MB of commit before it has read anything. The exe grew from 2,285,568 to 2,341,376 bytes (2.4%).

## Summary

Peak memory for the value or rows below, with the default limits (`--max-cell 500`) unless the first column says otherwise. 1 GiB took about 20 s to read, so it is the size used (the acceptance in PLAN.md asks for 1 GB, or at least 200 MB if that is too slow).

| Case | Before | After |
|---|---|---|
| 1 GiB `varchar(max)` value | 2,057 MB working set (2,054 MB commit), 20.4 s | **10.3 MB** (2.0 MB commit), 19.4 s |
| 1 GiB `nvarchar(max)` value (512M characters) | 2,569 MB (2,567 MB commit), 18.5 s | **9.7 MB** (1.9 MB commit), 18.7 s |
| 100 MiB `varchar(max)` value | 209 MB, 2.1 s | 9.7 MB, 2.2 s |
| 60 MB `varbinary(max)` value | 66 MB | 9.6 MB |
| 20 rows of 50 MB, `--max-rows 1` (19 hidden) | 105 MB, 15.6 s | 9.7 MB, 15.8 s |
| 200 MB `nvarchar(max)` value, `--max-cell 0` (the 1 MB output budget decides) | 486 MB, 4.7 s | 12.0 MB (commit 4.9 MB), 4.1 s |
| 100 MB `varchar(max)` value, `--max-cell 0 --max-output 0` (everything asked for) | 273 MB | 268 MB |
| `text` of 90 MB, `ntext` of 90 MB, `image` of 90 MB | 181, 224, 95 MB | unchanged |
| `text` of 120 MB | exit 10: "Text data length 120000000 exceeds maximum allowed size of 104857600 bytes" | unchanged (the driver's limit) |
| 200,000 `$(big)` references, `big` 3,000 bytes (600 MB if built) | 595 MB (886 MB commit), 0.35 s | **29 MB** (24 MB commit), 0.03 s |

CPU time for the 1 GiB values went from 1.5 s to 0.9 s (`varchar`) and from 1.8 s to 1.4 s (`nvarchar`): decoding and counting the characters that are cut costs less than allocating and copying the value did. The run time is the network's and the server's: the same within a second or two, either way, from one run to the next.

The output of the two builds was compared byte for byte (timings aside) in 180 runs, in text, json and csv: cut limits from 6 to 1000 characters; values of 2 to 6 MB, so that chunk boundaries fall inside surrogate pairs and double-byte characters; the Japanese, Chinese (simplified and traditional), Korean, Cyrillic and Latin-1 code pages and a UTF-8 collation, all built from code points on the server; `nvarchar`, `varbinary`, `xml`, `json`, `geography`, `hierarchyid`, `text`, `ntext` and `image`; NULLs and empty strings; `FOR XML` and `FOR JSON`; procedures; rows past `--max-rows`; `--max-results`; errors after some rows; messages between result sets; and `--max-cell 0` with and without `--max-output`. None differed. The unit tests in `db/stream.rs` pin the same thing against the driver's own whole-value decoding, for 11 encodings and for random malformed bytes, in chunks of 1 byte upward.

One difference is known, found afterwards and left: in the text format a value in the first column that starts with `--` gets a backslash, so that it can't pass for a metadata line. The sink decides that from the whole value, and a cut value arrives without the part past the cut, so with `--max-cell 1` the first column's `--abc` of a `(max)` column is now written `-…[+4]` where it was `\-…[+4]`. From `--max-cell 2` on the kept part still starts with `--` and nothing changes. Neither form can be mistaken for a `-- ` line.

## Why not a cutting `RowWriter`

PLAN.md proposed `next_row_into` with a `RowWriter` that cuts at `--max-cell`. In 0.2.0 that cuts after the peak:

- `datatypes/decoder.rs`, macro `read_value_into!`: for a PLP value whose total length the server declares, the decoder asks the writer for a destination with `RowWriter::value_destination(col, kind, length)`. A writer that answers `None` gets `Vec::with_capacity(length)`, filled from the wire, then handed to `write_string` or `write_bytes`. A writer that answers `Some` must return a slice of exactly `length` bytes (the trait's contract). Either way a value of 1 GiB is in memory in one piece before any writer code sees a byte of it. `DiscardRowWriter` declines, so decoding a hidden row into it allocates the value too. A value without a declared total (`PLP_UNKNOWNLEN`) is accumulated chunk by chunk into a growing `Vec` instead (`read_plp_chunks_unknown_len`).
- `ResultSet::next_row` is `next_row_into(DefaultRowWriter)`, so the old read path was this one: the driver's `Vec` of 1 GiB, then `SqlString::to_utf8_string` made a second copy, then the sink cut it.

The driver has another way in, built for ODBC `SQLGetData` and used by sqlh now:

- `next_row_cursor` positions on the next row after reading only its header; any row it left half read is drained first, with `ColumnPolicy::SkipAll`, which skips a PLP value chunk by chunk without allocating it (`token_stream.rs`, `skip_column`, `PlpColumnStream::skip_to_end`) and decodes other columns into a discarding writer.
- `read_row_column(i)` returns `CursorColumn::PlpStreaming` for a PLP column and leaves the payload on the wire; `read_active_plp_chunk(buf)` then fills `buf` from it (up to 32 KB at a time here), reports `reached_end`, and honours the query timeout and the cancel handle like any other read. The `try_` variants (`try_next_row_cursor`, `try_read_row_column`, `try_read_active_plp_chunk`, `try_read_row_plp_complete`) do the same synchronously when the bytes are already buffered.
- `ResolvedDecoder` (`datatypes/sql_string`) is the driver's incremental text decoder, with encoding_rs semantics, for every encoding a collation can name (Windows-125x and 874, the double-byte pages 932, 936, 949 and 950, UTF-8, and its own tables for CP437 and CP850). It is what `SqlString::decode` uses, so the characters are the same ones.

## What sqlh does now

`db/stream.rs` and `Session::read_rows`:

- **A result set with a PLP column** (`varchar(max)`, `nvarchar(max)`, `varbinary(max)`, `xml`, `json`, CLR types) is read through the cursor. A column that isn't PLP is read with `read_row_column`, which decodes it whole. For each PLP cell of a row that is shown, a `Cutter` takes the chunks. Text goes through the decoder into UTF-8, the first `cap` characters are kept, and the rest is only counted; binary keeps `cap / 2` bytes and counts the rest. A small value that is already in the read buffer arrives in one piece (`try_read_row_plp_complete`).
- **`cap`** is `BatchSink::cell_limit()`: `--max-cell`; with `--max-cell 0` the `--max-output` budget in characters (every character takes at least one byte of the response, so a longer value can only be withheld; the output is identical to before); 0 only when both are off, which keeps the whole value, as asked.
- **The counts are exact.** A character is a Unicode scalar value, and a malformed sequence is one U+FFFD, as in the old whole-value decode, so the decoder runs over everything, not just the kept part. A surrogate pair, a UTF-8 sequence or a double-byte character that straddles the cut or a chunk boundary is one character. `xml` drops a byte order mark at its start, as before.
- **The sinks** get `Value::TextCut(kept, more)` and `Value::BinCut(kept, more)` and write the kept part and the `…[+N]` marker, which is what they wrote for a `Text` they cut themselves (`output/mod.rs` has an exact-output test across the three formats).
- **A row past `--max-rows`** (or after the budget ran out, or in a result set that `--max-results` hides) is not read into values. In a result set with a PLP column it is left positioned and skipped by the next `next_row_cursor`. In any other result set it is decoded into `DiscardRowWriter`, so no `ColumnValues` is built and a string isn't copied out of the read buffer.
- **A result set with no PLP column** is read as before (`next_row`), which is the faster path for narrow rows: its cells are at most 8,016 bytes, apart from `text`, `ntext` and `image`. A result set with an Always Encrypted column, which the driver can't stream, stays on this path too.

## The output path, component by component

| Part | Bound | Evidence |
|---|---|---|
| Driver read buffer | packet-sized (the driver asks for 8,000-byte packets) | baseline process: 9.6 MB working set |
| Scratch for large values | 32 KB chunk + 64 KB decode room, made once per session | `Scratch`, unit test |
| A kept cell | at most `cap` characters: up to 4 bytes each as UTF-8 text, `cap / 2` bytes of binary | unit test `a_cutter_holds_the_limit_not_the_value`: 16 MB through a cutter held under 4 KB |
| A row's values | the sum of its cells | |
| The formatted line (`TextSink::line`, `JsonSink::buf`, `CsvSink::line`) | columns × cap × the longest escape: 4 bytes a control character in text, 6 in JSON (`\u0001`) | 400 columns x 500 control characters (1.2 MB of JSON a row), reader stalled: 12.8 MB working set, 6.2 MB commit |
| A writer's buffer | 64 KB per stream (csv has two) | `ChannelWriter` |
| A queued chunk | at most 64 KB (R11: `ChannelWriter::write` splits any write) | `a_big_write_is_queued_in_chunks` |
| One write to the reader | at most 4 KB | `WRITE_UNIT` |
| The queue | 256 KB counted, the chunk being written included, **plus the event that crossed the threshold**: the check runs after each row and before each statement's results, so a row, or the messages of one statement (at most `--max-messages` formatted), can arrive past it | reader that never reads, `-t 2`: narrow rows 10.0 MB working set (2.1 MB commit); 1,000 columns x 600 characters 11.9 MB (4.5 MB commit) |
| `--verbose` stderr in text and json | a thread and queue of their own, one line at a time | |

The 256 KB limit is therefore a threshold, as DESIGN.md §11 now says. The code-review note that `ChannelWriter::write` appends a whole caller buffer before checking 64 KB described the code before R11: the loop that sends a chunk whenever the buffer reaches 64 KB is the fix, and the test above pins it. What the review also asked for, a bound on the largest encoded event, is the row bound above. It could be tightened to the output budget by stopping a row's formatting once its line passes what is left, which would touch the hot loop of all three sinks; at the default limits the bound is already far under the budget for ordinary rows, and a pathological one (4,096 columns of 500 control characters in JSON) is about 12 MB of line, as much again of values and as much again queued.

Messages: a loop of 1,000,000 `PRINT`s with the default `--max-messages` peaked at 9.5 MB (100,000: 9.5 MB too); the driver hands them over at statement boundaries, and sqlh counts the ones past the limit without formatting them.

## Substitution (`script.rs`)

`substitute` took the whole text and checked the 10 MB limit after building it. It now checks before every append (original text, a value, a literal `$(`) against what the run has left of the limit, so a reference repeated a million times stops at the limit: 200,000 references to a 3,000-byte value went from 595 MB working set (886 MB commit) to 29 MB (24 MB commit), and from 0.35 s to 0.03 s. The text is reserved with `reserve_exact` and never past the room, so its capacity is at most the limit. The line map is now made only when a value first adds lines (before, it was built eagerly and thrown away if no value had any): a 9 MB result with no multi-line value went from 33 MB to 27 MB of commit, and a result with 2.4 million lines keeps a map of 4 to 8 bytes a line. Accepted inputs, messages and exit codes are unchanged; `script.rs` tests compare the new function with the old algorithm over 20,000 random inputs and several rooms.

## Cost

Throughput against the same server, 7 runs each (3 for the two longest), medians, before and after (Wi-Fi, a shared server: the spread between runs is as large as most of the differences). The tables are the StackOverflow2010 sample database's `dbo.Votes` (10.1M rows of five fixed-width columns), `dbo.Posts` (3.7M rows; `Body` is `nvarchar(max)`) and `dbo.Comments` (3.9M rows; `Text` is `nvarchar(700)`). "Shown" is `--max-rows 0 --max-output 0`, written to a file; "hidden" is the default `--max-rows 100`. "Small PLP column" is `CAST(UserId AS nvarchar(max))`.

| Case | Before | After |
|---|---|---|
| 2M rows of `Votes` shown (no PLP column: the `next_row` path, unchanged) | 2.09 s | 2.04 s |
| all of `Votes` (10.1M rows) hidden | 4.05 s | 3.71 s |
| json, 500K rows of `Votes` shown | 0.71 s | 0.69 s |
| 50K rows of `Posts` shown, `Id` and `Body` | 1.23 s | 1.35 s |
| 300K rows of `Posts` hidden, `Id`, `Title` and `Body` | 6.56 s | 6.64 s |
| all of `Comments` (3.8M rows) hidden (no PLP column) | 21.3 s | 22.1 s |
| 3M rows hidden, one small PLP column | 2.64 s | 2.80 s |
| 1M rows shown, one small PLP column | 1.13 s | 1.11 s |
| 3M rows hidden, int, int, small PLP, int | 1.93 s | 2.04 s |
| **1M rows shown, int, int, small PLP, int** | 0.73 s | **1.05 s** |
| **json, 300K rows of the same shape** | 0.38 s | **0.47 s** |

Where rows are big (a text column of a kilobyte or more) or have no PLP column, there is no difference beyond the noise. The two bold rows are the cost: a narrow row with a small PLP column in the middle, shown, is 25 to 45% slower, about 0.1 µs a column, because the cursor reads each column with a call of its own where the old path decoded the row in one. It only shows for explicit bulk output (`--max-rows 0`) of rows of a few dozen bytes. A hidden row of that shape costs about 5%.

## Found on the way

- **The read-only guard is not bounded by the input limit.** `guard::tokenize` makes a `Token` of some 56 bytes (a `&str`, an uppercased `String`, a kind and a line) for each token of the input, in one `Vec` that grows by doubling, so a run's peak depends on what the input looks like, not on its size: 10 MB of `(` peaked at 795 MB working set (1.18 GB commit); 9.9 MB of `SELECT 1` lines, 233 MB; 10 MB of two-letter words, 250 MB; 9 MB of `$(v)` references to a 9-byte value, 30 MB. The 10 MB cap bounds accepted SQL, not what it costs. A fix was outside this task (`src/guard.rs`); it is PLAN.md 2.14, measured in the last section below.
- **Abandoning a result set is not flat in the driver.** `drain_rows`, behind `advance` and `close_query`, reads the rest of a result set with `next_row_into(DiscardRowWriter)`, which allocates each PLP value whole. (`drain_result_set_rows`, which `drain_stream` uses after an error, skips with `ColumnPolicy::SkipAll` and doesn't.) sqlh's batch loop reads every result set to its end, so it doesn't reach `drain_rows` for rows; a future early cutoff (the drain policy, PLAN.md 2.6) would, and would need the cursor skip instead.
- **A cancel during the new read** ends the run like any other: Ctrl+Break while a 1 GiB value was streaming, and while rows of 400 MB were being skipped, gave exit 6 within 0.2 to 0.5 s with "the statement was cancelled; the connection stayed usable". The query timeout does too (`-t 12` on the 1 GiB value: exit 4; the driver then took 5 s to settle the attention, in the old build as well).

## Upstream issues worth filing

Moved to [upstream-issues.md](upstream-issues.md) (2026-10-17), the one list the owner files from: `DiscardRowWriter` is U2, `text`/`ntext`/`image` U3, and the UTF-8 panic U6 (upstream's own #310).

## The read-only guard (PLAN.md 2.14)

Measured 2026-10-11 on the same machine, with release builds of `2badb88` (before) and the 2.14 change (after), by the same method as above (peak working set and peak commit of the exited child). Every run is `-S 127.0.0.1,1 -l 1 -i file`, so a run that passes the guard ends at the connect (about 1 s of timeout in its wall time); a refused run ends before any connect. The inputs are at the 10 MB limit.

| Input | Before | After |
|---|---|---|
| 10,000,000 bytes of `(` (one-character tokens) | 793 MB working set (1,181 MB commit) | **36 MB** (37 MB) |
| 9,999,999 bytes of `SELECT 1` lines | 233 MB (334 MB) | 36 MB (46 MB) |
| `SELECT ` and 3.3M two-letter words | 250 MB (352 MB) | 36 MB (37 MB) |
| 3.3M `#a,` in a `DROP TABLE` list | 442 MB (640 MB) | 36 MB (37 MB) |
| 9,999,000 `(` and a `DELETE` on line 2 (refused), wall time | 793 MB, 0.45 s | 36 MB (47 MB commit), 0.17 s |
| two-letter words (refused at the first word), wall time | 250 MB, 0.40 s | 36 MB, 0.05 s |
| `EXEC a.a.a. ...` (a 10 MB name, refused; the message quotes it, the output cuts it to 1 MB) | 858 MB (1,253 MB), 1.3 s | **54 MB** (65 MB), 0.45 s |
| the same inputs with `--allow-write`, so no guard | 36 MB | |
| `sqlh --version` | 7 MB | |

So a run costs what the input costs without the guard (the text as read, as decoded, and as split into batches: about 3.5 times the input), and the guard adds nothing that grows. The one input shape that still adds is a refusal that quotes a long name, which builds the message from it (the name, the formatted reason, and the copies the output makes before it cuts to the budget).

**What changed.** `tokenize` is gone. A `Lexer` hands out `Token`s one at a time: a view of the input (kind, the text, the name to compare by, the line). A `Lexer` is a position (the input, a byte offset and a line), so a lookahead (the target after a `TOP` clause, the words after `CREATE`, the parts of a procedure name, the arguments of `sp_spaceused`) clones it, reads on, and drops the clone. The uppercased `String` per token is gone: words compare with `eq_ignore_ascii_case`, and the main loop's `match` on keywords uppercases a word into a 16-byte stack buffer (a longer word can't be a keyword it acts on). A dotted name is walked as an iterator; of its parts only the first three are kept, for the qualifier rule. The `--check` NOEXEC search and the write classifier each make their own pass, which keeps the order of their refusals.

**Same decisions.** The classifier's logic is the old logic with positions in place of indexes. Checked by running the old guard (`2badb88`) and the new one over 8 million random inputs (3 million, then 5 million, with another seed) under three policies, from random pieces and from statement-shaped templates in random case (INSERT, UPDATE and DELETE with `TOP`, CREATE/DROP/TRUNCATE/ALTER, EXEC with qualified names and `sp_spaceused` arguments, DBCC, BEGIN, NEXT VALUE FOR, labels, comments, unterminated strings): the line, reason and advice were identical in every case, with about 16% of them allowed. That harness needs a copy of the old guard, so it is not in the tree. The unit tests of the old guard pass unchanged; new ones pin the lexer's tokens and lines, the lookahead positions, the keyword buffer (and that every list is upper case and fits it), every list matching in any case, words longer than the buffer, the line of each kind of refusal, and inputs of 200,000 tokens in the shapes that were expensive. One quirk was kept as it was: an `EXEC` with nothing after it was reported on line 1, whatever its line, because the refusal looked the line up on the token that should follow (fixed in R18, below).

**Cost in time.** Flat inputs got faster (the allocation per token is gone): the refused runs above take a third to a tenth of the time. Deeply nested lookahead got slower. A `TOP ( ... )` clause, an `sp_spaceused` argument list inside parentheses and a long name are scanned again from each statement keyword in them, so nested statements cost time quadratic in the depth, as before (the old guard had the same shape); the constant is about 16 times higher now, because the scan lexes again where it used to step through a vector. `UPDATE TOP (UPDATE TOP (... @t) @t) @t` nested 2,000 deep takes 0.19 s (0.025 s before), 5,000 deep 1.1 s (0.07 s), 10,000 deep 4.3 s (0.25 s), 20,000 deep 17 s (1.0 s). Nothing a person or an agent writes is nested a hundred deep. A bound on the guard's work would remove the corner; it was made later, as R18 (below).

## The read-only guard's work (R18)

The guard's memory was fixed above; its time was not. Classification runs on the whole input before anything connects and before the signal listeners exist, so `-l`, `-t` and Ctrl+C don't reach it, and the nested shapes above were still quadratic: the lookahead from each `UPDATE` scanned to the end of its `TOP ( ... )` clause again. The review (R18) measured `UPDATE TOP (` repeated N times, `@t`, then `) @t ` N times at 166 ms for N = 2,000 up to 9.6 s for N = 16,000, input that passes the classifier, that no server accepts, and that is far under the 10 MB cap.

Measured 2026-10-11 on the same machine (shared with other work, so repeat runs vary by about 30%), release builds. "Classify" times `guard::check` in process, with the guard of `3ef8f3c` ("before") and the one of this change ("after") compiled side by side in a scratch program. "Process" is the whole `sqlh.exe -S 127.0.0.1,1 -l 1 -i file` run, ended at the guard's refusal (a `DELETE FROM dbo.T` line is appended so that the run is refused instead of waiting for the connect), measured with PowerShell's `Measure-Command`.

| Input (nested N deep) | Bytes | Classify before | Classify after | Process after |
|---|---:|---:|---:|---:|
| N = 2,000 | 34,000 | 215 ms (179 ms) | 0.5 ms | 23 ms |
| N = 4,000 | 68,000 | 949 ms (736 ms) | 1.0 ms | 18 ms |
| N = 8,000 | 136,000 | 3.4 s (2.7 s) | 2 ms | 18 ms |
| N = 16,000 | 272,000 | 15.5 s (10.8 s) | 3.6 ms | 23 ms |
| N = 20,000 | 340,000 | not run (17 s in the section above) | 4 ms, refused | 26 ms |
| `EXEC sp_spaceused (` x 4,800 | 91,000 | 1.06 s | refused, 0 ms | |
| `EXEC sp_spaceused (` x 20,000 | 380,000 | 19.9 s | refused, 0 ms | |
| `EXEC sp_spaceused (` x 500,000 | 9,500,000 | hours (quadratic) | refused, 0 ms | 46 ms |
| `DELETE TOP (` x 833,000, never closed | 10,000,000 | | refused, 100 ms | 106 ms |

The numbers in parentheses are a second run of the "before" guard. The nested cases up to 16,000 deep are classified as before (allowed); N = 20,000 and the `sp_spaceused` shapes are refused with the new message.

**What changed.**
- **`TOP (` clauses, one pass.** `skip_top` is gone. An `INSERT`, `UPDATE` or `DELETE` followed by `TOP (` enters a small record (its kind, line, position and parenthesis depth) in a stack, and the check of its target is made when the main pass reads the clause's closing parenthesis, the same way it would have been made by scanning ahead to it. Refusals found in between are held until the clauses that began before them are closed, so the refusal given is still the earliest keyword's. The stack is capped at 16,384 entries (24 bytes each, 393 KB at most); one more is refused. The 20,000-deep input peaks at about 8 MB.
- **`sp_spaceused`'s arguments.** The scan stops as soon as the answer is known (the second positional argument is enough), and a scan that reads more than 8 parentheses refuses the input. The procedure takes no parentheses in its arguments, so no real call has any. Why 8 bounds the work: a scan that runs on past another call's `EXEC` (a statement keyword, which ends the scan at parenthesis depth 0) needs a parenthesis open there, so a scan that reaches the k-th call after it has read a parenthesis from each of the calls in between; with 8, no token is read by more than 9 scans. A cap on the nesting depth would not do this: `EXEC sp_spaceused ) (` repeated holds a parenthesis open at every call, at depth 1, because a `)` at depth 0 is a no-op, and every scan would read to the end of the input. The limit counts the parentheses read, not their depth.
- **Names.** The three reads of a procedure name (the allowlist, the qualifier rule, the refusal's message) stay. A name that passes the qualifier rule has at most three parts, and one that does not is refused after those reads, so an `EXEC` inside a name is never reached to read the name again. No change.
- **`DROP TABLE` lists.** The loop read each name's neighbors three times; it reads them once (7 reads a name became 5).
- **The work budget.** Every `Lexer` made by the classifier is charged to a shared counter: one unit a token read, lookahead included, and one more for each 32 bytes the read went over. A pass gets 2,000,000 plus 3 a byte; when it is spent the lexers report the end of the input, and the pass's answer is replaced by the refusal. The counter is the proof that repeated reading is counted: unit tests read the same text twice and check that it is charged twice. No input found uses more than 1.7 units a byte (below), so the budget is a backstop for a shape not yet thought of. `--check` makes a second, separate pass (the NOEXEC search), with a budget of its own.
- **The refusal** is exit 5 with `this input is too complex for the read-only check: <why>` and advice to simplify, split with `GO`, or use `--allow-write`; DESIGN.md §7 has it whole.
- **The `EXEC` line.** `check_proc` took the line from the token after the name (or line 1 when the input ended), so a final `EXEC` on line 3 was reported on line 1 and an `EXEC` whose name was on the next line on that line. It now takes the `EXEC` keyword's line.

**Same decisions.** The old guard (the one of `3ef8f3c`) and the new one were run over 8 million random inputs in 4 seeds, plus 7 million earlier in the work, under three policies, from random pieces and from statement-shaped templates in random case, including nested and repeated `TOP (` clauses with and without targets, procedure names and arguments, unterminated pieces and comments between lines (the harness is not in the tree). The reason and the advice were identical in every case. The line was identical in every case but the `EXEC` ones (about 0.5% of the inputs, all containing the word and all an `EXEC` whose line moved to its keyword). The existing guard unit tests pass unchanged except the assertion that pinned the old line.

**Cost of the budget and the other shapes.** At the 10 MB limit, `guard::check` in process, before and after, in ms, with the units the work counter reached (per byte) after:

| Input (about 10 MB) | Before | After | Units a byte |
|---|---:|---:|---:|
| `(` x 10,000,000 | 121 | 129 | 1.00 |
| `SELECT 1` lines | 62 | 68 | 0.22 |
| `DROP TABLE #a,#a,...` (3.3 million names) | 300 | 222 | 1.67 |
| `EXEC sp_who;` x 769,000 | 214 | 227 | 0.75 |
| `INSERT #t ` x 1,000,000 | 108 | 110 | 0.50 |
| `UPDATE TOP (1) #t SET x=1;` x 380,000 | 144 | 141 | 0.62 |
| `CREATE INDEX a ON #t;` x 470,000 | 259 | 199 | 0.76 |
| `EXEC sys.sp_who,` x 620,000 | 247 | 262 | 0.94 |
| `EXEC sp_spaceused (1);` x 450,000 | 227 | 233 | 0.73 |
| `EXEC sp_spaceused (` and `)` nested 4 deep, balanced, repeated | | 326 | 1.03 |
| `UPDATE TOP (((...x...))) #t ` with 10,000 parentheses, repeated | | 128 | 1.00 |
| a 10 MB string / a 10 MB comment | 11 / 51 | 25 / 25 | 0.06 / 0.09 |

So the flat shapes cost what they did (the counter adds a few percent), the slowest takes a third of a second, and no shape found uses more than 1.7 units a byte, against 3 a byte (plus 2 million) allowed: at most 32,000,000 units for a 10 MB batch. The classifier did 75 million units a second on the fastest shape (the `DROP TABLE` list) and 31 million on the slowest (the nested `sp_spaceused` calls), so a batch that spent its whole budget would take 0.4 to 1 s: the bound on the time, in the unreachable case. The process tests (`tests/cli.rs`) bound a refused or classified nested input at 5 s, headroom for debug builds and loaded machines; `guard.rs` tests count units instead of time.

Memory is unchanged. The process memory test now also runs 10 MB of `DELETE TOP (` that never close (the stack fills to its cap: about 36 MB peak working set, sampled) and 9.5 MB of `EXEC sp_spaceused (` (refused at the first call: about 23 MB).
