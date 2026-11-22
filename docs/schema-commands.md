# Schema commands: design and plan

Status: built 2026-10-13 (`src/schema.rs`; plan-history.md Phase 4), `deps` on 2026-10-14 (§6.7), the server command `errorlog` on 2026-11-21 (§6.9; Phase 6 slice 2, decisions 84, 89 and 90), and the server command `activity` on 2026-11-22 (§6.8; decisions 83, 85, 86 and 88). This doc is the spec, with DESIGN.md §5.1 "Sections" and §9. MCP mode exposes the same commands as tools (`docs/mcp-mode.md`).

**As built, where it differs from the proposal below** (the sections below are corrected too):
- **The lookup is not a batch.** `describe` and `def` look the name up first on the run's own connection (`run::Plan::Lookup`), inside the read-only wrapper; the lookup shows nothing, and its rows decide the one batch that follows. So a `describe` is `ok: 1 batch`. An error in the lookup ends the run as a failure quoting the server's message.
- **`def` is escaped, not raw**, framed by its line count (the review's correction: framing that survives control characters): DESIGN.md §5.1 "Sections".
- **Limits:** `describe` defaults to 2,000 rows a section and 4,000 characters a value; `def` to no cell limit, so `--max-output` bounds it. A flag wins; a profile's `max_rows`/`max_cell` don't apply to these two (they were chosen for queries). `tables` and `find` keep the run's limits.
- **Resolution** (§5): an explicit schema is never set aside, and the fallback is case-insensitive, so it is defined for case-sensitive databases (the review's corrections). Name errors are exit 1; another database or a linked server is a usage error (exit 3).
- **`tables`** matches the pattern against the bare name or `schema.name`, so `Ord*` and `sales.*` both work.
- **Patterns** are bracket-escaped (`[%]`, `[_]`, `[[]`), not `ESCAPE N'\'`, so the same pattern works in `PATINDEX` for `find --text`.
- **Hidden metadata is marked:** a default, computed column or check constraint whose definition the login can't see shows `(hidden)`, not NULL. Parameters show `output` and `readonly`, not `has_default`, which the catalog only keeps for CLR objects.
- **Synonyms:** `describe` shows the `object` section with `base_object`; it doesn't describe the target (run `describe` on it). `def` of a synonym says what it points at.
- **Temporary tables** are refused before connecting: each run is a new session, so `#t` is never visible (§8).
- **The first review's corrections (R27–R33, 2026-10-14):** names are parsed by sqlh before connecting, each part checked, and the lookup gets the parts as separate literals (§5); whether a database part is the current database is SQL Server's decision (`DB_ID`); the lookup returns at most 11 candidates and their total, and keeps a bounded reply (`run::Capture`); every name sqlh shows is bracketed when it must be (§5 "Names shown"), and lists use `STRING_AGG` (§3); `def` checks the definition it reads (§6.6); `find --text` doesn't search bodies the login can't see (§6.5); a definition cut while read says by what (DESIGN.md §5.1); B-tree indexes have keys and includes, other index types list their columns as keys (§6.4).
- **The aggregation threshold (R37, 2026-10-18):** `describe` aggregates with `STRING_AGG` from compatibility level 110, and with `FOR XML PATH` only at level 100 (§3).
- **SQL Server 2016 (the owner's benchmark, 2026-10-31):** `describe` raised seven error 195s on a SQL Server 2016 server, which has no `STRING_AGG` at any compatibility level; the other schema commands worked there. The lookup now also returns `SERVERPROPERTY('ProductMajorVersion')`, and `STRING_AGG` is used only from version 14 (SQL Server 2017) at level 110 and up (§3). Checked by unit tests and live on SQL Server 2025, and live on 2016 SP3 on 2026-11-01 (decision 49).

Product direction updated 2026-10-10: [VISION.md](VISION.md) prioritizes concise schema discovery for agents with limited context working through human-led DBA/developer questions. The first release targets SQL Server 2025 Standard on Windows Server 2025 Standard. A 2016 floor is a candidate expansion, conditional on acceptable technical debt.

## 1. Why

Before an agent writes a query it needs the schema: which tables exist, their columns and types, keys, and how tables relate. Today it has to write catalog queries itself, and those go wrong in predictable ways:
- catalog queries may assume functions or metadata absent on an older compatibility target
- the length of `nvarchar` in `sys.columns` is in bytes
- `sp_help` output is long and padded
- several round trips are spent for each table

The schema commands give compact, correct answers in one call each, reducing repeated query-writing and context overhead. Validate them on the first-release target; expand their compatibility only when that support is accepted. Every output format applies to them.

## 2. Commands

| Command | Answers |
|---|---|
| `sqlh info` | Server, version, edition, database, login, user, transport, auth scheme, server time, server permissions (§6.1); encryption as a note. Same as `profiles test`. |
| `sqlh dbs` | The databases this login can see, with state and access. |
| `sqlh tables [pattern]` | Tables and views with estimated rows. |
| `sqlh describe <object>` | Everything needed to query a table or view, or call a procedure or function. |
| `sqlh find <pattern> [--text]` | Objects and columns whose names match. With `--text`, also procedure, view and function bodies that contain the pattern. |
| `sqlh def <object>` | The source of a view, procedure, function or trigger. |
| `sqlh deps <object>` | What the object references and what references it. |
| `sqlh activity` | A server command (§6.8): the sessions running, blocked, blocking or idle in an open transaction, with their chains. |
| `sqlh errorlog [--since T] [--search TEXT] [--agent] [--archive N]` | A server command (§6.9): the SQL Server error log, or the SQL Server Agent log, newest first. |

**Invocation.**
- The command is the first positional word after the connection options: `sqlh --profile test describe dbo.Users`.
- Only these names count as commands. A query given with `-Q`, `-i` or stdin is unaffected, and none of these words is valid T-SQL on its own, so nothing that worked before changes meaning.
- `-Q`/`-i` can't be combined with a command.
- `--format`, the limits and `-o` all apply.
- A command's own options (`--text` for `find`; `--since`, `--search`, `--agent`, `--archive` for `errorlog`) can come anywhere on the line, and reach the command through one struct (`schema::Flags`). With another command, or with SQL, each is a usage error (exit 3) before anything connects: `--text only applies to sqlh find`, `--since only applies to sqlh errorlog` (and so on), `--search only applies to sqlh errorlog and sqlh profiles list` (whose own `--search` is parsed apart).

**Scope.** Schema commands cover the current database, set by `-d` or the profile; a server command (`activity`, `errorlog`) reads the server, whatever the database. Cross-database names (`OtherDb.dbo.T`) are refused with a hint to use `-d`. Catalog views are per database, and Azure SQL DB doesn't allow cross-database queries at all.

## 3. How they run

Each command is canned T-SQL that goes through the normal pipeline: the session setup, the read-only wrapper transaction, the query timeout, the output formats, error mapping and exit codes. Implementing a command mostly means writing SQL.

- **Trusted SQL skips the classifier.** The SQL is written by the tool, not the agent, so the keyword classifier (guard layer 1) is skipped for it. The rolled-back wrapper transaction (layer 2) still applies.
- **Arguments are bound as literals, never spliced into identifiers.** User arguments become `N'...'` literals in the batch (a name's parts as `DECLARE @db sysname = ..., @schema sysname = ..., @object sysname = N'...';`, NULL for an omitted part), with single quotes doubled. That is the only escaping an `N'...'` literal needs, and it is unit-tested with hostile names.
- **Two steps for object commands.** `describe`, `def` and `deps` first look the name up (a lookup on the same connection that shows nothing) returning `object_id`, the schema-qualified name and the type; sqlh, not the SQL, raises the name errors (§5). The batch that follows is chosen by type, so its sections are known in advance. That costs one extra round trip, about 1–5 ms on a LAN.
- **A lookup can do a command's server work** (`run::Plan::Lookup`, Phase 6 slice 2). It may hold several statements and fill `#temp` tables, which the step after it reads (same session, same wrapper), so a command whose step is one `SELECT` keeps `guard::several_outputs` false for it: a capped result gets the brief count, never the 5 s longer count or `-- partial:` advice about `GO` batches, which a command's user has no way to act on (decision 45; tested by `run::tests::a_step_of_one_select_after_a_lookup_has_no_later_statements`). A lookup's own failures name what it was doing (`run::LookupLabel`): the name commands' `query timeout after N s while looking the name up`, `looking the name up failed: error N: ...` and `the name lookup returned more than it should; nothing was shown`, unchanged. A lookup's `then` refuses what it found with exit 1 and `-- error: ...` (`schema::command_error`), as a name error is.
- **Compatibility is deliberate.** The sketches use property functions for some metadata to retain inexpensive portability; aggregation uses `STRING_AGG` (SQL Server 2017+), an accepted cost (see the next point). They are proposals, not a requirement to maintain one query across every historical SQL Server version. Metadata semantics are validated on SQL Server 2025 and on 2016 SP3, which is supported for reading (decision 49).
- **Aggregation.** Lists such as index key columns use `(SELECT STRING_AGG(CAST(item AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY ...) FROM ...)`. As first proposed they used `STUFF((SELECT N', ' + ... FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')`, which fails on a name with a control character (XML can't carry it); `STRING_AGG` takes any character. SQL Server 2016 has no `STRING_AGG`, so there `FOR XML` is the only form, and such a name makes `describe` fail after its columns, with error 6841 (live test `describe_shows_any_name_from_compatibility_level_110`); an ordinary name is described in full. Each column in a list is shown as a name is (§5), so `[a, b]` is one column and `a, b` two. **By compatibility level** (found 2026-10-17, while building 5.3: `describe` failed on every table in StackOverflow2010, at level 100, which refuses `WITHIN GROUP`): the lookup returns the current database's level and the server's major version (`SERVERPROPERTY('ProductMajorVersion')`, since 2026-10-31: SQL Server 2016, version 13, has no `STRING_AGG` at any level and raised error 195), and `describe` uses `STRING_AGG ... WITHIN GROUP` from version 14 (SQL Server 2017) at level 110 and up, where SQL Server takes it (Microsoft's STRING_AGG remarks), and the ordered `FOR XML PATH` form only at 100, the lowest level SQL Server 2025 has, and when the lookup returns no level (`schema::Aggregate::for_level`; R37, `2504b7c`). The cast to `nvarchar(max)`, the explicit ordering and the name quoting are the same in both. Only at level 100 does a name with a character XML can't carry (a control character such as U+001B) make `describe` fail (error 6841). Checked live at 110 and 120 by `describe_shows_any_name_from_compatibility_level_110`, which sets the scratch database's level (owner decision 21, PLAN.md §6) and describes a table whose indexed column is `[a<U+001B>b]`, and at 100 with an ordinary table's ordered keys, includes, foreign key columns and trigger events. From `d6237da` (2026-10-17) to `2504b7c` the line was 130, which brought the XML failure back at 110 and 120 (the external review's R37, plan-history.md, Phase 5.2 and 5.3 review fixes).

## 4. Sections: an addition to the output contract

A command returns several result sets, and each gets a **label** in place of a number. The text format (DESIGN.md §5.1) gains these rules for labeled result sets:

- `-- <label>` replaces `-- result N`.
- The header has **column names only**. The types would be the catalog query's own (`nvarchar(4000)` everywhere), which is noise.
- **Empty sections print nothing.** One closing line lists them: `-- no rows: referenced_by, check_constraints, triggers`. The header is held back until the first row arrives, so an empty section costs nothing.
- `def` prints its definition framed by a count: `-- definition: dbo.usp_GetUser (SQL_STORED_PROCEDURE), 42 lines`, then exactly 42 lines, each escaped like a value except that a tab stays a tab (DESIGN.md §5.1). The count tells a reader where the block ends, even though T-SQL comments inside it start with `-- `.
- A section's footer is written only when it didn't show everything (rows hidden, values cut).

In the other formats:
- **json:** result events gain `"label"`. Empty sections are kept, with `"rows":[]`, so the schema is stable. A `def` definition is a normal string value, cut like any value (`--max-cell`, or the output budget when there is no cell limit; DESIGN.md §5.2).
- **csv:** the label appears in the stderr marker (`-- columns: column, type, ...`), empty sections are skipped, and a definition is one quoted field.

The mechanism is `Output::set_sections(&[Section])`, called before a command's batch begins. `Tally::start_result` takes the next section (`Section { label, definition }`); every section has a names-only header and collapses when empty, and a `definition` one prints as a counted block.

**Limits.** `describe` defaults to 2,000 rows a section and 4,000 characters a value, and `def` to no cell limit, because a 150-column table or a long procedure is the answer, not noise; `--max-output` still bounds the whole response (the review's correction: a high default under the response budget, not unlimited). Explicit flags still apply. `tables` and `find` keep the normal limits, and the footer tells the agent to narrow the pattern.

## 5. Name resolution (`describe`, `def`, `deps`)

0. **sqlh parses the name before connecting** (`schema::parse_name`, R27). A name has at most four dot-separated parts, `server.database.schema.object`. A part is plain (no `[`, `]` or `"` in it) or delimited: `[...]` with `]]` for a `]`, or `"..."` with `""` for a `"`, so a delimited part can hold dots and commas. A part left empty is omitted (`AppDb..Users`, `.Users`), except the object's. A part longer than 128 UTF-16 units (SQL Server's limit for an identifier), an empty or unclosed delimited part, text after a closing delimiter, a plain part with a delimiter character, or a fifth part is a usage error (exit 3), before anything connects: an invalid part is never taken for an omitted one, so it never widens the lookup. A `#` object is a temporary table (§8), refused with exit 1; a server part is refused with exit 3.
1. **The database part** is compared with the current database once, by SQL Server: `DB_ID(@db) = DB_ID()` (R28). `DB_ID` compares database names as the server does (its collation, not the database's), so an alternate-case spelling of the current database is accepted on a case-insensitive server, and a name the server takes for another database is refused, `run with -d Other` (exit 3); a name that is no database is refused too (`there is no database X on this server`, exit 3). The candidates below are only sought when the database part is the current database.
2. Try `OBJECT_ID` of the parts, each `QUOTENAME`d (`[schema].[object]`, or `[object]` alone). An unqualified name resolves in the user's default schema, in the database's own collation. System objects resolve too (`sys.objects`).
3. If that fails, look for top-level objects with that name (compared with `COLLATE Latin1_General_100_CI_AS`): in any schema for an unqualified name, and only in the schema given for a qualified one, compared the same way. An explicit schema is never set aside. If exactly one exists, use it: agents often write `Users` when the table is `sales.Users`, and in a case-sensitive database `users` still finds `Users`.
4. If several exist (two schemas, or two names that differ only in case), the run fails: `ambiguous name Users: archive.Users, sales.Users; qualify it with the schema` (ten names at most, then `and N more`). The lookup returns at most 11 candidates (`TOP (11)`) and their total (`COUNT(*) OVER ()`), and sqlh keeps a bounded reply: four result sets, 16 rows each, 4,096 characters a value. A reply past those bounds, or a total that doesn't fit the rows, fails the run (exit 10) rather than being taken for an answer (R31).
5. If none exist: `no object named Usrs in database AppDb; try: sqlh find Usrs`.

Through `sqlh mcp` (mcp-mode.md §4) the hints name what a tool call can change instead of a flag (`output::Advice`): `call with database Other` for `run with -d Other`, `try: find with pattern Usrs` for `try: sqlh find Usrs`, and a server part's refusal ends `(the profile's)` where the CLI's ends `(-S)`. The limits a section's footer or a cut definition's header names follow the same rule: `(max_rows 100)`, `(the server's --max-output, max_cell 4000)` (DESIGN.md §5.1). So do `def`'s two refusals (§6.6, a table and a synonym; `schema::def_steps` takes the `Advice` since `05032c0`): through the `definition` tool they name the tools and their argument, `for a table use: describe with object X` and `call definition with object B`. A name lookup that times out says `raise timeout (the server allows up to S s)` below the server's limit and `N s is the server's limit` at it, where the CLI says `raise -t` (mcp-mode.md §4).

Name errors end the run as a failure (exit 1, `-- error: ...` in text, the document's `error` in JSON).

**Names shown** (R32). Every schema-qualified name sqlh shows (`tables`, `find`, the `object` section, `references` and `table` in the foreign-key sections, the `def` header, and the names in errors and hints) has each part shown as it is when it is ASCII letters, digits and `_ @ # $`, starting with a letter or `_`, and otherwise in brackets with `]` doubled, as `QUOTENAME` writes it: `sales.Users`, `[s.a].b`, `s.[a.b]`, `dbo.[Order Details]`. The same rule is applied in SQL (`schema::shown`) and in Rust (`schema::quoted`). So a shown name can be given back to `describe` or `def` as it is, and two names never look alike because of a dot in one. Column lists (index `keys` and `includes`, foreign-key `columns` and `ref_columns`) bracket each column the same way: `[a, b], [c DESC] DESC` is two columns, the second descending. A name in its own field (a column's `column`, an index's `name`) is shown as it is: the field holds one name. Reserved words aren't bracketed (`select` stays `select`): sqlh's own parser takes them, and a shell doesn't care. A name with a non-ASCII letter is bracketed, which is conservative.

## 6. Command details

All SQL below is a sketch of the intended queries; the implementation fixes the details. `<type>` stands for the shared type-formatting expression:

```sql
CASE WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary')
          THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length AS nvarchar(10)) END + N')'
     WHEN t.name IN (N'nvarchar', N'nchar')
          THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length / 2 AS nvarchar(10)) END + N')'
     WHEN t.name IN (N'decimal', N'numeric')
          THEN t.name + N'(' + CAST(c.precision AS nvarchar(3)) + N',' + CAST(c.scale AS nvarchar(3)) + N')'
     WHEN t.name IN (N'datetime2', N'time', N'datetimeoffset')
          THEN t.name + N'(' + CAST(c.scale AS nvarchar(3)) + N')'
     ELSE t.name END
```

Alias types (`CREATE TYPE Phone FROM varchar(20)`) show their alias name; `describe` adds the base type in parentheses.

### 6.1 `info`

The `profiles test` query already in `src/profile.rs`, moved to `src/schema.rs` (`INFO_SQL`), with encryption printed as a note. It is one `SELECT` of one row, so `profiles test`, which runs it as a plain query, shows one result with a typed header, and the MCP `info` tool one row. Its columns: `server, version, edition, database, login, user, transport, auth_scheme, server_time, server_permissions`.

**`server_permissions`** (Phase 6 slice 2, spec §1; `nvarchar(114)` on SQL Server 2025 and 2016) says which server-wide reads the login has, so an agent knows before it tries whether a server-wide read (a server DMV, the error log) will be refused: `sysadmin` alone for a sysadmin (it holds everything); otherwise the held ones of, in this order, `VIEW SERVER STATE`, `VIEW SERVER PERFORMANCE STATE` and `VIEW SERVER SECURITY STATE` (these two only when VIEW SERVER STATE isn't held, since it implies them), `VIEW ANY ERROR LOG`, `VIEW ANY DEFINITION`, `securityadmin`, separated by `, `; `none` when it holds none of them. Each is the server's own answer for the caller (`HAS_PERMS_BY_NAME(NULL, NULL, N'...')`, `IS_SRVROLEMEMBER(N'...')`), compared with `= 1`: a permission the server doesn't know (the SQL Server 2022 ones on 2016, where `HAS_PERMS_BY_NAME` gives NULL) is not held. `securityadmin` is listed because SQL Server's `sp_readerrorlog` admits it (Microsoft treats it as near-sysadmin; sqlh detects it, never advises it). `VIEW DEFINITION` on a database is not `VIEW ANY DEFINITION`. Any login can run it; Azure SQL DB is unverified for these functions. Tested: `schema::tests::info_ends_with_the_server_permissions` (the SQL's shape and order) and live `info_names_the_logins_server_permissions` on both servers (the owner's `sysadmin` in `info` and in `profiles test`'s typed header; a reader's `none`; VIEW SERVER STATE, VIEW ANY DEFINITION and securityadmin each alone, and on SQL Server 2022 and later VIEW ANY ERROR LOG, VIEW SERVER PERFORMANCE STATE and VIEW SERVER SECURITY STATE; several at once in the fixed order, VIEW SERVER STATE hiding the two it implies; on 2016 the VIEW ANY ERROR LOG grant refused as a syntax error).

### 6.2 `dbs`

```sql
SELECT name, state_desc AS state, HAS_DBACCESS(name) AS accessible,
       compatibility_level AS compat, recovery_model_desc AS recovery,
       is_read_only AS read_only, collation_name AS collation
FROM sys.databases ORDER BY name;
```

From a user database on Azure SQL DB this shows only `master` and the current database. That's expected; the help text says so.

### 6.3 `tables [pattern]`

```sql
SELECT shown(s.name) + N'.' + shown(o.name) AS name,  -- shown: §5 "Names shown"
       CASE o.type WHEN 'U' THEN N'table' ELSE N'view' END AS type,
       (SELECT SUM(p.rows) FROM sys.partitions AS p
         WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS rows,
       o.modify_date AS modified
FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id
WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0
  AND (o.name COLLATE Latin1_General_100_CI_AI LIKE @pattern
       OR (s.name + N'.' + o.name) COLLATE Latin1_General_100_CI_AI LIKE @pattern)  -- with a pattern
ORDER BY s.name, o.name;
```

`rows` is the partition-stats estimate, so the query is instant on big tables and never scans. Views show NULL.

### 6.4 `describe <object>`

The sections depend on the object's type:

| Type | Sections, in order |
|---|---|
| Table | `object`, `columns`, `indexes`, `foreign_keys`, `referenced_by`, `check_constraints`, `triggers` |
| View | `object`, `columns`, `indexes` (indexed views only) |
| Procedure | `object`, `parameters` |
| Scalar function | `object`, `parameters` (the return type appears as `(return)`) |
| Table-valued function | `object`, `parameters`, `columns` |
| Synonym | `object` (with `base_object`), then the target described in full if it resolves locally |

What each section contains:
- **object:** name, type, estimated rows (tables only), created and modified dates, `MS_Description`, and for tables flags such as `temporal`, `memory_optimized` and `heap`, read with `OBJECTPROPERTY`.
- **columns:** `column, type, null, identity, default, computed, description`. Computed columns show their expression, and defaults show their definition.
- **indexes:** `name, type, unique, pk, keys, includes, filter, disabled`. For a B-tree index (clustered, nonclustered, hash), `keys` are in key order with ` DESC` where it applies, and `includes` are its included columns; `filter` comes from the filtered metadata. Every other type (columnstore, XML, spatial, JSON) has no key order in the catalog (`key_ordinal` 0), so `keys` lists the columns it covers, in column order, and `includes` is NULL. Each column is shown as a name is (§5).
- **foreign_keys:** `name, columns, references, ref_columns, on_delete, on_update, trusted`.
- **referenced_by:** the same, from the referencing side.
- **check_constraints:** `name, definition, trusted`.
- **triggers:** `name, events, instead_of, disabled`.
- **parameters:** `name, type, output, readonly`. Whether a T-SQL parameter has a default isn't in the catalog (`has_default_value` is only kept for CLR objects), so `def` is the way to see it. A scalar function's return type is `(return)`, which the catalog marks as an output.

Example of the intended text output:

```
$ sqlh --profile test describe Users
-- object
name	type	rows	flags	created	modified	description
dbo.Users	USER_TABLE	299398	NULL	2018-06-01T11:02:44.137	2018-06-01T11:02:44.137	NULL
-- columns
column	type	null	identity	default	computed	description
Id	int	0	1	NULL	NULL	NULL
AboutMe	nvarchar(max)	1	0	NULL	NULL	NULL
Age	int	1	0	NULL	NULL	NULL
CreationDate	datetime	0	0	NULL	NULL	NULL
DisplayName	nvarchar(40)	0	0	NULL	NULL	NULL
...
-- indexes
name	type	unique	pk	keys	includes	filter	disabled
PK_Users_Id	CLUSTERED	1	1	Id	NULL	NULL	0
-- no rows: foreign_keys, referenced_by, check_constraints, triggers
-- ok: 1 batch, 31 ms; db01/StackOverflow2010, connect 25 ms
```

### 6.5 `find <pattern> [--text]`

Sections:
- **objects:** `name, type` for every user object whose name matches: tables, views, procedures, functions, synonyms, types and triggers.
- **columns:** `table, column, type` for every column whose name matches.
- **definitions** (`--text` only): `name, type, line, text`. Every module whose body (`sys.sql_modules.definition`) contains the pattern, with the line number of the first match and that line, cut to 200 UTF-16 units (199 where the 200th is the first half of a surrogate pair, so the preview never ends in half a character outside the BMP). The line number is computed in T-SQL by counting `CHAR(10)` before the match. A body the login can't see (encrypted, or without VIEW DEFINITION) is NULL: it isn't searched and has no row, though its name can still match in `objects`; `ISNULL` keeps `PATINDEX` from getting a NULL whatever order the server evaluates the predicates in (R33). Every position function takes the body in one collation (`Latin1_General_100_CI_AI`, not supplementary-character aware), so they all count UTF-16 units alike.

This answers "where is `ClosedDate` used?" in one call.

### 6.6 `def <object>`

```sql
DECLARE @id int = 1234;
DECLARE @definition nvarchar(max) = OBJECT_DEFINITION(@id);
IF @definition IS NULL OR NOT EXISTS (SELECT 1 FROM sys.all_objects AS o
    JOIN sys.schemas AS s ON s.schema_id = o.schema_id
    WHERE o.object_id = @id AND s.name = N'dbo' COLLATE Latin1_General_100_BIN2
      AND o.name = N'usp_Get' COLLATE Latin1_General_100_BIN2)
  THROW 50000, N'the definition of dbo.usp_Get can''t be shown: ...', 1;
SELECT N'dbo.usp_Get' AS name, N'SQL_STORED_PROCEDURE' AS type, @definition AS definition;
```

- **NULL definition:** the run fails (exit 1), `the definition of dbo.p (SQL_STORED_PROCEDURE) is not visible: it is encrypted, it has none (a constraint, a CLR or extended procedure), or the login lacks VIEW DEFINITION`. The lookup checks this, so no batch runs.
- **Checked where it is read** (R30): the catalog can change between the lookup and the read, which the read-only wrapper doesn't freeze. So the read keeps the definition in a variable, and when it is NULL or the object no longer has the schema and name it was looked up by, it fails with error 50000 (exit 1, an error in every format): `the definition of dbo.p can't be shown: since it was looked up, the object was dropped or renamed, or its definition was encrypted or hidden from the login; run the command again`. The row shown carries that same variable, so no later read can differ from the one checked. An empty definition is shown as one (it is not NULL).
- **Tables:** the run fails, `def shows views, procedures, functions and triggers; for a table use: sqlh describe dbo.Users`. Scripting `CREATE TABLE` is out of scope.
- **Synonyms:** the run fails, `dbo.s is a synonym for [dbo].[t]; run: sqlh def [dbo].[t]`.
- Through `sqlh mcp`'s `definition` tool (§5, after step 5) both name the tools instead: `definition shows views, procedures, functions and triggers; for a table use: describe with object dbo.Users`, and `dbo.s is a synonym for [dbo].[t]; call definition with object [dbo].[t]` (`05032c0`; unit test `def_refuses_what_has_no_definition_to_show`).

### 6.7 `deps <object>` (built 2026-10-14)

What the object refers to and what refers to it, one level each way. The name is looked up as for `describe` (§5). Two sections, each `name, type, via, note`, ordered by name:

- **uses:** what the object's code refers to, from `sys.sql_expression_dependencies` (`referencing_id` = the object, or one of its check and default constraints), plus the tables its foreign keys point at and, for a synonym, its target. A table's code is its computed columns and constraints. A reference to the object itself (a computed column naming its own table) is left out.
- **used_by:** what refers to the object: code that refers to it by id (`referenced_id`), code whose reference SQL Server resolves only at run time by the caller's default schema (`EXEC usp_x`, `is_caller_dependent`), matched by name and schema (when given), the tables whose foreign keys point at it, and the synonyms for it.

The columns:
- **name:** the object, shown as every name is (§5 "Names shown"). In `uses`, a reference that SQL Server resolved shows the object it resolved to (`dbo.Orders` for an unqualified `Orders`); one it didn't shows the name as written, each part shown the same way (`master.dbo.spt_values`, `usp_x`). In `used_by`, a reference held by a table's computed column or constraint shows the table.
- **type:** `type_desc` of the object (`USER_TABLE`, `SQL_STORED_PROCEDURE`, ...); for a referenced type, XML schema collection or partition function, that class (`TYPE`, ...); NULL for an object SQL Server didn't resolve.
- **via:** the part of the object that holds the reference, when it isn't the object's own code: `column total` (a computed column), `check CK_x`, `default DF_x`, `foreign key FK_x`. NULL for a module's code and a synonym.
- **note:** what SQL Server says about the reference, NULL when there is nothing to say: `other server`, `other database` (not followed: catalog views are per database), `caller-dependent` (resolved at run time, by the caller's default schema: in `used_by`, a reference that may mean this object), `ambiguous`, `not found` (no such object in this database, or none the login can see; deferred name resolution allows it), and `schema-bound` (the referenced object can't be dropped or altered while this exists: schema-bound views and functions, and constraints and computed columns that call a function). At most one of the first five applies, and `schema-bound` follows it after `, ` when both do.

Column-level detail (which columns a procedure reads) is left out: `sys.dm_sql_referenced_entities` gives it, but it re-binds the module and fails on one with a missing reference; `def` shows the source, and `find --text` finds a column's uses. Only one level is shown. To go further, run `deps` on a listed object in this database; that doesn't work for a `TYPE`, `XML_SCHEMA_COLLECTION` or `PARTITION_FUNCTION` entry (the lookup only finds objects), for a `not found` or `caller-dependent` name (no object to look up, or one the caller decides), or for another database's or server's name (connect there: `-d`, `-S`). Database and server DDL triggers aren't listed (`referencing_class` 1 only).

**Coverage.** The dependency catalog records the by-name references in persisted code (modules, computed columns, constraints) as SQL Server parsed them, not a run-time call graph: dynamic SQL (`EXEC (@sql)`, `sp_executesql`) and application code aren't in it, and SQL Server doesn't keep dependency information for system objects. So an empty `used_by` isn't proof that nothing uses an object; for a change's impact, check `find --text` and the applications too.

**Permissions.** `sys.sql_expression_dependencies` returns no rows at all to a login without VIEW DEFINITION on the database, not even for the objects it can see (checked on SQL Server 2025 with a `db_datareader` user). So `deps` checks it (`HAS_PERMS_BY_NAME(NULL, 'DATABASE', 'VIEW DEFINITION')`, read by the lookup) and, without it, fails with exit 1: `deps needs VIEW DEFINITION on the database, which the login lacks: without it SQL Server shows no dependencies at all. describe shows foreign keys, and def an object's source`. The view's documented prerequisites are VIEW DEFINITION on the database **and** SELECT on `sys.sql_expression_dependencies`. sqlh checks only the first, before running, with its own refusal; a login that lacks the second gets the server's permission error (229), which ends the run as any SQL error does (exit 1, in every format).

`sys.dm_sql_referencing_entities` was not chosen for `used_by` (R34 corrected the reason first recorded). On SQL Server 2014 and later it needs no permission on the referenced object, only VIEW DEFINITION on the referencing ones, and returns what it can see (CONTROL on the referenced object was the 2008–2012 rule). One catalog view serves both directions, with the same id-based edges and flags (`is_caller_dependent`, `is_schema_bound_reference`), and the database-level check above makes its answer complete or refused rather than silently partial.

**Limits:** as `describe`: 2,000 rows a section and 4,000 characters a value by default (§4).

### 6.8 `activity` (built 2026-11-22)

A server command (Phase 6 slice 2; decision 28 as amended by decisions 83, 85, 86 and 88, PLAN.md §6): the server's sessions worth looking at during an investigation, with their blocking chains, as `sp_WhoIsActive` would show them, from DMVs alone. Its evidence is live check 10 and the slice's design review (`artifacts/slice2/`, local; `activity2.sql` is the SQL as first built and `activity3.sql` the SQL below, with the ages clamped and the cycle mark, each run on SQL Server 2025 and 2016 SP3 under a three-session chain, 2026-11-22). No arguments (decision 85: no `--plans`); MCP's `activity` tool is the same (mcp-mode.md §4), with no `database` argument, since it reads the server. Anything after the word is refused before connecting (`activity takes no arguments, not "x"`, exit 3), and so is another command's option (`--since only applies to sqlh errorlog`).

**The lookup** (one batch, `run::Plan::Lookup`, label `reading the server's sessions`; `schema::ACTIVITY_SQL`) takes one snapshot into `#s`:

```sql
SET NOCOUNT ON;
DECLARE @can int = COALESCE(HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER PERFORMANCE STATE'), HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE'));
IF @can = 1
SELECT s.session_id,
       CASE WHEN r.blocking_session_id > 0 AND r.blocking_session_id <> s.session_id THEN r.blocking_session_id
            WHEN w.blocking_session_id IS NOT NULL THEN w.blocking_session_id
            WHEN r.blocking_session_id IN (-2, -3, -4) THEN r.blocking_session_id END AS blocked_by,
       COALESCE(r.status, s.status) AS status, r.command,
       COALESCE(w.wait_type, r.wait_type) AS wait, COALESCE(w.wait_duration_ms, r.wait_time) AS wait_ms,
       COALESCE(NULLIF(r.wait_resource, N''), w.resource_description) AS wait_resource,
       r.cpu_time AS cpu_ms, r.logical_reads AS reads, r.total_elapsed_time AS elapsed_ms,
       CASE WHEN r.session_id IS NULL THEN CASE WHEN s.last_request_end_time > GETDATE() THEN 0
            ELSE DATEDIFF_BIG(ms, s.last_request_end_time, GETDATE()) END END AS idle_ms,
       s.open_transaction_count AS open_tran,
       CASE WHEN t.begun > GETDATE() THEN 0 ELSE DATEDIFF_BIG(ms, t.begun, GETDATE()) END AS tran_ms,
       s.login_name AS login, s.host_name AS host, s.program_name AS program,
       DB_NAME(COALESCE(r.database_id, s.database_id)) AS [database],
       r.sql_handle, r.statement_start_offset AS stmt_start, r.statement_end_offset AS stmt_end,
       CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END AS busy, s.is_user_process AS is_user
INTO #s
FROM sys.dm_exec_sessions s
OUTER APPLY (SELECT TOP (1) q.* FROM sys.dm_exec_requests q WHERE q.session_id = s.session_id
             ORDER BY CASE WHEN q.blocking_session_id <> 0 THEN 0 ELSE 1 END, q.request_id) r
OUTER APPLY (SELECT TOP (1) k.blocking_session_id, k.wait_type, k.wait_duration_ms, k.resource_description
             FROM sys.dm_os_waiting_tasks k
             WHERE k.session_id = s.session_id AND k.blocking_session_id > 0 AND k.blocking_session_id <> s.session_id
             ORDER BY CASE WHEN k.blocking_session_id = r.blocking_session_id THEN 0 ELSE 1 END, k.wait_duration_ms DESC) w
OUTER APPLY (SELECT MIN(a.transaction_begin_time) AS begun
             FROM sys.dm_tran_session_transactions st
             JOIN sys.dm_tran_active_transactions a ON a.transaction_id = st.transaction_id
             WHERE st.session_id = s.session_id) t
WHERE s.session_id <> @@SPID;
SELECT @can AS can_view, CAST(SERVERPROPERTY('ProductMajorVersion') AS int) AS major;
```

Why each piece:
- **One snapshot, materialized.** Reading the DMVs in place through the step's CTEs scanned each one 10 times: 203 ms of CPU and 29,832 reads on an idle SQL Server 2025, against 49 ms and 1,815 reads into `#s` (the design review), and every scan a different instant, so a chain could be half seen.
- **The permission first** (decision 86). Without VIEW SERVER PERFORMANCE STATE (VIEW SERVER STATE before SQL Server 2022, which implies it after) SQL Server shows a login only its own session, which `@@SPID` leaves out, and `dm_os_waiting_tasks` and the transaction DMVs raise error 300 (live check 10, both servers): an empty list would read as an idle server. On 2016 `HAS_PERMS_BY_NAME` gives NULL for the permission it doesn't know, and VIEW SERVER STATE decides. VIEW SERVER PERFORMANCE STATE alone is enough for every DMV and function here (live check 10, 2025).
- **One request and one waiting task per session.** The request is the blocked one if any; the waiting task is the one blocked by the request's blocker, else the longest waiting, so a parallel query whose worker is blocked while its request isn't is still seen as blocked. `blocked_by` is the request's blocker when positive and not itself, else the task's, else -2, -3 or -4 as the server gives them (an orphaned distributed transaction, deferred recovery, a latch owner unknown); -5, "latch owner not tracked", is NULL.
- **Not itself:** `@@SPID` is the report's own session, the lookup's and the step's.

**What the lookup's row decides** (`schema::activity_then`): `can_view` 1, the step; otherwise exit 1 with `-- error: activity needs VIEW SERVER PERFORMANCE STATE, which this login lacks: without it SQL Server shows a login only its own session. A DBA can grant it` (`major` 16 and later), or `... needs VIEW SERVER STATE, which ...` before.

**The step,** one `SELECT` (so a capped list gets the brief count and no `-- partial:` advice, §3), section `sessions` (`schema::ACTIVITY_STEP`):

```sql
WITH shown AS (
  SELECT x.* FROM #s x
  WHERE (x.is_user = 1 AND (x.busy = 1 OR x.open_tran > 0)) OR x.blocked_by IS NOT NULL
     OR EXISTS (SELECT 1 FROM #s b WHERE b.blocked_by = x.session_id)
), up AS (
  SELECT b.session_id, b.blocked_by AS above, 1 AS hops,
         CAST(CONCAT(',', b.session_id, ',', b.blocked_by, ',') AS varchar(1000)) AS seen, 0 AS again
  FROM shown b WHERE b.blocked_by > 0
  UNION ALL
  SELECT u.session_id, p.blocked_by, u.hops + 1,
         CAST(CONCAT(u.seen, p.blocked_by, ',') AS varchar(1000)),
         CASE WHEN CHARINDEX(CONCAT(',', p.blocked_by, ','), u.seen) > 0 THEN 1 ELSE 0 END
  FROM up u JOIN shown p ON p.session_id = u.above
  WHERE p.blocked_by > 0 AND u.again = 0 AND u.hops < 100
), tops AS (
  SELECT u.session_id, MAX(u.hops) AS depth,
         MAX(CASE WHEN u.again = 1 OR u.hops = 100 THEN 1 ELSE 0 END) AS cycle
  FROM up u GROUP BY u.session_id
), heads AS (
  SELECT u.session_id, u.above AS head FROM up u JOIN tops t ON t.session_id = u.session_id AND t.depth = u.hops
), under AS (
  SELECT u.above AS session_id, COUNT(DISTINCT u.session_id) AS blocking FROM up u WHERE u.above <> u.session_id GROUP BY u.above
)
SELECT x.session_id AS session, x.blocked_by, n.blocking,
       CASE WHEN t.cycle = 1 THEN NULL WHEN t.depth IS NOT NULL THEN t.depth WHEN n.blocking IS NOT NULL THEN 0 END AS depth,
       x.status, x.command, x.wait, x.wait_ms, x.wait_resource, x.cpu_ms, x.reads, x.elapsed_ms, x.idle_ms,
       x.open_tran, x.tran_ms, x.login, x.host, x.program, x.[database],
       CASE WHEN x.stmt_end = -1 OR x.stmt_end >= x.stmt_start
            THEN SUBSTRING(st.text, x.stmt_start / 2 + 1,
                 (CASE x.stmt_end WHEN -1 THEN DATALENGTH(st.text) ELSE x.stmt_end END - x.stmt_start) / 2 + 1) END AS statement,
       ib.event_info AS input_buffer
FROM shown x
LEFT JOIN tops t ON t.session_id = x.session_id
LEFT JOIN heads h ON h.session_id = x.session_id
LEFT JOIN under n ON n.session_id = x.session_id
LEFT JOIN under hn ON hn.session_id = COALESCE(h.head, CASE WHEN n.blocking IS NOT NULL THEN x.session_id END)
OUTER APPLY sys.dm_exec_sql_text(x.sql_handle) st
OUTER APPLY (SELECT i.event_info FROM sys.dm_exec_input_buffer(x.session_id, NULL) i
             WHERE (n.blocking IS NOT NULL AND t.depth IS NULL) OR (x.busy = 0 AND x.open_tran > 0)) ib
ORDER BY CASE WHEN hn.blocking IS NULL THEN 1 ELSE 0 END, hn.blocking DESC, COALESCE(h.head, x.session_id), COALESCE(t.depth, 0),
         COALESCE(x.elapsed_ms, x.tran_ms, x.idle_ms) DESC, x.session_id
OPTION (MAXRECURSION 100);
```

- **Rows:** a user session with a request or an open transaction; any blocked session, system ones too; any session a shown one is blocked by. An idle pooled `sqlh mcp` connection has neither a request nor a transaction (`put_back` rolls it back), so it falls out by this rule, and shows when it blocks, as its blocker (decision 83: no `own` column; an agent tells a `sqlh mcp` session by `program` and `host`). The report's own session never shows.
- **Columns:** `session, blocked_by, blocking, depth, status, command, wait, wait_ms, wait_resource, cpu_ms, reads, elapsed_ms, idle_ms, open_tran, tran_ms, login, host, program, database, statement, input_buffer`.
  - `blocking`: how many shown sessions wait on it, directly or down its chain; NULL when none. `depth`: 0 for a head blocker (it blocks and isn't blocked by a shown session), 1 and on down its chain; NULL outside any chain, inside a cycle, and behind one: `up` carries the sessions each walk has passed (`seen`) and stops at a step back to one of them (`again`) or at 100 hops (`MAXRECURSION 100`), and `tops.cycle` marks a walk that did either, so a session waiting on a cycle it isn't in gets NULL, not the hop bound's 100 (as does one at the end of a chain of 100 hops or more, which no real server has).
  - `status`: the request's, else the session's (`sleeping`). `wait`, `wait_ms`, `wait_resource`: the blocked task's when there is one, else the request's; `wait_resource` the request's compact form (`RID: 7:1:584:0`), else the task's description.
  - `cpu_ms`, `reads` (logical) and `elapsed_ms` are the request's, NULL without one; `idle_ms` is since the session's last request ended, only without a request; `open_tran` the session's `open_transaction_count`; `tran_ms` the age of its oldest active transaction. `idle_ms` and `tran_ms` are 0, never negative, for a time later than the snapshot's `GETDATE()`, which is fixed when its statement starts while the DMVs are read after it (a `tran_ms` of -3 was seen on 2016 before the clamp). Durations only, no datetimes, so there is no time zone to explain.
  - `statement`: the running statement, cut from the batch's text by its offsets (none for a negative length). `input_buffer`: `dm_exec_input_buffer`'s `event_info`, the client's batch, for a head blocker and for a session with no request and an open transaction, where the statement says nothing.
  - **A sqlh read-only run shows `open_tran 1`**, its rolled-back wrapper (measured on the blocked reader, live check 10): not a transaction left open. `--allow-write` runs show the transactions they open.
- **Order:** chains first, the largest `blocking` first, each chain by depth; then the rest by age (`elapsed_ms`, else `tran_ms`, else `idle_ms`) descending; then session.
- **Empty:** `-- no rows: sessions`.
- **Limits** (`Command::limits`): 50 rows and 300 characters a value (the statement and the input buffer) by default; a flag over either. A plan is not shown (decision 85: 2,900 to over 200,000 characters each); an agent reads one session's plan with a query of `sys.dm_exec_text_query_plan(plan_handle, statement_start_offset, statement_end_offset)` over its `sys.dm_exec_requests` row, which the MCP tool's description says.

**A timeout during the lookup** says `query timeout after N s while reading the server's sessions; raise -t` (through `sqlh mcp`, `raise timeout (the server allows up to S s)`, or `S s is the server's limit`); its other failures are `reading the server's sessions failed: error N: ...` (exit 1) and `the read of the server's sessions returned more than it should; nothing was shown` (exit 10).

**In `sqlh mcp`** the call takes a pooled connection like any other (decision 88), so with every connection of its profile held by calls stuck behind a lock it waits until its timeout; that wait's error then adds `; sqlh activity on the command line opens a connection of its own` (`engine::wait_for_connection`), since `sqlh activity` connects afresh. The engine reserves nothing for it; reopen that if real use shows the wait.

**Tested:** unit tests in `schema::tests` (`activity_takes_nothing`: the parse, every refusal of another command's option, the limits; `activitys_lookup_reads_one_snapshot_after_the_permission`; `activitys_step_is_one_select_of_its_columns`: `several_outputs` false, the columns in order, the recursion's bounds and the cycle mark; `activity_says_which_permission_it_lacks`: each branch of its row; `the_docs_show_activitys_sql_as_built`: both SQL blocks above are the constants, word for word), `mcp::tools::tests::the_activity_tool_takes_no_database_and_shows_its_defaults`; exact output through the fake server (`engine::tests::activity_shows_its_sessions_or_says_which_permission_it_lacks`, the section, `-- no rows:` and both `-- error:` wordings; `an_activity_timeout_says_what_it_was_reading_and_what_helps`, the lookup's timeout and the pool wait's advice); refusals before connecting (`tests/cli.rs`, `activity_refuses_what_it_is_given_before_connecting` and `mcp::the_activity_tool_refuses_an_argument_before_connecting`); and live on SQL Server 2025 and 2016 SP3: `mcp_tools::activity_shows_the_tests_own_chain_to_who_may_see_it` (a write of the test's own held open and a read waiting on it: the head at depth 0 with `blocking` 1 or more and its batch in `input_buffer`, the read with `blocked_by` the head, an `LCK_M_` wait on a `RID: ` resource, depth 1, `open_tran 1`; asserted on those two sessions only, since other live tests' chains show too; a reader without permission refused in its version's words, and with VIEW SERVER STATE, and on 2022 and later VIEW SERVER PERFORMANCE STATE alone, seeing the same chain with the head's `input_buffer` and the read's statement, `LCK_M_` wait and `RID: ` resource; each look checked against the held write's end, so a test that outran its minute says so) and `mcp_tools::the_activity_tool_shows_a_busy_sqlh_mcp_session_and_not_an_idle_one` (`--pool 2`: a call blocked behind the test's write, with a 28 s timeout the looks before it must stay within, shows in another call's `activity` with `program` `sqlh mcp`, blocked, `open_tran 1`; once it is cancelled, both pooled sessions are seen from another session idle with no transaction before and after the next `activity`, one of them untouched by it, since the reset on reuse gives the session it takes a new `login_time`, and neither shows); and `mcp_tools::activitys_depth_is_null_in_a_cycle_and_behind_one`, the step above run on a `#s` of made-up sessions (a three-session chain 0, 1, 2; a two-session cycle, a session waiting on it and one waiting on that, all NULL; a chain of 101, 99 one from its end and NULL at 100 hops), since no server keeps a cycle long enough to be read.

### 6.9 `errorlog [--since T] [--search TEXT] [--agent] [--archive N]` (built 2026-11-21)

A server command (Phase 6 slice 2; decision 28 as amended by decisions 84, 89 and 90, PLAN.md §6): the SQL Server error log, or with `--agent` the SQL Server Agent log, newest first. Its evidence is live check 10 (`artifacts/slice2/`, local), run on SQL Server 2025 and 2016 SP3. MCP's `errorlog` tool is the same (mcp-mode.md §4), with no `database` argument: it is a server-wide read, and an agent would take the argument for a filter.

**Options**, each checked before anything connects (exit 3; through `sqlh mcp` a tool error with exit 3, the option named without `--`, mcp-mode.md §4):
- `--since T`: the lines from `T` on, in the server's clock. `T` is `<N>m`, `<N>h` or `<N>d` before now (N a whole number of ASCII digits, at least 1; the whole span at most 3,650 days), or a server-local time `YYYY-MM-DD`, `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS`, with a space or a `T` between date and time: a real calendar date (leap years counted) in `datetime`'s years, 1753 to 9999. An offset or `Z` is refused: the log's times have none. sqlh's own parser (`schema::since_sql`, no date crate) turns it into the SQL, `DATEADD(minute, -<minutes>, GETDATE())` or `CONVERT(datetime, N'<YYYY-MM-DDTHH:MM:SS>', 126)`, so the server never parses the user's text. Refused: `--since <value>: expected 30m, 12h, 7d or a server-local time like 2026-11-20 08:00 (no offset)`, the value cut after 100 characters (`error::echo`).
- `--search TEXT`: 1 to 500 characters (code points, as MCP's schema counts its `maxLength`; `@search` is `nvarchar(1000)`, room for 500 characters of two UTF-16 units each, so it never cuts one without a word), not only whitespace, and no control character below U+0020 (a NUL cuts the string short on the server without an error, measured). Refused: `--search takes 1 to 500 characters and no control characters`. Always sent as `N'...'` (`literal`).
- `--agent`: the SQL Server Agent log (`@type` 2) instead of the server's (1).
- `--archive N`: 0 to 99 (`--archive N: expected 0 to 99`, for a number past 99 and for anything that isn't a whole number from 0 up, `-1` and `x` too; in MCP, the schema's bound: `archive must be a whole number from 0 to 99`). With `--since`: `--since and --archive don't combine: --since reads back through the archives its window needs`, since each picks what is read.

**What is read** (decision 84). Without `--since`, archive 0 (the current log) or the one `--archive` names. With it, archive 0, then 1 and on, newest first into one list, stopping after the first archive whose oldest line is at or before the window's start: each restart and each `sp_cycle_errorlog` starts a new archive, so archive 0 alone often begins hours ago, and the lead-up to a restart is always in archive 1. Only an archive `sp_enumerrorlogs` lists is read.

**The lookup** (one batch, `run::Plan::Lookup`, label `reading the error log`; `schema::errorlog_sql`):

```sql
SET NOCOUNT ON;
DECLARE @type int = 1, @archive int = NULL, @since datetime = DATEADD(minute, -120, GETDATE()), @search nvarchar(1000) = N'Login failed';
DECLARE @can bit = CASE WHEN IS_SRVROLEMEMBER(N'securityadmin') = 1 OR HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE') = 1
    OR HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW ANY ERROR LOG') = 1 THEN 1 ELSE 0 END;
DECLARE @logs TABLE (archive int, [date] nvarchar(259), bytes int);
CREATE TABLE #log (n int IDENTITY, LogDate datetime, ProcessInfo nvarchar(64), [Text] nvarchar(max));
DECLARE @a int = ISNULL(@archive, 0), @read int = 0, @mark int;
IF @can = 1
BEGIN
  INSERT INTO @logs EXEC sys.sp_enumerrorlogs @type;
  WHILE EXISTS (SELECT 1 FROM @logs WHERE archive = @a)
  BEGIN
    SET @mark = ISNULL((SELECT MAX(n) FROM #log), 0);
    INSERT INTO #log (LogDate, ProcessInfo, [Text]) EXEC sys.sp_readerrorlog @a, @type;
    SET @read += 1;
    IF @archive IS NOT NULL OR @since IS NULL OR (SELECT MIN(LogDate) FROM #log WHERE n > @mark) <= @since BREAK;
    SET @a += 1;
  END;
  IF @since IS NOT NULL DELETE #log WHERE LogDate < @since;
  IF @search IS NOT NULL DELETE FROM #log WHERE NOT EXISTS (SELECT 1 FROM #log m
      WHERE m.LogDate = #log.LogDate AND m.ProcessInfo = #log.ProcessInfo AND CHARINDEX(@search, m.[Text]) > 0);
  -- --agent: IF @search IS NOT NULL DELETE FROM #log WHERE ISNULL(CHARINDEX(@search, [Text]), 0) = 0;
END;
SELECT @can AS can_read, (SELECT COUNT(*) FROM @logs) AS archives, (SELECT MAX(archive) FROM @logs) AS last,
       @read AS [read], CAST(SERVERPROPERTY('ProductMajorVersion') AS int) AS major;
```

Why each piece:
- **Only listed archives, and only two `int`s reach the procedure.** A 22004 raised inside `INSERT ... EXEC` (an archive that doesn't exist, a date it can't read, a `varchar` search) ends the batch as mssql-tds 0.4.0's protocol error, exit 10, whatever `TRY` says (S1; DESIGN.md §7, docs/notes/upstream-issues.md U14). So `sp_readerrorlog` gets only `@a`, an archive `sp_enumerrorlogs` listed, and `@type`; the time and the search never reach it: they filter `#log` afterwards, in SQL. The time is still sqlh's own expression (a usage error before connecting, the server never parsing the user's text), and the search an `N'...'` literal.
- **`sp_readerrorlog` alone, whole archives** (decision 89). Reading through `xp_readerrorlog` with a server-side window measured the same (210 ms for a 455 KB archive): the server reads the whole file either way. Decision 84 needs each archive's oldest line, which a window hides. So `errorlog` needs no `xp_readerrorlog` grant and runs one batch.
- **The search in SQL, with each match's companions in the server's log** (decision 89): the server's own search returns only the matching line of a multi-line entry, so `Login failed` would lose its `Error: 18456, Severity: 14, State: 8.` line and `18456` the user and the reason. In the server's log a match keeps every line of the same `LogDate` and `ProcessInfo`. That is more than the entry where many lines share one time and source: at a startup the server writes a dozen `Server` lines in one millisecond, so a search for `Server process ID is` returned 8 lines on `test` (2026-11-22).
- **The Agent log's search keeps the matching lines alone** (decision 90). There `ProcessInfo` is the level (1, 2, 3) and `LogDate` has one-second precision, and no Agent message spans lines, so the server log's rule returned every line of the same level in that second: on `test` a search for `Waiting for SQL Server to recover` returned the Agent's 15 info lines of its start, and on 2016 one for `idle CPU` brought the `[475] Database Mail is not enabled` warning of the same second (2026-11-22). A line with no text matches nothing, in either log.
- **Collation and cost.** `CHARINDEX` compares in `#log`'s collation, tempdb's default (case-insensitive on the usual server collations). A search no longer narrows what is copied, so a large log takes as long with `--search` as without, bounded by `-t`.
- **`@type` always passed:** with `@p2` NULL, `sp_readerrorlog` calls `xp_readerrorlog @p1` alone and drops its filters.
- **The permission checked first:** without it `sp_readerrorlog` refuses with error 27219 (`Caller does not have permissions to execute the stored procedure`), which would end the run as a SQL error that says nothing of what to grant. securityadmin is checked because the procedure admits it.

**What the lookup's row decides** (`schema::errorlog_then`), each refusal exit 1 with `-- error: ...` (`command_error`), as a name error is:
- `can_read` 0: `errorlog needs VIEW ANY ERROR LOG or VIEW SERVER STATE, which this login lacks; a DBA can grant one` on SQL Server 2022 and later (`major` 16+), `errorlog needs VIEW SERVER STATE, which this login lacks; a DBA can grant it` before (VIEW ANY ERROR LOG came with 2022). securityadmin is never offered: Microsoft treats it as near sysadmin; it is detected, not advised. VIEW SERVER PERFORMANCE STATE alone doesn't open the log (live check 10, and the live test below).
- `archives` 0: `this server lists no error log archives` (with `--agent`: `... no SQL Server Agent log archives`).
- `read` 0: `the error log has no archive N; it keeps 0 to <last>` (`the SQL Server Agent log has no archive N; ...`), N the `--archive` asked for, else 0.
- Otherwise the step.

**The step,** one `SELECT` (so a capped log gets the brief count and no `-- partial:` advice, §3), section `server_log` (`time, source, text`) or with `--agent` `agent_log` (`time, level, text`):

```sql
SELECT LogDate AS [time], ProcessInfo AS source,
       CASE WHEN RIGHT([Text], 2) COLLATE Latin1_General_100_BIN2 = NCHAR(13) + NCHAR(10)
            THEN LEFT([Text], LEN([Text] + N'x') - 3) ELSE [Text] END AS [text]
FROM #log ORDER BY LogDate DESC, n;
-- --agent: CASE ProcessInfo WHEN N'1' THEN N'error' WHEN N'2' THEN N'warning' WHEN N'3' THEN N'info' ELSE ProcessInfo END AS level
```

- **Order:** newest first, and within one time the log's own order (`n`), so `Error: 18456, Severity: 14, State: 8.` stays above its `Login failed for user ...` line (measured on both servers). `max_rows` keeps the newest.
- **Text:** the server ends some lines with CR LF, which would show as `\r\n`; one trailing CR LF is dropped (`LEN` of the text with an `x` after it counts trailing spaces, which `LEN` alone leaves out).
- **Times** are `datetime`, server-local, as the log writes them, shown with no offset (`2026-11-21T10:15:30.120`): the `--since` given is in the same clock.
- **Empty:** `-- no rows: server_log`.
- **Limits** (`Command::limits`): 50 rows by default (each about 220 bytes), a flag over it; the cell limit stays the run's (500 by default).

**A timeout during the lookup** says what it was doing and what helps: `query timeout after N s while reading the error log; raise -t; a later --since reads fewer archives, each one whole even with --search` with `--since`, and `...; raise -t; the archive is read whole, even with --search` without (`run::LookupLabel::ErrorLog`); never "narrow the query", which a command's user can't, and a narrower search wouldn't help. Through `sqlh mcp` the same with the call's words (`raise timeout (the server allows up to S s)`, `S s is the server's limit`, `since`, `search`; mcp-mode.md §4). Its other failures: `reading the error log failed: error N: ...` (exit 1) and `the error log read returned more than it should; nothing was shown` (exit 10).

**Rare paths remain:** the log renamed by `sp_cycle_errorlog` between the listing and the read, or an access error on a file, can still reach U14's exit 10. **Unmeasured:** `--agent` on an instance whose SQL Server Agent never ran (no `SQLAGENT.OUT`), which AppVeyor's certification has, since it starts only the database engine (`scripts/appveyor-live.ps1`). The live tests take `agent_log` rows, `-- no rows: agent_log` or exit 1 `this server lists no SQL Server Agent log archives` there and fail on anything else, exit 10 included, so the next certification run measures it.

**Tested:** unit tests in `schema::tests` (the options' parse and refusals, every `--since` form and its boundaries, the search and archive bounds, the literals in the SQL, each answer of the lookup's row, the step being one `SELECT`), `cli::tests::errorlog_takes_its_options_from_anywhere`, `mcp::tools::tests::the_errorlog_tool_takes_its_arguments_and_no_database`; exact output through the fake server (`engine::tests::errorlog_shows_its_section_or_says_why_not`, `an_errorlog_timeout_names_since_and_the_timeout`); the refusals before connecting (`tests/cli.rs`, `errorlog_refuses_its_bad_options_before_connecting` and `mcp::the_errorlog_tool_refuses_a_bad_argument_before_connecting`); and live on SQL Server 2025 and 2016 SP3: `errorlog_reads_the_servers_and_the_agents_log` (the owner's reads, newest first, no trailing CR LF, the Agent log's level words, an Agent-log search returning only lines that match, an archive past the last exit 1, never 10), `errorlog_finds_a_whole_entry_and_reads_back_through_archives` (a failed login to `sqlh_nologin_<pid>` found by `--since 10m --search` as its `Error: 18456` line above its `Login failed` line, the same time and source; a window that starts inside archive 1 returns archive 1's newest line), `errorlog_needs_a_permission_and_says_which` (a reader refused in its version's words; VIEW SERVER STATE, securityadmin and, on 2022 and later, VIEW ANY ERROR LOG each read it; VIEW SERVER PERFORMANCE STATE alone doesn't) and `mcp_tools::the_errorlog_tool_reads_the_log`.

## 7. Patterns (`tables`, `find`)

Agents think in substrings and globs, and `_` is everywhere in column names (`user_id`), where it would be a LIKE wildcard. So:

- **Matching style:** no wildcard means substring (`Post` matches `dbo.Posts` and `dbo.PostLinks`). `*` and `?` are glob wildcards. `%`, `_` and `[` are always literal, bracketed (`[%]`, `[_]`, `[[]`), so no `ESCAPE` clause is needed and `\` is an ordinary character.
- **Case-insensitive:** matching uses `COLLATE Latin1_General_100_CI_AI` whatever the database collation, so a case-sensitive database doesn't make search case-sensitive.
- **What's matched:** `tables` matches against the bare name or `schema.name`, as the catalog has them (not as shown, §5); `find` matches against the bare object and column names.

## 8. Compatibility rules

- **First release:** SQL Server 2025 Standard on Windows Server 2025 Standard. Catalog fixtures and assertions must cover that target.
- **Floor:** SQL Server 2016 SP3 is supported for reading and tested live (decision 49). Prefer inexpensive portability; do not add speculative fallback paths or retain an unconditional SQL Server 2012 restriction.
- **Azure SQL DB:** a future compatibility consideration, not a first-release gate. The scope restrictions and limited `dbs` behavior described above remain constraints if Azure support is pursued.
- **Permissions:** catalog views only show objects the user has some permission on. The commands report what is visible; `info` shows the login and user, so "table missing" can be diagnosed, and its `server_permissions` the server-wide reads the login has (§6.1). The schema commands need no server permission; the server commands do, and without it each fails with exit 1, naming what to grant: `activity` needs VIEW SERVER PERFORMANCE STATE, or VIEW SERVER STATE before SQL Server 2022 (§6.8), and `errorlog` VIEW ANY ERROR LOG or VIEW SERVER STATE (or securityadmin; §6.9).
- **Temp tables:** `describe #t` works only on the session that created the table. Every CLI run is a new session, and so, in effect, is every `sqlh mcp` call (stateless, owner decision 15: the reset before a call drops the temp tables, `docs/mcp-mode.md` §5), so both refuse `#` names before connecting (exit 1, `#t is a temporary table, which only the session that made it can see; each sqlh run is a new session`; the same text in MCP). A session that keeps temp tables across calls would need MCP sessions, deferred until an agent trial shows the need.

## 9. Implementation plan (done 2026-10-13, `deps` 2026-10-14; as built above)

1. **Output sections** (`src/output/`)
   - `Section { label, names_only, collapse_empty, raw }` and `Output::set_sections`
   - text: a held-back header, the "no rows" line, and raw definition blocks
   - json: `label`
   - csv: the label in the marker, with empty sections skipped
   - exact-output tests for each
2. **`src/schema.rs`**
   - the command enum and the SQL builders
   - literal escaping and glob-to-LIKE translation
   - the type-formatting expression
   - the resolution batch, with type-specific section lists
   - unit tests for the SQL text: quoting of hostile names, patterns, per-type section lists
3. **CLI**
   - recognize commands as the first positional word, plus `find --text`
   - reject `-Q`/`-i` together with a command, and cross-database names
4. **Runner refactor** (shared with MCP)
   - pull the batch loop out of `main.rs` into a `Runner` that runs batches against a `Session` into an `Output` (done: `src/run.rs`, plan-history.md 3.1 and the Phase 3 design follow-ups)
   - add a "trusted" flag that skips the classifier but keeps the wrapper transaction (done in plan-history.md 3.1: `QueryOrigin::Generated` in `src/run.rs`, which also skips `$(var)`, and since the Phase 3 design follow-ups always runs as a read-only run, even under a profile that allows writes). `run::prepare` and `run::execute` take a `run::Request` and any `Output`, not the CLI's types, so a schema command builds its own request
   - support two-step flows: resolve, then describe
5. **Defaults** (as built, §4 "Limits"): `describe` defaults to 2,000 rows a section and 4,000 characters a value, `def` to no cell limit; a flag wins, and `--max-output` bounds both.
6. **Docs:**
   - `--help`: a SCHEMA section, under 8 lines
   - DESIGN.md §5.1: labeled sections
   - §9: points here
7. **Live checks** (as built: `schema_commands_describe_what_the_catalog_holds` in `tests/cli.rs`, with no one-time setup and no StackOverflow2010):
   - against system objects: `describe sys.objects` works, because catalog views are views
   - against a fixture each run makes in the scratch database and drops (`Schema`, on the `Objects` fixture): a view, a procedure with an output parameter and a `--` comment line, a scalar function and a TVF, a filtered index with a descending key and an include, foreign keys both ways, a computed column, a default, a check constraint, a trigger, a synonym, an alias type, descriptions, and two tables with one name in two schemas for the ambiguity

## 10. Open questions

- **Commands or flags?** Should `tables` grow `--all` (procedures and functions too), or should that be a separate `objects` command? Proposal: `find` covers it; keep `tables` focused.
- **Columns in `tables`?** Should `tables` include a column count, or the column names when there are only a few tables? Not by default; agents call `describe`.
- **Scripting tables in `def`?** Should `def` script `CREATE TABLE`? It's out of scope unless asked for, because exact scripting (partitioning, compression, filegroups) is a deep hole.
