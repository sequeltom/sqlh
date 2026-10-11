# Memory (PLAN.md 2.7)

Measured 2026-10-11 on the dev machine (Windows 11, 12 logical cores, 31 GB) against the owner's dev/test SQL Server 2025 over Wi-Fi, with release builds of two trees and mssql-tds 0.2.0: **before** is `f4a6610`, **after** is the 2.7 change. Large values are built on the server and read-only, for example `REPLICATE(CAST('x' AS varchar(max)), 1073741824)`. Peak working set and peak commit come from the exited child's process handle (`GetProcessMemoryInfo`), CPU time from `GetProcessTimes`, so nothing is sampled. The wrapper that did this was a short program kept outside the repository; `run_measured` in `tests/cli.rs` does the same for the tests. A run of `sqlh --version` peaks at 7 MB; a run that connects peaks at 9.6 MB of working set and 1.9 MB of commit before it has read anything. The exe grew from 2,285,568 to 2,341,376 bytes (2.4%).

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

- **The read-only guard is not bounded by the input limit.** `guard::tokenize` makes a `Token` of some 56 bytes (a `&str`, an uppercased `String`, a kind and a line) for each token of the input, in one `Vec` that grows by doubling, so a run's peak depends on what the input looks like, not on its size: 10 MB of `(` peaked at 795 MB working set (1.18 GB commit); 9.9 MB of `SELECT 1` lines, 233 MB; 10 MB of two-letter words, 250 MB; 9 MB of `$(v)` references to a 9-byte value, 30 MB. The 10 MB cap bounds accepted SQL, not what it costs. A fix is outside this task (`src/guard.rs`): tokenize a statement at a time, or keep the kind and the span and uppercase on demand.
- **Abandoning a result set is not flat in the driver.** `drain_rows`, behind `advance` and `close_query`, reads the rest of a result set with `next_row_into(DiscardRowWriter)`, which allocates each PLP value whole. (`drain_result_set_rows`, which `drain_stream` uses after an error, skips with `ColumnPolicy::SkipAll` and doesn't.) sqlh's batch loop reads every result set to its end, so it doesn't reach `drain_rows` for rows; a future early cutoff (the drain policy, PLAN.md 2.6) would, and would need the cursor skip instead.
- **A cancel during the new read** ends the run like any other: Ctrl+Break while a 1 GiB value was streaming, and while rows of 400 MB were being skipped, gave exit 6 within 0.2 to 0.5 s with "the statement was cancelled; the connection stayed usable". The query timeout does too (`-t 12` on the 1 GiB value: exit 4; the driver then took 5 s to settle the attention, in the old build as well).

## Upstream issues worth filing

Read in 0.2.0; check `main` (0.3.0) first. Upstream takes issues, not PRs.

1. **`DiscardRowWriter` allocates what it discards.** A `RowWriter` can't decline a PLP value without the decoder allocating its declared length (`value_destination` has no way to say "skip"), and `DiscardRowWriter`, `drain_rows` and every consumer that wants a prefix of a large value pay for all of it. A skip or a limit in the writer's contract, or `drain_rows` using `ColumnPolicy::SkipAll` as `drain_result_set_rows` does, would fix the common case.
2. **`text`, `ntext` and `image` have no streaming path**: the pull cursor returns them as `CursorColumn::Value`, fully decoded, and the decoder refuses over 100 MB (`MAX_ALLOC_SIZE`), a limit the PLP types don't have.
3. **`SqlString::decode` panics on malformed UTF-8** under a UTF-8 collation (`String::from_utf8(...).unwrap()`, the driver's own #310). The cutter decodes with replacement, so a malformed value now shows U+FFFD where the old path would have panicked.
