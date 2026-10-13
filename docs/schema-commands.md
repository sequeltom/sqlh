# Schema commands: design and plan

Status: built 2026-10-13 (`src/schema.rs`; PLAN.md Phase 4), except `deps`, which stays phase 2. This doc is the spec, with DESIGN.md §5.1 "Sections" and §9. MCP mode exposes the same commands as tools (`docs/mcp-mode.md`).

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
| `sqlh info` | Server, version, edition, database, login, user, transport, auth scheme, server time; encryption as a note. Same as `profile test`. |
| `sqlh dbs` | The databases this login can see, with state and access. |
| `sqlh tables [pattern]` | Tables and views with estimated rows. |
| `sqlh describe <object>` | Everything needed to query a table or view, or call a procedure or function. |
| `sqlh find <pattern> [--text]` | Objects and columns whose names match. With `--text`, also procedure, view and function bodies that contain the pattern. |
| `sqlh def <object>` | The source of a view, procedure, function or trigger. |
| `sqlh deps <object>` (phase 2) | What the object references and what references it. |

**Invocation.**
- The command is the first positional word after the connection options: `sqlh --profile test describe dbo.Users`.
- Only these names count as commands. A query given with `-Q`, `-i` or stdin is unaffected, and none of these words is valid T-SQL on its own, so nothing that worked before changes meaning.
- `-Q`/`-i` can't be combined with a command.
- `--format`, the limits and `-o` all apply.

**Scope.** Commands cover the current database, set by `-d` or the profile. Cross-database names (`OtherDb.dbo.T`) are refused with a hint to use `-d`. Catalog views are per database, and Azure SQL DB doesn't allow cross-database queries at all.

## 3. How they run

Each command is canned T-SQL that goes through the normal pipeline: the session setup, the read-only wrapper transaction, the query timeout, the output formats, error mapping and exit codes. Implementing a command mostly means writing SQL.

- **Trusted SQL skips the classifier.** The SQL is written by the tool, not the agent, so the keyword classifier (guard layer 1) is skipped for it. The rolled-back wrapper transaction (layer 2) still applies.
- **Arguments are bound as literals, never spliced into identifiers.** User arguments become `DECLARE @name nvarchar(776) = N'...';` at the top of the batch, with single quotes doubled. That is the only escaping an `N'...'` literal needs, and it is unit-tested with hostile names.
- **Two steps for object commands.** `describe`, `def` and `deps` first look the name up (a lookup on the same connection that shows nothing) returning `object_id`, the schema-qualified name and the type; sqlh, not the SQL, raises the name errors (§5). The batch that follows is chosen by type, so its sections are known in advance. That costs one extra round trip, about 1–5 ms on a LAN.
- **Compatibility is deliberate.** The sketches use property functions for some metadata and `FOR XML PATH` for aggregation to retain inexpensive portability. They are proposals, not a requirement to maintain one query across every historical SQL Server version. Validate first-release metadata semantics and assess any added complexity against the candidate 2016 floor.
- **Aggregation sketch.** Lists such as index key columns use `STUFF((SELECT N', ' + ... FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')`. The `TYPE`/`.value()` form avoids XML entity escaping; a different implementation can be selected when its maintainability and target compatibility justify it.

## 4. Sections: an addition to the output contract

A command returns several result sets, and each gets a **label** in place of a number. The text format (DESIGN.md §5.1) gains these rules for labeled result sets:

- `-- <label>` replaces `-- result N`.
- The header has **column names only**. The types would be the catalog query's own (`nvarchar(4000)` everywhere), which is noise.
- **Empty sections print nothing.** One closing line lists them: `-- no rows: referenced_by, check_constraints, triggers`. The header is held back until the first row arrives, so an empty section costs nothing.
- `def` prints its definition framed by a count: `-- definition: dbo.usp_GetUser (SQL_STORED_PROCEDURE), 42 lines`, then exactly 42 lines, each escaped like a value except that a tab stays a tab (DESIGN.md §5.1). The count tells a reader where the block ends, even though T-SQL comments inside it start with `-- `.
- A section's footer is written only when it didn't show everything (rows hidden, values cut).

In the other formats:
- **json:** result events gain `"label"`. Empty sections are kept, with `"rows":[]`, so the schema is stable. A `def` definition is a normal string value that is never cut.
- **csv:** the label appears in the stderr marker (`-- columns: column, type, ...`), empty sections are skipped, and a definition is one quoted field.

The mechanism is `Output::set_sections(&[Section])`, called before a command's batch begins. `Tally::start_result` takes the next section (`Section { label, definition }`); every section has a names-only header and collapses when empty, and a `definition` one prints as a counted block.

