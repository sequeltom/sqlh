# Schema commands: design and plan

Status: proposed (2026-10-09). This doc expands DESIGN.md §9 and becomes the spec once built. MCP mode exposes the same commands as tools (`docs/mcp-mode.md`).

## 1. Why

Before an agent writes a query it needs the schema: which tables exist, their columns and types, keys, and how tables relate. Today it has to write catalog queries itself, and those go wrong in predictable ways:
- `STRING_AGG` doesn't exist on SQL Server 2012–2016
- the length of `nvarchar` in `sys.columns` is in bytes
- `sp_help` output is long and padded
- several round trips are spent for each table

The schema commands give compact, correct answers in one call each. They run on every supported server, and every output format applies to them.

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
- **Two steps for object commands.** `describe`, `def` and `deps` first resolve the name in a small batch that returns `object_id`, the schema-qualified name and the type, or raises an error (§5). The second batch is chosen by type, so its sections are known in advance. That costs one extra round trip, about 1–5 ms on a LAN.
- **Version-specific features use property functions.** Using a catalog column that doesn't exist on older servers is a compile error, while property functions just return NULL there. So features from 2016 and later (temporal tables, generated-always columns, hidden columns) are read with `OBJECTPROPERTY(...)` and `COLUMNPROPERTY(...)`, which keeps one static query for every version.
- **Aggregation uses FOR XML PATH.** Lists such as index key columns use `STUFF((SELECT N', ' + ... FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')`. It works on SQL Server 2012 and Azure SQL DB, and the `TYPE`/`.value()` form avoids XML entity escaping.

## 4. Sections: an addition to the output contract

A command returns several result sets, and each gets a **label** in place of a number. The text format (DESIGN.md §5.1) gains these rules for labeled result sets:

- `-- <label>` replaces `-- result N`.
- The header has **column names only**. The types would be the catalog query's own (`nvarchar(4000)` everywhere), which is noise.
- **Empty sections print nothing.** One closing line lists them: `-- no rows: referenced_by, check_constraints, triggers`. The header is held back until the first row arrives, so an empty section costs nothing.
- `def` prints its definition **raw**, framed by a count: `-- definition: dbo.usp_GetUser (SQL_STORED_PROCEDURE), 42 lines`, then exactly 42 lines as written. The count tells a reader where the block ends, even though T-SQL comments inside it start with `-- `.

In the other formats:
- **json:** result events gain `"label"`. Empty sections are kept, with `"rows":[]`, so the schema is stable. A `def` definition is a normal string value that is never cut.
- **csv:** the label appears in the stderr marker (`-- columns: column, type, ...`), empty sections are skipped, and a definition is one quoted field.

The mechanism is `Output::set_sections(&[Section])`, called before a canned batch runs. `Tally::start_result` takes the next section and its rendering hints: names-only header, collapse when empty, raw.

**Limits.** `describe` and `def` default to no row or cell limit (`--max-rows 0 --max-cell 0`), because a 150-column table or a long procedure is the answer, not noise. Explicit flags still apply. `tables` and `find` keep the normal limits, and the footer tells the agent to narrow the pattern.

## 5. Name resolution (`describe`, `def`, `deps`)

1. Try `OBJECT_ID(@name)`. It handles 1- and 2-part names, brackets and quoted names, and unqualified names in the user's default schema.
2. If that fails, look for objects with that name in any schema (`PARSENAME(@name, 1)`, case-insensitive). If exactly one exists, use it. Agents often write `Users` when the table is `sales.Users`.
3. If several exist, raise error 50000: `ambiguous name 'Users': sales.Users, archive.Users; qualify it with the schema`.
4. If none exist, raise: `no object named 'Usrs' in database StackOverflow2010; try: sqlh find Usrs`.

Errors come through the normal error path (exit 1, `-- error ...` in text, an error event in JSON).

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
- **parameters:** `name, type, output, has_default`. The default value itself isn't in the catalog, so `def` is the way to see it.

Example of the intended text output:

```
$ sqlh --profile test describe Users
-- object
name	type	rows	created	modified	description
dbo.Users	USER_TABLE	299398	2018-06-01T11:02:44.137	2018-06-01T11:02:44.137	NULL
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
-- ok: 2 batches, 31 ms
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

- **NULL definition:** error 50000, `definition of dbo.p is not visible: it is encrypted, or the login lacks VIEW DEFINITION`.
- **Tables:** error, `def shows views, procedures, functions and triggers; for tables use: sqlh describe dbo.Users`. Scripting `CREATE TABLE` is out of scope.

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

- **SQL Server 2012 floor:** no `STRING_AGG`, `STRING_SPLIT`, `TRIM`, `CONCAT_WS`, `OPENJSON`, `DROP … IF EXISTS`, or catalog columns added after 2012. Version-specific facts come through property functions (§3).
- **Azure SQL DB:** no cross-database names. `dbs` is limited, as noted above.
- **Permissions:** catalog views only show objects the user has some permission on. The commands report what is visible; `info` shows the login and user, so "table missing" can be diagnosed. No command needs `VIEW SERVER STATE`.
- **Temp tables:** `describe #t` works only on the session that created the table. That means MCP sessions (`docs/mcp-mode.md`), not one-shot CLI calls, so the help text says so.

## 9. Implementation plan

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
   - pull the batch loop out of `main.rs` into a `Runner` that runs batches against a `Session` into an `Output`
   - add a "trusted" flag that skips the classifier but keeps the wrapper transaction
   - support two-step flows: resolve, then describe
5. **Defaults:** `describe` and `def` default to unlimited rows and cells unless flags say otherwise.
6. **Docs:**
   - `--help`: a SCHEMA section, under 8 lines
   - DESIGN.md §5.1: labeled sections
   - §9: points here
7. **Live checks:**
   - against StackOverflow2010: users, posts, votes and their keys
   - against system objects: `describe sys.objects` works, because catalog views are views
   - against a fixture schema: a view, a procedure with output parameters, a scalar function and a TVF, a filtered and an included-column index, foreign keys both ways, a computed column, a check constraint, a trigger, a synonym, and an alias type. This lives in `tests/fixtures/schema.sql`, which creates schema `sqlh_test`. It needs `--allow-write`, so it is an opt-in, one-time setup on the test server, and it comes with a drop script.

## 10. Open questions

- **Commands or flags?** Should `tables` grow `--all` (procedures and functions too), or should that be a separate `objects` command? Proposal: `find` covers it; keep `tables` focused.
- **Columns in `tables`?** Should `tables` include a column count, or the column names when there are only a few tables? Not by default; agents call `describe`.
- **Scripting tables in `def`?** Should `def` script `CREATE TABLE`? It's out of scope unless asked for, because exact scripting (partitioning, compression, filegroups) is a deep hole.
