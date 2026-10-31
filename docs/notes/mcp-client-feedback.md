# MCP client feedback, October 31, 2026

The owner ran `sqlh mcp` (`0.1.0-dev.5`) from Claude Desktop against a real environment, twice: once in chat and once in Cowork. Each session's model wrote up what it found. This note keeps what matters to sqlh, with the server, listener and login names left out. PLAN.md, "MCP client feedback (October 31, 2026)", tracks each item; decision 45 came from it.

## The environment

- **Server:** SQL Server 2016 SP3 (13.0.6500), Enterprise, in an availability group, reached through its listener. This is outside the first-release target (SQL Server 2025), and everything ran, `describe` included, on the candidate 2016 floor (PLAN.md Phase 7).
- **Login:** a SQL login, read-only on that server.
- **Connection:** the tools ran through the desktop bridge (`mcp__remote-devices__sqlh__*`).
- **Speed:** `info` took 859 ms, 585 of it the connect. The link is slower than the dev LAN that measured decision 6.

## What both reported

1. **A capped result cancelled the later statements of its batch while the status line said `ok`.** Three statements, no `GO`: the first was capped at 100 rows, the count stopped (`≥2000`), and the other two never ran. The note said so, but the last line read `-- ok: 1 batch, counting stopped in 1`. The chat session's model called it "the only issue that makes a model confidently report something false". The Cowork session's model caught it, but only from the note.
   - The count stopped at about 600 rows of `msdb.dbo.sysjobhistory` (a wide `message` column): over this link the 250 ms budget ends after modest results, not only huge ones.
   - **Done (decision 45):** the 5 s budget where later statements could show something, and `-- partial:` when they still don't run.
2. **`≥N` is a lower bound with no per-call way to make it exact.** The models read `≥` right; `exact_count` existed only as a profile key. **Done:** `query` takes `exact_count`.
3. **The defaults weren't in the schema.** The 100 rows and 500 characters showed only in the output. **Done:** each tool's `max_rows` and `max_cell` descriptions give its default.
4. **`NT SERVICE\\SQLSERVERAGENT`** was read as an escaping artifact. It is the documented escaping (decision 11: a value can be decoded exactly). **Done:** the `query` description and the instructions say `\\` is one backslash.
5. **`max_rows` and `max_cell` on `info` do nothing.** Half right. **Done:** `info` no longer takes `max_rows`, since it returns one row. `databases` keeps it, since it returns a row per database. `max_cell` was dropped too at first, but a profile's or the launch's small `max_cell` still cuts `info`'s values, so it is back (review R49).
6. **`check`'s `sql` said "The T-SQL to run".** **Done:** "The T-SQL to compile (not run)."

## What one reported

- **No paging for `definition`.** A 63,000-character procedure arrived whole, but Claude's tool-output limit moved it to a file the model had to read separately. Planned: a line range for `definition`.
- **`info` named a node, not the listener.** The model couldn't tell whether it had connected through the listener. Planned: the name dialed, the availability group role, and a named time zone where the server has one (`CURRENT_TIMEZONE()`, SQL Server 2019 and later).
- **A machine-readable status line** (`complete=false rows_shown=...`). Declined: `format: json` already is that per call, and the `query` description now says so.
- **Cut markers in a footer instead of in the value** (`…[+3498]` moved out). Declined: a footer costs tokens and separates the mark from its value, and the models read it right in agent trial 3 and here.
- **The `definition` line count** (1,677 against 1,678 line feeds). Not a defect: a trailing line break makes no empty last line (DESIGN.md §5.1), and the other session counted 1,677 and found the source complete.
- **No time zone on `sysjobhistory` times; no timing for the client's tool search.** These come from the data and the client, not sqlh.

## What both said to keep

The read-only guard's refusal (clear, nothing run, exit 5, the whole batch blocked), the timing with the connect shown apart, and `describe`.