**Limits.** `describe` defaults to 2,000 rows a section and 4,000 characters a value, and `def` to no cell limit, because a 150-column table or a long procedure is the answer, not noise; `--max-output` still bounds the whole response (the review's correction: a high default under the response budget, not unlimited). Explicit flags still apply. `tables` and `find` keep the normal limits, and the footer tells the agent to narrow the pattern.

## 5. Name resolution (`describe`, `def`, `deps`)

1. A linked-server name is refused, and a name in another database is refused with `run with -d Other` (exit 3). `PARSENAME` splits the name; a name it can't split is a usage error.
2. Try `OBJECT_ID(@name)`. It handles 1- and 2-part names, brackets and quoted names, and unqualified names in the user's default schema, in the database's own collation. System objects resolve too (`sys.objects`).
3. If that fails, look for top-level objects with that name (compared with `COLLATE Latin1_General_100_CI_AS`): in any schema for an unqualified name, and only in the schema given for a qualified one, compared the same way. An explicit schema is never set aside. If exactly one exists, use it: agents often write `Users` when the table is `sales.Users`, and in a case-sensitive database `users` still finds `Users`.
4. If several exist (two schemas, or two names that differ only in case), the run fails: `ambiguous name Users: archive.Users, sales.Users; qualify it with the schema` (ten names at most, then `and N more`).
5. If none exist: `no object named Usrs in database AppDb; try: sqlh find Usrs`.

Name errors end the run as a failure (exit 1, `-- error: ...` in text, the document's `error` in JSON).

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

The `profile test` query already in `src/profile.rs`, moved to `src/schema.rs`, with encryption printed as a note.

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
SELECT s.name + N'.' + o.name AS name,
       CASE o.type WHEN 'U' THEN N'table' ELSE N'view' END AS type,
       (SELECT SUM(p.rows) FROM sys.partitions AS p
         WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS rows,
       o.modify_date AS modified
FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id
WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0
  AND (@pattern IS NULL OR s.name + N'.' + o.name COLLATE Latin1_General_100_CI_AI LIKE @pattern ESCAPE N'\')
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
- **indexes:** `name, type, unique, pk, keys, includes, filter, disabled`. Keys are in key order with ` DESC` where it applies; `includes` and `filter` come from the filtered and covering metadata.
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
- **definitions** (`--text` only): `name, type, line, text`. Every module whose `OBJECT_DEFINITION` contains the pattern, with the line number of the first match and that line (cut to 200 characters). The line number is computed in T-SQL by counting `CHAR(10)` before `CHARINDEX`.

This answers "where is `ClosedDate` used?" in one call.

### 6.6 `def <object>`

```sql
SELECT @qualified AS name, o.type_desc AS type, OBJECT_DEFINITION(@id) AS definition
FROM sys.objects AS o WHERE o.object_id = @id;
```

- **NULL definition:** the run fails (exit 1), `the definition of dbo.p (SQL_STORED_PROCEDURE) is not visible: it is encrypted, it has none (a constraint, a CLR or extended procedure), or the login lacks VIEW DEFINITION`. The lookup checks this, so no batch runs.
- **Tables:** the run fails, `def shows views, procedures, functions and triggers; for a table use: sqlh describe dbo.Users`. Scripting `CREATE TABLE` is out of scope.
- **Synonyms:** the run fails, `dbo.s is a synonym for [dbo].[t]; run: sqlh def [dbo].[t]`.

### 6.7 `deps <object>` (phase 2)

- **uses:** from `sys.sql_expression_dependencies` where `referencing_id = @id`, with `entity, type, columns`, including cross-database references by name.
- **used_by:** from `sys.dm_sql_referencing_entities`, via its documented error-tolerant use.

This is phase 2 because the DMV has permission and ambiguity quirks worth testing against real schemas first.

## 7. Patterns (`tables`, `find`)

Agents think in substrings and globs, and `_` is everywhere in column names (`user_id`), where it would be a LIKE wildcard. So:

- **Matching style:** no wildcard means substring (`Post` matches `dbo.Posts` and `dbo.PostLinks`). `*` and `?` are glob wildcards. `%`, `_`, `[` and `\` are always literal, escaped with `ESCAPE N'\'`.
- **Case-insensitive:** matching uses `COLLATE Latin1_General_100_CI_AI` whatever the database collation, so a case-sensitive database doesn't make search case-sensitive.
- **What's matched:** `tables` matches against `schema.name`; `find` matches against the bare object and column names.

## 8. Compatibility rules

- **First release:** SQL Server 2025 Standard on Windows Server 2025 Standard. Catalog fixtures and assertions must cover that target.
- **Candidate floor:** SQL Server 2016 / Windows Server 2016, subject to acceptable technical debt. Prefer inexpensive portability; do not add speculative fallback paths or retain an unconditional SQL Server 2012 restriction.
- **Azure SQL DB:** a future compatibility consideration, not a first-release gate. The scope restrictions and limited `dbs` behavior described above remain constraints if Azure support is pursued.
- **Permissions:** catalog views only show objects the user has some permission on. The commands report what is visible; `info` shows the login and user, so "table missing" can be diagnosed. No command needs `VIEW SERVER STATE`.
- **Temp tables:** `describe #t` works only on the session that created the table. That means MCP sessions (`docs/mcp-mode.md`), not one-shot CLI calls, so the CLI refuses `#` names before connecting (exit 1, `#t is a temporary table, which only the session that made it can see; each sqlh run is a new session`).

## 9. Implementation plan (done 2026-10-13, except `deps`; as built above)

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
   - pull the batch loop out of `main.rs` into a `Runner` that runs batches against a `Session` into an `Output` (done: `src/run.rs`, PLAN.md 3.1 and the Phase 3 design follow-ups)
   - add a "trusted" flag that skips the classifier but keeps the wrapper transaction (done in PLAN.md 3.1: `QueryOrigin::Generated` in `src/run.rs`, which also skips `$(var)`, and since the Phase 3 design follow-ups always runs as a read-only run, even under a profile that allows writes). `run::prepare` and `run::execute` take a `run::Request` and any `Output`, not the CLI's types, so a schema command builds its own request
   - support two-step flows: resolve, then describe
5. **Defaults:** `describe` and `def` default to unlimited rows and cells unless flags say otherwise.
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
