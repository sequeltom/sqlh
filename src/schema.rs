//! Schema commands (docs/schema-commands.md, PLAN.md Phase 4): `info`, `dbs`, `tables`,
//! `describe`, `find` and `def`. Each is canned T-SQL run as sqlh's own SQL
//! (`QueryOrigin::Generated`): read-only, in the wrapper transaction, no guard and no `$(var)`.
//! Arguments reach the SQL only as `N'...'` literals (`literal`), never as identifiers. Each
//! result set fills a labeled section (`output::Section`).
//!
//! `describe` and `def` first look the name up (`run::Plan::Lookup`), on the same connection:
//! the object's id and type decide what runs next.

use crate::db::Value;
use crate::error::{Exit, Failure};
use crate::output::{Advice, Limits, Section};
use crate::run::{Found, Input, Plan, Report, Step};

/// The `profile test` and `info` query. Runs on SQL Server 2012+ and Azure SQL DB without extra
/// permissions (CONNECTIONPROPERTY needs no VIEW SERVER STATE). Encryption is a client-side
/// fact, printed as a note with the connection line.
pub const INFO_SQL: &str = "SELECT @@SERVERNAME AS server, \
    CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS version, \
    CAST(SERVERPROPERTY('Edition') AS nvarchar(128)) AS edition, \
    DB_NAME() AS [database], SUSER_SNAME() AS login, USER_NAME() AS [user], \
    CAST(CONNECTIONPROPERTY('net_transport') AS nvarchar(40)) AS transport, \
    CAST(CONNECTIONPROPERTY('auth_scheme') AS nvarchar(40)) AS auth_scheme, \
    SYSDATETIMEOFFSET() AS server_time;";

/// The longest name `describe` and `def` take: four delimited parts of 128 characters, each
/// with every character a doubled `]`, and three dots (`parse_name` checks each part).
const MAX_NAME_CHARS: usize = 4 * 258 + 3;

/// The longest pattern `tables` and `find` take.
const MAX_PATTERN_CHARS: usize = 500;

/// The longest line `find --text` shows of a match, in UTF-16 units.
const PREVIEW_UNITS: usize = 200;

/// How many names an ambiguity error lists.
const AMBIGUOUS_SHOWN: usize = 10;

/// A schema command, as the command line gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Info,
    Dbs,
    Tables(Option<String>),
    Describe(String),
    Find { pattern: String, text: bool },
    Def(String),
    Deps(String),
}

impl Command {
    /// The command a first positional word names, with the words after it, or `None` when the
    /// word isn't a command (it is then SQL, as before). `text` is `--text`, for `find` only.
    pub fn parse(words: &[String], text: bool) -> Option<Result<Command, String>> {
        let (first, rest) = words.split_first()?;
        let name = first.as_str();
        if !matches!(
            name,
            "info" | "dbs" | "tables" | "describe" | "find" | "def" | "deps"
        ) {
            return None;
        }
        let one = |what: &str| -> Result<String, String> {
            match rest {
                [arg] if !arg.is_empty() => Ok(arg.clone()),
                [] | [_] => Err(format!("{name} needs {what}: sqlh {name} {what}")),
                _ => Err(format!(
                    "{name} takes one {what}; quote a name with spaces as one argument"
                )),
            }
        };
        let command = match name {
            "info" | "dbs" if !rest.is_empty() => {
                Err(format!("{name} takes no arguments, not {:?}", rest[0]))
            }
            "info" => Ok(Command::Info),
            "dbs" => Ok(Command::Dbs),
            "tables" => match rest {
                [] => Ok(Command::Tables(None)),
                [pattern] => Ok(Command::Tables(
                    (!pattern.is_empty()).then(|| pattern.clone()),
                )),
                _ => Err("tables takes one pattern; quote it as one argument".to_owned()),
            },
            "describe" => one("NAME").map(Command::Describe),
            "find" => one("PATTERN").map(|pattern| Command::Find { pattern, text }),
            "deps" => one("NAME").map(Command::Deps),
            _ => one("NAME").map(Command::Def),
        };
        Some(command.and_then(|command| {
            if text && !matches!(command, Command::Find { .. }) {
                Err("--text only applies to sqlh find".to_owned())
            } else {
                Ok(command)
            }
        }))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Command::Info => "info",
            Command::Dbs => "dbs",
            Command::Tables(_) => "tables",
            Command::Describe(_) => "describe",
            Command::Find { .. } => "find",
            Command::Def(_) => "def",
            Command::Deps(_) => "deps",
        }
    }

    /// The line about the connection: `info` prints it as a note, like `profile test`.
    pub fn report(&self) -> Report {
        match self {
            Command::Info => Report::Note,
            _ => Report::Off,
        }
    }

    /// The command's own default limits (schema-commands.md §4): `describe` shows up to 2,000
    /// rows a section and 4,000 characters a value, since a wide table or a long check
    /// constraint is the answer, not noise (and so are `deps`'s lists); `def` shows its definition whole, as far as
    /// `--max-output` allows. A flag still wins; a profile's setting doesn't, since it was
    /// chosen for queries.
    pub fn limits(&self, limits: &mut Limits, rows_given: bool, cell_given: bool) {
        let (rows, cell) = match self {
            Command::Describe(_) | Command::Deps(_) => (2000, 4000),
            Command::Def(_) => (2000, 0),
            _ => return,
        };
        if !rows_given {
            limits.max_rows = rows;
        }
        if !cell_given {
            limits.max_cell = cell;
        }
    }

    /// The run's input: the command's SQL and the sections it fills.
    /// The run's input; `advice` words the hints of a name that resolves to nothing usable.
    pub fn input(&self, advice: Advice) -> Result<Input, Failure> {
        let plan = match self {
            Command::Info => steps(INFO_SQL.to_owned(), &[Section::rows("server")]),
            Command::Dbs => steps(DBS_SQL.to_owned(), &[Section::rows("databases")]),
            Command::Tables(pattern) => {
                let pattern = pattern.as_deref().map(checked_pattern).transpose()?;
                steps(tables_sql(pattern.as_deref()), &[Section::rows("tables")])
            }
            Command::Find { pattern, text } => {
                let pattern = checked_pattern(pattern)?;
                let mut sections = vec![Section::rows("objects"), Section::rows("columns")];
                if *text {
                    sections.push(Section::rows("definitions"));
                }
                steps(find_sql(&pattern, *text), &sections)
            }
            Command::Describe(name) => lookup(name, Shows::Describe, advice)?,
            Command::Def(name) => lookup(name, Shows::Def, advice)?,
            Command::Deps(name) => lookup(name, Shows::Deps, advice)?,
        };
        Ok(Input::Command {
            name: self.name(),
            plan,
        })
    }
}

fn steps(sql: String, sections: &[Section]) -> Plan {
    Plan::Steps(vec![Step {
        sql,
        sections: sections.to_vec(),
    }])
}

/// A failure of a schema command's argument: exit 1, as the design gives name errors
/// (schema-commands.md §5), with the message as the run's error.
fn name_error(message: String) -> Failure {
    Failure::new(Exit::SqlError, message)
}

/// `s` as a T-SQL `N'...'` literal: the only escaping one needs is a doubled quote.
pub fn literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn checked_pattern(pattern: &str) -> Result<String, Failure> {
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(Failure::usage(format!(
            "the pattern is longer than {MAX_PATTERN_CHARS} characters"
        )));
    }
    Ok(pattern.to_owned())
}

/// A `tables` or `find` pattern as a `LIKE` pattern (schema-commands.md §7): `*` and `?` are
/// wildcards; `%`, `_` and `[` are literal (bracketed, so no `ESCAPE` clause is needed, and
/// the same pattern works in `PATINDEX`). With no wildcard it matches anywhere in the name.
pub fn like_pattern(pattern: &str) -> String {
    let mut like = String::with_capacity(pattern.len() + 2);
    let glob = pattern.contains(['*', '?']);
    if !glob {
        like.push('%');
    }
    for c in pattern.chars() {
        match c {
            '*' => like.push('%'),
            '?' => like.push('_'),
            '%' => like.push_str("[%]"),
            '_' => like.push_str("[_]"),
            '[' => like.push_str("[[]"),
            c => like.push(c),
        }
    }
    if !glob {
        like.push('%');
    }
    like
}

/// Matching that a case-sensitive database's collation doesn't change (schema-commands.md §7).
const CI: &str = "COLLATE Latin1_General_100_CI_AI";

/// Whether `name` can be shown as it is, by the rule `shown` applies in SQL: ASCII letters,
/// digits and `_ @ # $`, starting with a letter or `_`. Anything else is shown in brackets.
/// Reserved words aren't bracketed: sqlh's own name parser takes them as they are.
fn plain(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'))
}

/// One part of a name as sqlh shows it (R32): as it is when that is unambiguous (`plain`),
/// else in brackets with `]` doubled, as `QUOTENAME` does, so a shown name can be given back
/// to `describe` or `def`, and a dot or comma in it isn't taken for a separator.
fn quoted(name: &str) -> String {
    if plain(name) {
        name.to_owned()
    } else {
        format!("[{}]", name.replace(']', "]]"))
    }
}

/// `schema.name`, each part `quoted`.
fn qualified_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quoted(schema), quoted(name))
}

/// `quoted` in T-SQL, for a name expression `n`.
fn shown(n: &str) -> String {
    format!(
        "CASE WHEN {n} COLLATE Latin1_General_100_BIN2 LIKE N'[A-Za-z_]%' \
         AND {n} COLLATE Latin1_General_100_BIN2 NOT LIKE N'%[^A-Za-z0-9_@#$]%' \
         THEN {n} ELSE QUOTENAME({n}) END"
    )
}

/// `qualified_name` in T-SQL.
fn qualified(schema: &str, name: &str) -> String {
    format!("{} + N'.' + {}", shown(schema), shown(name))
}

/// A type's name with its size, precision or scale, for a type name expression `n` and the
/// column's (or parameter's) `max_length`, `precision` and `scale`. `nvarchar` and `nchar`
/// lengths are in bytes in the catalog, so they are halved.
fn sized_type(n: &str, owner: &str) -> String {
    format!(
        "CASE WHEN {n} IN (N'varchar', N'char', N'varbinary', N'binary') \
         THEN {n} + N'(' + CASE WHEN {owner}.max_length = -1 THEN N'max' \
         ELSE CAST({owner}.max_length AS nvarchar(10)) END + N')' \
         WHEN {n} IN (N'nvarchar', N'nchar') \
         THEN {n} + N'(' + CASE WHEN {owner}.max_length = -1 THEN N'max' \
         ELSE CAST({owner}.max_length / 2 AS nvarchar(10)) END + N')' \
         WHEN {n} IN (N'decimal', N'numeric') \
         THEN {n} + N'(' + CAST({owner}.precision AS nvarchar(3)) + N',' \
         + CAST({owner}.scale AS nvarchar(3)) + N')' \
         WHEN {n} IN (N'datetime2', N'time', N'datetimeoffset') \
         THEN {n} + N'(' + CAST({owner}.scale AS nvarchar(3)) + N')' \
         ELSE {n} END"
    )
}

/// The type of a column or parameter (`owner`, joined to `sys.types` as `t`): its name and
/// size; an alias type (`CREATE TYPE Phone FROM varchar(20)`) shows its base type too,
/// `Phone (varchar(20))`.
fn type_sql(owner: &str) -> String {
    let own = sized_type("t.name", owner);
    let base = sized_type(&format!("TYPE_NAME({owner}.system_type_id)"), owner);
    format!(
        "CASE WHEN t.is_user_defined = 1 AND t.is_table_type = 0 AND t.is_assembly_type = 0 \
         THEN t.name + N' (' + {base} + N')' ELSE {own} END"
    )
}

/// How `describe` aggregates its lists, by the database's compatibility level (found while
/// building 5.3: StackOverflow2010, at level 100, refused `WITHIN GROUP`; the threshold is
/// R37's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Aggregate {
    /// `STRING_AGG ... WITHIN GROUP`, at level 110 and up: takes any character.
    StringAgg,
    /// `FOR XML PATH`, at level 100 (and at a level the lookup didn't return): ordered at any
    /// level, but a name with a character XML can't carry (a control character such as
    /// U+001B) makes it fail (error 6841), so such a `describe` fails there.
    ForXml,
}

impl Aggregate {
    /// The aggregation for a database at `compat`. SQL Server takes `STRING_AGG`'s `WITHIN
    /// GROUP` at compatibility level 110 and up (Microsoft's STRING_AGG remarks; the live test
    /// `describe_shows_any_name_from_compatibility_level_110` runs it at 110 and 120), and
    /// refuses it at 100, the lowest level SQL Server 2025 has. A missing level gets the form
    /// every level parses.
    fn for_level(compat: Option<i64>) -> Aggregate {
        match compat {
            Some(level) if level >= 110 => Aggregate::StringAgg,
            _ => Aggregate::ForXml,
        }
    }
}

/// A list aggregated server-side (schema-commands.md §3): `item` for each row of `from`
/// (`FROM ... WHERE ...`), in `order`, joined with `, `; NULL when there are none.
fn joined(aggregate: Aggregate, item: &str, from: &str, order: &str) -> String {
    match aggregate {
        Aggregate::StringAgg => format!(
            "(SELECT STRING_AGG(CAST({item} AS nvarchar(max)), N', ') \
             WITHIN GROUP (ORDER BY {order}) {from})"
        ),
        Aggregate::ForXml => format!(
            "STUFF((SELECT N', ' + CAST({item} AS nvarchar(max)) {from} ORDER BY {order} \
             FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')"
        ),
    }
}

const DBS_SQL: &str = "SELECT name, state_desc AS state, HAS_DBACCESS(name) AS accessible, \
    compatibility_level AS compat, recovery_model_desc AS recovery, \
    is_read_only AS read_only, collation_name AS collation \
    FROM sys.databases ORDER BY name;";

fn tables_sql(pattern: Option<&str>) -> String {
    // The bare name or `schema.name`, so `Ord*`, `sales.*` and `dbo.Ord*` all work.
    let filter = match pattern {
        Some(pattern) => {
            let like = literal(&like_pattern(pattern));
            format!("AND (o.name {CI} LIKE {like} OR (s.name + N'.' + o.name) {CI} LIKE {like})")
        }
        None => String::new(),
    };
    format!(
        "SELECT {name} AS name, \
         CASE o.type WHEN 'U' THEN N'table' ELSE N'view' END AS type, \
         CASE WHEN o.type = 'U' THEN (SELECT SUM(p.rows) FROM sys.partitions AS p \
         WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) END AS rows, \
         o.modify_date AS modified \
         FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 {filter} \
         ORDER BY s.name, o.name;",
        name = qualified("s.name", "o.name")
    )
}

fn find_sql(pattern: &str, text: bool) -> String {
    let like = literal(&like_pattern(pattern));
    let mut sql = format!(
        "SELECT {object} AS name, o.type_desc AS type \
         FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE o.is_ms_shipped = 0 \
         AND o.type IN ('U', 'V', 'P', 'PC', 'X', 'FN', 'FS', 'IF', 'TF', 'FT', 'AF', 'SN', 'TR', 'SO') \
         AND o.name {CI} LIKE {like} \
         UNION ALL \
         SELECT {ty_name}, N'TYPE' \
         FROM sys.types AS ty JOIN sys.schemas AS s ON s.schema_id = ty.schema_id \
         WHERE ty.is_user_defined = 1 AND ty.name {CI} LIKE {like} \
         ORDER BY name;\n\
         SELECT {object} AS [table], c.name AS [column], {ty} AS type \
         FROM sys.columns AS c JOIN sys.objects AS o ON o.object_id = c.object_id \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
         WHERE o.is_ms_shipped = 0 AND o.type IN ('U', 'V', 'IF', 'TF', 'FT') \
         AND c.name {CI} LIKE {like} \
         ORDER BY s.name, o.name, c.column_id;",
        object = qualified("s.name", "o.name"),
        ty_name = qualified("s.name", "ty.name"),
        ty = type_sql("c")
    );
    if text {
        // The line of the first match: its number (line breaks before it, plus one) and its
        // text, cut to `PREVIEW_UNITS` UTF-16 units, or one fewer where the cut would split a
        // surrogate pair (a character outside the BMP), so the preview never ends in half a
        // character. Every position is taken in one non-supplementary collation, so they all
        // count UTF-16 units. A body the login can't see is NULL: it isn't searched, and
        // ISNULL keeps PATINDEX from getting a NULL whatever order the server evaluates the
        // predicates in (R33).
        let anywhere = literal(&format!("%{}%", like_pattern(pattern).trim_matches('%')));
        sql.push_str(&format!(
            "\nSELECT {object} AS name, o.type_desc AS type, \
             (DATALENGTH(h.head) - DATALENGTH(REPLACE(h.head, NCHAR(10), N''))) / 2 + 1 AS line, \
             LEFT(t.whole, CASE WHEN UNICODE(SUBSTRING(t.whole, {PREVIEW_UNITS}, 1)) \
             BETWEEN 55296 AND 56319 THEN {PREVIEW_UNITS} - 1 ELSE {PREVIEW_UNITS} END) AS text \
             FROM sys.sql_modules AS m JOIN sys.objects AS o ON o.object_id = m.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             CROSS APPLY (SELECT ISNULL(m.definition, N'') {CI} AS d) AS md \
             CROSS APPLY (SELECT PATINDEX({anywhere}, md.d) AS at) AS f \
             CROSS APPLY (SELECT LEFT(md.d, f.at) AS head) AS h \
             CROSS APPLY (SELECT CHARINDEX(NCHAR(10), REVERSE(h.head)) AS back, \
             CHARINDEX(NCHAR(10), md.d, f.at) AS next) AS b \
             CROSS APPLY (SELECT CASE WHEN b.back = 0 THEN 1 ELSE f.at - b.back + 2 END AS first, \
             CASE WHEN b.next = 0 THEN DATALENGTH(md.d) / 2 + 1 ELSE b.next END AS past) AS l \
             CROSS APPLY (SELECT REPLACE(SUBSTRING(md.d, l.first, l.past - l.first), NCHAR(13), \
             N'') AS whole) AS t \
             WHERE o.is_ms_shipped = 0 AND m.definition IS NOT NULL AND f.at > 0 \
             ORDER BY s.name, o.name;",
            object = qualified("s.name", "o.name")
        ));
    }
    sql
}

/// A name as `describe` and `def` take it: up to four dot-separated parts (R27), each plain
/// or delimited, `[...]` with `]]` for a `]` and `"..."` with `""` for a `"`, so a delimited
/// part can hold dots. A part left empty (`db..name`) is omitted; the object's part can't be.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Name {
    server: Option<String>,
    db: Option<String>,
    schema: Option<String>,
    object: String,
}

/// The most UTF-16 units in a part: SQL Server's limit for an identifier (`sysname`).
const MAX_PART_UNITS: usize = 128;

/// Parses a name for `describe` and `def`, before anything connects: a part that isn't a
/// valid identifier is refused (exit 3), never taken as omitted (R27).
fn parse_name(name: &str) -> Result<Name, Failure> {
    let bad = |why: &str| {
        Failure::usage(format!(
            "{name:?} is not an object name: {why}; a name has at most four dot-separated \
             parts, each plain or in [brackets] or \"quotes\", of at most 128 characters"
        ))
    };
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(bad("it is too long"));
    }
    let mut parts: Vec<Option<String>> = Vec::new();
    let mut chars = name.chars().peekable();
    loop {
        let part = match chars.peek().copied() {
            Some(open @ ('[' | '"')) => {
                chars.next();
                let close = if open == '[' { ']' } else { '"' };
                let mut part = String::new();
                loop {
                    match chars.next() {
                        None => return Err(bad("a delimited part isn't closed")),
                        Some(c) if c == close => {
                            if chars.peek() == Some(&close) {
                                chars.next();
                                part.push(close);
                            } else {
                                break;
                            }
                        }
                        Some(c) => part.push(c),
                    }
                }
                if part.is_empty() {
                    return Err(bad("a delimited part is empty"));
                }
                Some(part)
            }
            _ => {
                let mut part = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '.' {
                        break;
                    }
                    if matches!(c, '[' | ']' | '"') {
                        return Err(bad("a part with [, ] or \" in it goes in brackets"));
                    }
                    part.push(c);
                    chars.next();
                }
                (!part.is_empty()).then_some(part)
            }
        };
        if part
            .as_ref()
            .is_some_and(|p| p.encode_utf16().count() > MAX_PART_UNITS)
        {
            return Err(bad("a part is longer than 128 characters"));
        }
        parts.push(part);
        match chars.next() {
            None => break,
            Some('.') => {}
            Some(_) => return Err(bad("a delimited part ends at a dot or the end")),
        }
    }
    if parts.len() > 4 {
        return Err(bad("it has more than four parts"));
    }
    let object = parts
        .pop()
        .flatten()
        .ok_or_else(|| bad("the object's part is empty"))?;
    let schema = parts.pop().flatten();
    let db = parts.pop().flatten();
    let server = parts.pop().flatten();
    Ok(Name {
        server,
        db,
        schema,
        object,
    })
}

/// `s` as a T-SQL literal, or `NULL`.
fn literal_or_null(s: Option<&str>) -> String {
    s.map_or_else(|| "NULL".to_owned(), literal)
}

/// How many candidates the lookup returns (`TOP`): enough to list `AMBIGUOUS_SHOWN` of them and
/// know there are more; the total comes with them (R31).
const CANDIDATES: usize = AMBIGUOUS_SHOWN + 1;

/// What a looked-up object is for: the command that looked it up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shows {
    Describe,
    Def,
    Deps,
}

/// Looks `asked` up for `describe`, `def` or `deps` (`shows`): the objects it can mean
/// (schema-commands.md §5, with the reviews' corrections):
/// 0. The name is parsed here (`parse_name`); a server part is refused.
/// 1. Whether its database part is the current database is SQL Server's decision, made once
///    (`DB_ID`, which compares database names as the server does), and both the candidates
///    and the refusal of another database follow from it (R28).
/// 2. `OBJECT_ID` of the parts, each `QUOTENAME`d: an unqualified name in the user's default
///    schema, in the database's own collation.
/// 3. Failing that, the objects with that name in any schema, or in the schema given (an
///    explicit schema is never set aside), compared case-insensitively, so a case-sensitive
///    database still finds `Users` for `users`, and two that differ only in case are
///    ambiguous.
fn lookup(asked: &str, shows: Shows, advice: Advice) -> Result<Plan, Failure> {
    let name = parse_name(asked)?;
    if name.object.starts_with('#') {
        return Err(name_error(format!(
            "{asked} is a temporary table, which only the session that made it can see; each \
             sqlh run is a new session"
        )));
    }
    if let Some(server) = &name.server {
        return Err(Failure::usage(format!(
            "{asked} names a server ({server}); schema commands look at the current database \
             on the server sqlh connects to{}",
            match advice {
                Advice::Cli => " (-S)",
                Advice::Mcp => " (the profile's)",
            }
        )));
    }
    let sql = format!(
        "DECLARE @db sysname = {db}, @schema sysname = {schema}, @object sysname = {object};\n\
         DECLARE @scope int = CASE WHEN @db IS NULL OR DB_ID(@db) = DB_ID() THEN 1 \
         WHEN DB_ID(@db) IS NULL THEN 2 ELSE 0 END;\n\
         DECLARE @id int = CASE WHEN @scope = 1 \
         THEN OBJECT_ID(ISNULL(QUOTENAME(@schema) + N'.', N'') + QUOTENAME(@object)) END;\n\
         SELECT @scope AS scope, DB_NAME() AS db, \
         HAS_PERMS_BY_NAME(NULL, N'DATABASE', N'VIEW DEFINITION') AS view_definition, \
         (SELECT compatibility_level FROM sys.databases WHERE database_id = DB_ID()) AS compat;\n\
         SELECT TOP ({CANDIDATES}) o.object_id, s.name AS [schema], o.name, \
         RTRIM(o.type) AS type, o.type_desc, \
         CASE WHEN OBJECT_DEFINITION(o.object_id) IS NULL THEN 0 ELSE 1 END AS visible, \
         sn.base_object_name AS base, COUNT(*) OVER () AS total \
         FROM sys.all_objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.synonyms AS sn ON sn.object_id = o.object_id \
         WHERE @scope = 1 AND (o.object_id = @id \
         OR (@id IS NULL AND o.parent_object_id = 0 \
         AND o.name {CI} = @object {CI} \
         AND (@schema IS NULL OR s.name {CI} = @schema {CI}))) \
         ORDER BY s.name, o.name;",
        db = literal_or_null(name.db.as_deref()),
        schema = literal_or_null(name.schema.as_deref()),
        object = literal(&name.object),
        CI = "COLLATE Latin1_General_100_CI_AS"
    );
    let asked = asked.to_owned();
    let then = move |found: &Found| -> Result<Vec<Step>, Failure> {
        let object = resolve(&asked, &name, found, advice)?;
        match shows {
            Shows::Describe => Ok(describe_steps(&object, Aggregate::for_level(compat(found)))),
            Shows::Def => def_steps(&object, advice),
            Shows::Deps => deps_steps(&object, view_definition(found)),
        }
    };
    Ok(Plan::Lookup {
        sql,
        then: Box::new(then),
    })
}

/// The object a name was resolved to.
#[derive(Debug, PartialEq, Eq)]
struct Object {
    id: i64,
    /// Its schema and name, as the catalog has them.
    schema: String,
    name: String,
    /// `schema.name`, as sqlh shows it (`qualified_name`).
    qualified: String,
    /// `sys.objects.type`: `U`, `V`, `P`, `FN`, ...
    kind: String,
    type_desc: String,
    /// Whether `OBJECT_DEFINITION` returns its source.
    visible: bool,
    /// A synonym's target.
    base: Option<String>,
}

fn text(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::Text(s)) => Some(s),
        _ => None,
    }
}

fn int(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

/// The lookup's rows (`lookup`) as one object, or the error for a name that names none, more
/// than one, or one outside the current database.
fn resolve(asked: &str, name: &Name, found: &Found, advice: Advice) -> Result<Object, Failure> {
    let unexpected = || Failure::new(Exit::Internal, "the name lookup returned unexpected rows");
    let scope_row = found
        .first()
        .and_then(|rows| rows.first())
        .ok_or_else(unexpected)?;
    let scope = int(scope_row.first()).ok_or_else(unexpected)?;
    let current = text(scope_row.get(1)).unwrap_or("?");
    match (scope, name.db.as_deref()) {
        (1, _) => {}
        (2, Some(db)) => {
            return Err(Failure::usage(format!(
                "{asked}: there is no database {db} on this server; schema commands look at \
                 the current database ({current})"
            )));
        }
        (0, Some(db)) => {
            return Err(Failure::usage(format!(
                "{asked} is in database {db}; schema commands look at the current database \
                 ({current}): {}",
                match advice {
                    Advice::Cli => format!("run with -d {db}"),
                    Advice::Mcp => format!("call with database {db}"),
                }
            )));
        }
        _ => return Err(unexpected()),
    }
    let rows = found.get(1).ok_or_else(unexpected)?;
    let mut total = 0;
    let candidates: Vec<Object> = rows
        .iter()
        .map(|row| {
            let (schema, object) = (text(row.get(1))?, text(row.get(2))?);
            total = int(row.get(7))?;
            Some(Object {
                id: int(row.first())?,
                schema: schema.to_owned(),
                name: object.to_owned(),
                qualified: qualified_name(schema, object),
                kind: text(row.get(3))?.to_owned(),
                type_desc: text(row.get(4))?.to_owned(),
                visible: int(row.get(5))? == 1,
                base: text(row.get(6)).map(str::to_owned),
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(unexpected)?;
    let total = usize::try_from(total).map_err(|_| unexpected())?;
    // The total is the server's count of all of them: never fewer than the rows, and the rows
    // are fewer only because of the TOP.
    if candidates.len() > CANDIDATES
        || total < candidates.len()
        || (total > candidates.len() && candidates.len() < CANDIDATES)
    {
        return Err(unexpected());
    }
    match candidates.len() {
        0 => Err(name_error(format!(
            "no object named {asked} in database {current}; try: {} {}",
            match advice {
                Advice::Cli => "sqlh find",
                Advice::Mcp => "find with pattern",
            },
            name.object
        ))),
        1 => Ok(candidates.into_iter().next().ok_or_else(unexpected)?),
        _ => {
            let names: Vec<&str> = candidates
                .iter()
                .take(AMBIGUOUS_SHOWN)
                .map(|o| o.qualified.as_str())
                .collect();
            let more = total - names.len();
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            Err(name_error(format!(
                "ambiguous name {asked}: {}{more}; qualify it with the schema",
                names.join(", ")
            )))
        }
    }
}

/// What `describe` shows of an object, by its type (schema-commands.md §6.4).
fn describe_steps(object: &Object, aggregate: Aggregate) -> Vec<Step> {
    let id = object.id;
    let mut sql = format!("DECLARE @id int = {id};\n");
    let mut sections = Vec::new();
    let mut add = |section: Section, query: String| {
        sections.push(section);
        sql.push_str(&query);
        sql.push('\n');
    };
    let table = matches!(object.kind.as_str(), "U" | "S" | "IT");
    add(Section::rows("object"), object_sql(&object.kind));
    match object.kind.as_str() {
        _ if table => {
            add(Section::rows("columns"), columns_sql());
            add(Section::rows("indexes"), indexes_sql(aggregate));
            add(
                Section::rows("foreign_keys"),
                foreign_keys_sql(aggregate, false),
            );
            add(
                Section::rows("referenced_by"),
                foreign_keys_sql(aggregate, true),
            );
            add(Section::rows("check_constraints"), CHECKS_SQL.to_owned());
            add(Section::rows("triggers"), triggers_sql(aggregate));
        }
        "V" => {
            add(Section::rows("columns"), columns_sql());
            add(Section::rows("indexes"), indexes_sql(aggregate));
        }
        "P" | "PC" | "X" | "RF" | "FN" | "FS" | "AF" => {
            add(Section::rows("parameters"), parameters_sql());
        }
        "IF" | "TF" | "FT" => {
            add(Section::rows("parameters"), parameters_sql());
            add(Section::rows("columns"), columns_sql());
        }
        _ => {}
    }
    vec![Step { sql, sections }]
}

/// The `object` section: the name, the type, and what applies to it.
fn object_sql(kind: &str) -> String {
    let (extra, join) = match kind {
        "U" | "S" | "IT" => (
            "(SELECT SUM(p.rows) FROM sys.partitions AS p WHERE p.object_id = o.object_id \
             AND p.index_id IN (0, 1)) AS rows, \
             NULLIF(STUFF(CASE WHEN tb.temporal_type = 2 THEN N', temporal' ELSE N'' END \
             + CASE WHEN tb.temporal_type = 1 THEN N', history_table' ELSE N'' END \
             + CASE WHEN tb.is_memory_optimized = 1 THEN N', memory_optimized' ELSE N'' END \
             + CASE WHEN NOT EXISTS (SELECT 1 FROM sys.indexes AS i \
             WHERE i.object_id = o.object_id AND i.index_id = 1) THEN N', heap' ELSE N'' END, \
             1, 2, N''), N'') AS flags, ",
            "LEFT JOIN sys.tables AS tb ON tb.object_id = o.object_id ",
        ),
        "SN" => (
            "sn.base_object_name AS base_object, ",
            "LEFT JOIN sys.synonyms AS sn ON sn.object_id = o.object_id ",
        ),
        _ => ("", ""),
    };
    format!(
        "SELECT {name} AS name, o.type_desc AS type, {extra}\
         o.create_date AS created, o.modify_date AS modified, \
         CAST(ep.value AS nvarchar(4000)) AS description \
         FROM sys.all_objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         {join}\
         LEFT JOIN sys.extended_properties AS ep ON ep.class = 1 AND ep.major_id = o.object_id \
         AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
         WHERE o.object_id = @id;",
        name = qualified("s.name", "o.name")
    )
}

/// Columns of a table, view or table-valued function, system ones included. A default or a
/// computed column whose definition the login can't see shows `(hidden)`, not NULL, so it
/// isn't taken for having none (PLAN.md Phase 4).
fn columns_sql() -> String {
    format!(
        "SELECT c.name AS [column], {ty} AS type, c.is_nullable AS [null], \
         c.is_identity AS [identity], \
         CASE WHEN c.default_object_id <> 0 \
         THEN ISNULL(OBJECT_DEFINITION(c.default_object_id), N'(hidden)') END AS [default], \
         CASE WHEN c.is_computed = 1 THEN ISNULL(cc.definition, N'(hidden)') \
         + CASE WHEN cc.is_persisted = 1 THEN N' PERSISTED' ELSE N'' END END AS computed, \
         CAST(ep.value AS nvarchar(4000)) AS description \
         FROM sys.all_columns AS c JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.computed_columns AS cc ON cc.object_id = c.object_id \
         AND cc.column_id = c.column_id \
         LEFT JOIN sys.extended_properties AS ep ON ep.class = 1 AND ep.major_id = c.object_id \
         AND ep.minor_id = c.column_id AND ep.name = N'MS_Description' \
         WHERE c.object_id = @id ORDER BY c.column_id;",
        ty = type_sql("c")
    )
}

/// Indexes, with their columns `quoted` (R32). A B-tree index (clustered, nonclustered, hash)
/// has key columns, in key order, and included ones; every other type (columnstore, XML,
/// spatial, JSON) lists the columns it covers as its keys, since the catalog gives them no
/// key order (`key_ordinal` 0).
fn indexes_sql(aggregate: Aggregate) -> String {
    let from = "FROM sys.index_columns AS ic JOIN sys.all_columns AS c \
         ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id";
    let keys = joined(
        aggregate,
        &format!(
            "{} + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END",
            shown("c.name")
        ),
        &format!("{from} AND (ic.key_ordinal > 0 OR i.type NOT IN (1, 2, 7))"),
        "ic.key_ordinal, ic.index_column_id",
    );
    let includes = joined(
        aggregate,
        &shown("c.name"),
        &format!("{from} AND ic.is_included_column = 1 AND i.type IN (1, 2, 7)"),
        "ic.index_column_id",
    );
    format!(
        "SELECT i.name, i.type_desc AS type, i.is_unique AS [unique], i.is_primary_key AS pk, \
         {keys} AS keys, {includes} AS includes, i.filter_definition AS filter, \
         i.is_disabled AS disabled \
         FROM sys.indexes AS i WHERE i.object_id = @id AND i.index_id > 0 ORDER BY i.index_id;"
    )
}

/// The table's foreign keys, or (`referenced_by`) the ones that point at it; names `quoted`.
fn foreign_keys_sql(aggregate: Aggregate, referenced_by: bool) -> String {
    let columns = |table: &str, column: &str| {
        joined(
            aggregate,
            &shown("c.name"),
            &format!(
                "FROM sys.foreign_key_columns AS k JOIN sys.all_columns AS c \
                 ON c.object_id = k.{table} AND c.column_id = k.{column} \
                 WHERE k.constraint_object_id = fk.object_id"
            ),
            "k.constraint_column_id",
        )
    };
    let own = columns("parent_object_id", "parent_column_id");
    let theirs = columns("referenced_object_id", "referenced_column_id");
    let common = "fk.delete_referential_action_desc AS on_delete, \
         fk.update_referential_action_desc AS on_update, \
         CASE WHEN fk.is_not_trusted = 0 AND fk.is_disabled = 0 THEN 1 ELSE 0 END AS trusted";
    let other = qualified("s.name", "o.name");
    let (column, joins_on, mine) = if referenced_by {
        ("[table]", "fk.parent_object_id", "fk.referenced_object_id")
    } else {
        (
            "[references]",
            "fk.referenced_object_id",
            "fk.parent_object_id",
        )
    };
    let select = if referenced_by {
        format!("{other} AS [table], {own} AS columns")
    } else {
        format!("{own} AS columns, {other} AS [references]")
    };
    format!(
        "SELECT fk.name, {select}, {theirs} AS ref_columns, {common} \
         FROM sys.foreign_keys AS fk JOIN sys.all_objects AS o ON o.object_id = {joins_on} \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE {mine} = @id ORDER BY {column}, fk.name;"
    )
}

const CHECKS_SQL: &str = "SELECT name, ISNULL(definition, N'(hidden)') AS definition, \
    CASE WHEN is_not_trusted = 0 AND is_disabled = 0 THEN 1 ELSE 0 END AS trusted \
    FROM sys.check_constraints WHERE parent_object_id = @id ORDER BY name;";

fn triggers_sql(aggregate: Aggregate) -> String {
    let events = joined(
        aggregate,
        "te.type_desc",
        "FROM sys.trigger_events AS te WHERE te.object_id = tr.object_id",
        "te.type",
    );
    format!(
        "SELECT tr.name, {events} AS events, tr.is_instead_of_trigger AS instead_of, \
         tr.is_disabled AS disabled \
         FROM sys.triggers AS tr WHERE tr.parent_id = @id ORDER BY tr.name;"
    )
}

/// Parameters, and a scalar function's return type as `(return)`. Whether a T-SQL parameter
/// has a default isn't in the catalog (`has_default_value` is only kept for CLR objects), so
/// it isn't shown; `def` shows the declaration.
fn parameters_sql() -> String {
    format!(
        "SELECT CASE WHEN p.parameter_id = 0 THEN N'(return)' ELSE p.name END AS name, \
         {ty} AS type, p.is_output AS [output], p.is_readonly AS [readonly] \
         FROM sys.all_parameters AS p JOIN sys.types AS t ON t.user_type_id = p.user_type_id \
         WHERE p.object_id = @id ORDER BY p.parameter_id;",
        ty = type_sql("p")
    )
}

/// `def`: the object's source as one definition section, or why there is none to show.
///
/// The lookup found the definition visible, but the catalog can change before the definition
/// is read (R30). So the read keeps the definition in a variable, checks that it isn't NULL and
/// that the object still has the name it was looked up by, and fails (exit 1, as an error in
/// every format) when not; the row shown carries that same variable.
fn def_steps(object: &Object, advice: Advice) -> Result<Vec<Step>, Failure> {
    let name = &object.qualified;
    if matches!(object.kind.as_str(), "U" | "S" | "IT") {
        return Err(name_error(match advice {
            Advice::Cli => format!(
                "def shows views, procedures, functions and triggers; for a table use: sqlh \
                 describe {name}"
            ),
            Advice::Mcp => format!(
                "definition shows views, procedures, functions and triggers; for a table use: \
                 describe with object {name}"
            ),
        }));
    }
    if let Some(base) = &object.base {
        return Err(name_error(match advice {
            Advice::Cli => format!("{name} is a synonym for {base}; run: sqlh def {base}"),
            Advice::Mcp => {
                format!("{name} is a synonym for {base}; call definition with object {base}")
            }
        }));
    }
    if !object.visible {
        return Err(name_error(format!(
            "the definition of {name} ({}) is not visible: it is encrypted, it has none (a \
             constraint, a CLR or extended procedure), or the login lacks VIEW DEFINITION",
            object.type_desc
        )));
    }
    let gone = literal(&format!(
        "the definition of {name} can't be shown: since it was looked up, the object was \
         dropped or renamed, or its definition was encrypted or hidden from the login; run the \
         command again"
    ));
    Ok(vec![Step {
        sql: format!(
            "DECLARE @id int = {id};\n\
             DECLARE @definition nvarchar(max) = OBJECT_DEFINITION(@id);\n\
             IF @definition IS NULL OR NOT EXISTS (SELECT 1 FROM sys.all_objects AS o \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE o.object_id = @id AND s.name = {schema} {BIN} AND o.name = {object} {BIN}) \
             THROW 50000, {gone}, 1;\n\
             SELECT {shown} AS name, {kind} AS type, @definition AS definition;",
            id = object.id,
            schema = literal(&object.schema),
            object = literal(&object.name),
            shown = literal(name),
            kind = literal(&object.type_desc),
            BIN = "COLLATE Latin1_General_100_BIN2",
        ),
        sections: vec![Section::definition("definition")],
    }])
}

/// The current database's compatibility level (the lookup's first row), which decides how
/// `describe` aggregates its lists (`Aggregate`).
fn compat(found: &Found) -> Option<i64> {
    found
        .first()
        .and_then(|rows| rows.first())
        .and_then(|row| int(row.get(3)))
}

/// Whether the login has VIEW DEFINITION on the database (the lookup's first row). Without it
/// `sys.sql_expression_dependencies` returns no rows at all, not even for the objects the
/// login can see, so `deps` would look like an object nothing uses.
fn view_definition(found: &Found) -> bool {
    found
        .first()
        .and_then(|rows| rows.first())
        .and_then(|row| int(row.get(2)))
        == Some(1)
}

/// A name as a reference wrote it, from four part expressions (any of them NULL), each part
/// `shown`: the parts before the first one given are left out, and an empty one in between
/// stays empty (`db..t`).
fn written(server: &str, db: &str, schema: &str, entity: &str) -> String {
    let (sv, d, sc, e) = (shown(server), shown(db), shown(schema), shown(entity));
    format!(
        "CASE WHEN {server} IS NOT NULL THEN CONCAT({sv}, N'.', {d}, N'.', {sc}, N'.', {e}) \
         WHEN {db} IS NOT NULL THEN CONCAT({d}, N'.', {sc}, N'.', {e}) \
         WHEN {schema} IS NOT NULL THEN CONCAT({sc}, N'.', {e}) ELSE {e} END"
    )
}

/// The part of an object that holds a reference (`via`), for the referencing entity `r`
/// (a module, a table, or a check or default constraint), the object it belongs to `o`, and
/// the column `rc` (a computed column), if any: `check CK_x`, `default DF_x`, `column total`,
/// or NULL when it is the object's own code.
fn via(r: &str, o: &str, rc: &str) -> String {
    format!(
        "CASE WHEN {r}.object_id <> {o}.object_id \
         THEN CASE {r}.type WHEN 'C' THEN N'check ' ELSE N'default ' END + {r_name} \
         WHEN {rc}.column_id IS NOT NULL THEN N'column ' + {rc_name} END",
        r_name = shown(&format!("{r}.name")),
        rc_name = shown(&format!("{rc}.name")),
    )
}

/// What a synonym's base object name is, part by part (`p.server` ... `p.entity`), and the
/// object it is in this database (`OBJECT_ID` reads it; another database's or server's is
/// never taken for one here, whatever id it has there).
fn synonym_target(sn: &str) -> String {
    format!(
        "CROSS APPLY (SELECT PARSENAME({sn}.base_object_name, 4) AS server, \
         PARSENAME({sn}.base_object_name, 3) AS db, PARSENAME({sn}.base_object_name, 2) AS sch, \
         PARSENAME({sn}.base_object_name, 1) AS entity) AS p \
         CROSS APPLY (SELECT CASE WHEN p.server IS NULL \
         AND (p.db IS NULL OR DB_ID(p.db) = DB_ID()) \
         THEN OBJECT_ID({sn}.base_object_name) END AS id) AS t"
    )
}

/// The `uses` section: what the object refers to (schema-commands.md §6.7). Its own code's
/// references, and those of its computed columns and its check and default constraints, from
/// `sys.sql_expression_dependencies`; the tables its foreign keys point at; a synonym's
/// target. A reference to itself (a computed column naming its table) is left out.
fn uses_sql() -> String {
    let note = "STUFF(NULLIF(CONCAT(\
         CASE WHEN d.referenced_server_name IS NOT NULL THEN N', other server' \
         WHEN d.referenced_database_name IS NOT NULL \
         AND ISNULL(DB_ID(d.referenced_database_name), 0) <> DB_ID() THEN N', other database' \
         WHEN d.is_caller_dependent = 1 THEN N', caller-dependent' \
         WHEN d.is_ambiguous = 1 THEN N', ambiguous' \
         WHEN d.referenced_id IS NULL THEN N', not found' END, \
         CASE WHEN d.is_schema_bound_reference = 1 THEN N', schema-bound' END), N''), 1, 2, N'')";
    format!(
        "SELECT name, type, via, note FROM (\
         SELECT CASE WHEN ro.object_id IS NOT NULL THEN {resolved} ELSE {as_written} END AS name, \
         CASE WHEN ro.object_id IS NOT NULL THEN ro.type_desc \
         WHEN d.referenced_class <> 1 THEN d.referenced_class_desc END AS type, \
         {via} AS via, {note} AS note \
         FROM sys.sql_expression_dependencies AS d \
         JOIN sys.all_objects AS r ON r.object_id = d.referencing_id \
         JOIN sys.all_objects AS o ON o.object_id = @id \
         LEFT JOIN sys.all_objects AS ro ON d.referenced_class = 1 AND ro.object_id = d.referenced_id \
         LEFT JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id \
         LEFT JOIN sys.all_columns AS rc ON d.referencing_minor_id > 0 \
         AND rc.object_id = d.referencing_id AND rc.column_id = d.referencing_minor_id \
         WHERE d.referencing_class = 1 \
         AND (r.object_id = @id OR (r.parent_object_id = @id AND r.type IN ('C', 'D'))) \
         AND NOT (d.referenced_class = 1 AND ISNULL(d.referenced_id, 0) = @id) \
         UNION \
         SELECT {resolved}, ro.type_desc, N'foreign key ' + {fk_name}, NULL \
         FROM sys.foreign_keys AS fk JOIN sys.all_objects AS ro ON ro.object_id = fk.referenced_object_id \
         JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id \
         WHERE fk.parent_object_id = @id AND fk.referenced_object_id <> @id \
         UNION \
         SELECT CASE WHEN ro.object_id IS NOT NULL THEN {resolved} ELSE {target_written} END, \
         ro.type_desc, NULL, \
         CASE WHEN p.server IS NOT NULL THEN N'other server' \
         WHEN p.db IS NOT NULL AND ISNULL(DB_ID(p.db), 0) <> DB_ID() THEN N'other database' \
         WHEN ro.object_id IS NULL THEN N'not found' END \
         FROM sys.synonyms AS sn {target} \
         LEFT JOIN sys.all_objects AS ro ON ro.object_id = t.id \
         LEFT JOIN sys.schemas AS rs ON rs.schema_id = ro.schema_id \
         WHERE sn.object_id = @id\
         ) AS u ORDER BY name;",
        resolved = qualified("rs.name", "ro.name"),
        as_written = written(
            "d.referenced_server_name",
            "d.referenced_database_name",
            "d.referenced_schema_name",
            "d.referenced_entity_name"
        ),
        via = via("r", "o", "rc"),
        fk_name = shown("fk.name"),
        target_written = written("p.server", "p.db", "p.sch", "p.entity"),
        target = synonym_target("sn"),
    )
}

/// The `used_by` section: what refers to the object (schema-commands.md §6.7). Code that
/// refers to it by id (a module, or a table's computed column, check or default constraint,
/// shown as the table with `via`); code whose reference SQL Server resolves at run time by
/// the caller's schema (`EXEC usp_x`, `caller-dependent`), matched by name; the tables whose
/// foreign keys point at it; the synonyms for it.
fn used_by_sql() -> String {
    format!(
        "SELECT name, type, via, note FROM (\
         SELECT {name} AS name, o.type_desc AS type, {via} AS via, \
         STUFF(NULLIF(CONCAT(CASE WHEN d.referenced_id IS NULL THEN N', caller-dependent' END, \
         CASE WHEN d.is_schema_bound_reference = 1 THEN N', schema-bound' END), N''), 1, 2, N'') \
         AS note \
         FROM sys.sql_expression_dependencies AS d \
         JOIN sys.all_objects AS r ON r.object_id = d.referencing_id \
         JOIN sys.all_objects AS o ON o.object_id = CASE WHEN r.type IN ('C', 'D') \
         THEN r.parent_object_id ELSE r.object_id END \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.all_columns AS rc ON d.referencing_minor_id > 0 \
         AND rc.object_id = d.referencing_id AND rc.column_id = d.referencing_minor_id \
         WHERE d.referencing_class = 1 AND d.referenced_class = 1 AND o.object_id <> @id \
         AND (d.referenced_id = @id \
         OR (d.referenced_id IS NULL AND d.is_caller_dependent = 1 \
         AND d.referenced_server_name IS NULL \
         AND (d.referenced_database_name IS NULL OR DB_ID(d.referenced_database_name) = DB_ID()) \
         AND d.referenced_entity_name = @name \
         AND (d.referenced_schema_name IS NULL OR d.referenced_schema_name = @schema))) \
         UNION \
         SELECT {name}, o.type_desc, N'foreign key ' + {fk_name}, NULL \
         FROM sys.foreign_keys AS fk JOIN sys.all_objects AS o ON o.object_id = fk.parent_object_id \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE fk.referenced_object_id = @id AND fk.parent_object_id <> @id \
         UNION \
         SELECT {synonym}, sn.type_desc, NULL, NULL \
         FROM sys.synonyms AS sn JOIN sys.schemas AS s ON s.schema_id = sn.schema_id {target} \
         WHERE t.id = @id\
         ) AS u ORDER BY name;",
        name = qualified("s.name", "o.name"),
        via = via("r", "o", "rc"),
        fk_name = shown("fk.name"),
        synonym = qualified("s.name", "sn.name"),
        target = synonym_target("sn"),
    )
}

/// `deps`: what the object uses and what uses it, or why that can't be shown.
fn deps_steps(object: &Object, view_definition: bool) -> Result<Vec<Step>, Failure> {
    if !view_definition {
        return Err(name_error(
            "deps needs VIEW DEFINITION on the database, which the login lacks: without it SQL \
             Server shows no dependencies at all. describe shows foreign keys, and def an \
             object's source"
                .to_owned(),
        ));
    }
    Ok(vec![Step {
        sql: format!(
            "DECLARE @id int = {id}, @schema sysname = {schema}, @name sysname = {name};\n{uses}\n{used_by}\n",
            id = object.id,
            schema = literal(&object.schema),
            name = literal(&object.name),
            uses = uses_sql(),
            used_by = used_by_sql(),
        ),
        sections: vec![Section::rows("uses"), Section::rows("used_by")],
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands_are_the_first_word() {
        let parse = |args: &[&str], text| Command::parse(&words(args), text);
        assert_eq!(parse(&["SELECT 1"], false), None);
        assert_eq!(parse(&["Describe", "x"], false), None, "lower case only");
        assert_eq!(parse(&["info"], false), Some(Ok(Command::Info)));
        assert_eq!(parse(&["tables"], false), Some(Ok(Command::Tables(None))));
        assert_eq!(
            parse(&["tables", "Ord*"], false),
            Some(Ok(Command::Tables(Some("Ord*".into()))))
        );
        assert_eq!(
            parse(&["describe", "dbo.My Table"], false),
            Some(Ok(Command::Describe("dbo.My Table".into())))
        );
        assert_eq!(
            parse(&["find", "Id",], true),
            Some(Ok(Command::Find {
                pattern: "Id".into(),
                text: true
            }))
        );
        for (args, text, wanted) in [
            (
                &["describe"][..],
                false,
                "describe needs NAME: sqlh describe NAME",
            ),
            (
                &["describe", "a", "b"][..],
                false,
                "describe takes one NAME",
            ),
            (&["def", ""][..], false, "def needs NAME"),
            (&["dbs", "x"][..], false, "dbs takes no arguments"),
            (&["tables", "a", "b"][..], false, "tables takes one pattern"),
            (&["deps"][..], false, "deps needs NAME: sqlh deps NAME"),
            (&["deps", "a", "b"][..], false, "deps takes one NAME"),
            (
                &["describe", "x"][..],
                true,
                "--text only applies to sqlh find",
            ),
        ] {
            let error = parse(args, text).unwrap().unwrap_err();
            assert!(error.contains(wanted), "{args:?}: {error}");
        }
    }

    // Arguments only ever reach the SQL as N'...' literals with doubled quotes.
    #[test]
    fn arguments_are_literals() {
        assert_eq!(literal("dbo.Users"), "N'dbo.Users'");
        assert_eq!(
            literal("O'Brien'; DROP TABLE x;--"),
            "N'O''Brien''; DROP TABLE x;--'"
        );
        assert_eq!(literal("[a]]b]"), "N'[a]]b]'");
        let sql = tables_sql(Some("x'; SELECT 1 --"));
        assert!(sql.contains("LIKE N'%x''; SELECT 1 --%'"), "{sql}");
        let Plan::Lookup { sql, .. } = lookup("it's", Shows::Describe, Advice::Cli).unwrap() else {
            panic!("a lookup");
        };
        assert!(
            sql.starts_with(
                "DECLARE @db sysname = NULL, @schema sysname = NULL, @object sysname = N'it''s';\n"
            ),
            "{sql}"
        );
        let Plan::Lookup { sql, .. } =
            lookup("[a'b].\"c.d\"", Shows::Describe, Advice::Cli).unwrap()
        else {
            panic!("a lookup");
        };
        assert!(
            sql.starts_with(
                "DECLARE @db sysname = NULL, @schema sysname = N'a''b', @object sysname = N'c.d';\n"
            ),
            "{sql}"
        );
    }

    fn name(server: Option<&str>, db: Option<&str>, schema: Option<&str>, object: &str) -> Name {
        Name {
            server: server.map(Into::into),
            db: db.map(Into::into),
            schema: schema.map(Into::into),
            object: object.into(),
        }
    }

    // R27: a name's parts are parsed before anything connects. Delimited parts keep their dots
    // and doubled delimiters; an empty part is omitted, except the object's; anything invalid is
    // refused with exit 3, never taken for an omitted part.
    #[test]
    fn names_are_parsed_part_by_part() {
        for (input, wanted) in [
            ("Users", name(None, None, None, "Users")),
            ("dbo.Users", name(None, None, Some("dbo"), "Users")),
            ("My Table", name(None, None, None, "My Table")),
            ("[s.a].[b]", name(None, None, Some("s.a"), "b")),
            ("[s].[a.b]", name(None, None, Some("s"), "a.b")),
            ("[a]]b]", name(None, None, None, "a]b")),
            ("\"a\"\"b\".c", name(None, None, Some("a\"b"), "c")),
            ("db..t", name(None, Some("db"), None, "t")),
            ("App.dbo.t", name(None, Some("App"), Some("dbo"), "t")),
            (
                "srv.db.dbo.t",
                name(Some("srv"), Some("db"), Some("dbo"), "t"),
            ),
            (".t", name(None, None, None, "t")),
            ("[c DESC]", name(None, None, None, "c DESC")),
        ] {
            assert_eq!(parse_name(input).unwrap(), wanted, "{input}");
        }
        let long = "s".repeat(129);
        // 128 UTF-16 units is the limit: an astral character is two.
        let astral = "\u{1F600}".repeat(64);
        assert!(parse_name(&format!("[{astral}]")).is_ok());
        for (input, why) in [
            (format!("[{long}].p"), "longer than 128"),
            (format!("[{long}].dbo.p"), "longer than 128"),
            (format!("[{long}].db.dbo.p"), "longer than 128"),
            (format!("{long}.p"), "longer than 128"),
            (format!("[{astral}x].p"), "longer than 128"),
            ("a.b.c.d.e".into(), "more than four parts"),
            ("dbo.".into(), "the object's part is empty"),
            ("[dbo].[]".into(), "a delimited part is empty"),
            ("[dbo.p".into(), "isn't closed"),
            ("[dbo]x.p".into(), "ends at a dot"),
            ("a]b".into(), "goes in brackets"),
            ("x".repeat(MAX_NAME_CHARS + 1), "too long"),
        ] {
            let failure = parse_name(&input).unwrap_err();
            assert_eq!(failure.exit, Exit::Usage, "{input}");
            assert!(
                failure.message.contains(why),
                "{input}: {}",
                failure.message
            );
        }
        // Refused before connecting, with exit 3, through the command too.
        for input in [format!("[{long}].p"), "srv.db.dbo.p".into()] {
            let Err(failure) = lookup(&input, Shows::Def, Advice::Cli) else {
                panic!("{input} refused");
            };
            assert_eq!(failure.exit, Exit::Usage, "{input}");
        }
    }

    // R32: a name is shown as it is when that is unambiguous, else bracketed, the same way in
    // Rust and in the SQL.
    #[test]
    fn names_are_shown_so_they_can_be_given_back() {
        for (input, shown_as) in [
            ("Users", "Users"),
            ("_x1@#$", "_x1@#$"),
            ("select", "select"),
            ("My Table", "[My Table]"),
            ("s.a", "[s.a]"),
            ("a, b", "[a, b]"),
            ("c DESC", "[c DESC]"),
            ("a]b", "[a]]b]"),
            ("1st", "[1st]"),
            ("Größe", "[Größe]"),
            ("a\"b", "[a\"b]"),
        ] {
            assert_eq!(quoted(input), shown_as, "{input}");
            // Given back, the shown name is the same name.
            assert_eq!(parse_name(shown_as).unwrap().object, input, "{input}");
        }
        assert_eq!(qualified_name("s.a", "b"), "[s.a].b");
        assert_eq!(qualified_name("s", "a.b"), "s.[a.b]");
        let sql = shown("c.name");
        assert!(
            sql.contains("LIKE N'[A-Za-z_]%'")
                && sql.contains("NOT LIKE N'%[^A-Za-z0-9_@#$]%'")
                && sql.ends_with("ELSE QUOTENAME(c.name) END"),
            "{sql}"
        );
    }

    // schema-commands.md §7: a plain pattern is a substring; `*` and `?` are wildcards; `%`,
    // `_` and `[` are literal.
    #[test]
    fn patterns_become_like_patterns() {
        for (pattern, like) in [
            ("Post", "%Post%"),
            ("user_id", "%user[_]id%"),
            ("Post*", "Post%"),
            ("*Id", "%Id"),
            ("?d", "_d"),
            ("100%", "%100[%]%"),
            ("[x]", "%[[]x]%"),
            ("a*_b", "a%[_]b"),
        ] {
            assert_eq!(like_pattern(pattern), like, "{pattern}");
        }
    }

    // R33: the text search never hands PATINDEX a NULL, and doesn't search hidden bodies.
    #[test]
    fn the_text_search_skips_hidden_bodies() {
        let sql = find_sql("x", true);
        assert!(
            sql.contains("ISNULL(m.definition, N'') COLLATE Latin1_General_100_CI_AI AS d")
                && sql.contains("PATINDEX(N'%x%', md.d)")
                && sql.contains("m.definition IS NOT NULL AND f.at > 0"),
            "{sql}"
        );
        // The preview never ends in half a surrogate pair.
        assert!(
            sql.contains(
                "LEFT(t.whole, CASE WHEN UNICODE(SUBSTRING(t.whole, 200, 1)) \
                 BETWEEN 55296 AND 56319 THEN 200 - 1 ELSE 200 END) AS text"
            ),
            "{sql}"
        );
        assert!(!find_sql("x", false).contains("sql_modules"));
    }

    /// A lookup's reply: the scope row, then the candidates (id, schema, name, type,
    /// visible), with `total` as the server's count of all of them.
    fn found(scope: i64, objects: &[(i64, &str, &str, &str, i64)], total: i64) -> Found {
        vec![
            vec![vec![Value::Int(scope), Value::Text("AppDb".into())]],
            objects
                .iter()
                .map(|(id, schema, name, kind, visible)| {
                    vec![
                        Value::Int(*id),
                        Value::Text((*schema).into()),
                        Value::Text((*name).into()),
                        Value::Text((*kind).into()),
                        Value::Text("USER_TABLE".into()),
                        Value::Int(*visible),
                        Value::Null,
                        Value::Int(total),
                    ]
                })
                .collect(),
        ]
    }

    fn resolved(asked: &str, found: &Found) -> Result<Object, Failure> {
        resolve(asked, &parse_name(asked).unwrap(), found, Advice::Cli)
    }

    // schema-commands.md §5 with the reviews' corrections: one candidate is the object; none
    // and several are errors that say what to do; other databases are refused, by the scope
    // SQL Server decided (R28).
    #[test]
    fn a_name_resolves_to_one_object_or_says_why_not() {
        let object = resolved("Users", &found(1, &[(5, "sales", "Users", "U", 0)], 1)).unwrap();
        assert_eq!(object.id, 5);
        assert_eq!(object.qualified, "sales.Users");
        assert_eq!(
            (object.schema.as_str(), object.name.as_str()),
            ("sales", "Users")
        );
        assert_eq!(object.kind, "U");

        let odd = resolved("[s.a].b", &found(1, &[(7, "s.a", "b", "U", 0)], 1)).unwrap();
        assert_eq!(odd.qualified, "[s.a].b");

        let none = resolved("Usrs", &found(1, &[], 0)).unwrap_err();
        assert_eq!(
            none.message,
            "no object named Usrs in database AppDb; try: sqlh find Usrs"
        );
        assert_eq!(none.exit, Exit::SqlError);

        let two = resolved(
            "users",
            &found(
                1,
                &[
                    (6, "archive", "users", "U", 0),
                    (5, "sales", "Users", "U", 0),
                ],
                2,
            ),
        )
        .unwrap_err();
        assert_eq!(
            two.message,
            "ambiguous name users: archive.users, sales.Users; qualify it with the schema"
        );

        // R31: the lookup returns eleven at most, and the total says how many more there are.
        let eleven: Vec<(i64, String)> = (0..11).map(|n| (n, format!("s{n:02}"))).collect();
        let rows: Vec<(i64, &str, &str, &str, i64)> = eleven
            .iter()
            .map(|(n, s)| (*n, s.as_str(), "t", "U", 0))
            .collect();
        let many = resolved("t", &found(1, &rows, 5000)).unwrap_err();
        assert!(
            many.message.starts_with("ambiguous name t: s00.t, s01.t,")
                && many.message.contains("s09.t and 4990 more; qualify"),
            "{}",
            many.message
        );
        // A total that doesn't fit the rows is not an answer to trust.
        for (rows, total) in [(&rows[..1], 2), (&rows[..2], 1), (&rows[..], 3)] {
            let failure = resolved("t", &found(1, rows, total)).unwrap_err();
            assert_eq!(failure.exit, Exit::Internal, "{} rows, {total}", rows.len());
        }

        let other = resolved("Other.dbo.T", &found(0, &[], 0)).unwrap_err();
        assert_eq!(
            other.message,
            "Other.dbo.T is in database Other; schema commands look at the current database \
             (AppDb): run with -d Other"
        );
        assert_eq!(other.exit, Exit::Usage);
        // In sqlh mcp the hints name the tools' arguments, not the command line's.
        let mcp = |asked: &str, found: &Found| {
            resolve(asked, &parse_name(asked).unwrap(), found, Advice::Mcp)
                .unwrap_err()
                .message
        };
        assert_eq!(
            mcp("Other.dbo.T", &found(0, &[], 0)),
            "Other.dbo.T is in database Other; schema commands look at the current database \
             (AppDb): call with database Other"
        );
        assert_eq!(
            mcp("Usrs", &found(1, &[], 0)),
            "no object named Usrs in database AppDb; try: find with pattern Usrs"
        );
        let Err(server) = lookup("srv.db.dbo.T", Shows::Describe, Advice::Mcp) else {
            panic!("refused");
        };
        assert!(
            server
                .message
                .ends_with("on the server sqlh connects to (the profile's)"),
            "{}",
            server.message
        );
        let missing = resolved("Nope.dbo.T", &found(2, &[], 0)).unwrap_err();
        assert_eq!(missing.exit, Exit::Usage);
        assert!(
            missing.message.contains("there is no database Nope"),
            "{}",
            missing.message
        );
        // The current database named in any spelling the server accepts is fine.
        assert!(
            resolved(
                "APPDB.sales.Users",
                &found(1, &[(5, "sales", "Users", "U", 0)], 1)
            )
            .is_ok()
        );
    }

    #[test]
    fn a_temporary_table_is_refused_before_anything_runs() {
        let Err(failure) = lookup("#orders", Shows::Describe, Advice::Cli) else {
            panic!("refused");
        };
        assert!(
            failure.message.contains("temporary table"),
            "{}",
            failure.message
        );
    }

    fn object(kind: &str) -> Object {
        Object {
            id: -5,
            schema: "dbo".into(),
            name: "X".into(),
            qualified: "dbo.X".into(),
            kind: kind.into(),
            type_desc: "X".into(),
            visible: true,
            base: None,
        }
    }

    fn labels(steps: &[Step]) -> Vec<&'static str> {
        steps
            .iter()
            .flat_map(|s| s.sections.iter().map(|s| s.label))
            .collect()
    }

    // schema-commands.md §6.4: the sections follow the type, and each section is one query.
    #[test]
    fn describe_shows_what_applies_to_the_type() {
        for (kind, wanted) in [
            (
                "U",
                &[
                    "object",
                    "columns",
                    "indexes",
                    "foreign_keys",
                    "referenced_by",
                    "check_constraints",
                    "triggers",
                ][..],
            ),
            ("V", &["object", "columns", "indexes"][..]),
            ("P", &["object", "parameters"][..]),
            ("FN", &["object", "parameters"][..]),
            ("IF", &["object", "parameters", "columns"][..]),
            ("SN", &["object"][..]),
            ("TR", &["object"][..]),
        ] {
            for aggregate in [Aggregate::StringAgg, Aggregate::ForXml] {
                let steps = describe_steps(&object(kind), aggregate);
                assert_eq!(labels(&steps), wanted, "{kind}");
                let sql = &steps[0].sql;
                assert!(sql.starts_with("DECLARE @id int = -5;\n"), "{sql}");
                assert_eq!(
                    sql.matches(";\n").count(),
                    wanted.len() + 1,
                    "{kind}: {sql}"
                );
            }
        }
        assert!(object_sql("SN").contains("base_object"));
        assert!(object_sql("U").contains("AS rows") && object_sql("U").contains("AS flags"));
        assert!(!object_sql("V").contains("AS rows"));
    }

    // R37: ordered STRING_AGG from compatibility level 110, where SQL Server takes WITHIN
    // GROUP, so a name XML can't carry is shown at 110 and 120 too; the FOR XML lists only at
    // 100, which refuses WITHIN GROUP (StackOverflow2010, found while building 5.3), and when
    // the lookup returned no level. Both keep the list's order.
    #[test]
    fn lists_are_aggregated_as_the_database_level_allows() {
        assert_eq!(Aggregate::for_level(Some(170)), Aggregate::StringAgg);
        assert_eq!(Aggregate::for_level(Some(130)), Aggregate::StringAgg);
        assert_eq!(Aggregate::for_level(Some(120)), Aggregate::StringAgg);
        assert_eq!(Aggregate::for_level(Some(110)), Aggregate::StringAgg);
        assert_eq!(Aggregate::for_level(Some(100)), Aggregate::ForXml);
        assert_eq!(Aggregate::for_level(None), Aggregate::ForXml);
        // From the lookup's reply to describe's SQL: the level is the scope row's fourth value.
        for (level, xml) in [
            (100, true),
            (110, false),
            (120, false),
            (130, false),
            (170, false),
        ] {
            let Plan::Lookup { then, .. } = lookup("t", Shows::Describe, Advice::Cli).unwrap()
            else {
                panic!("a lookup");
            };
            let reply: Found = vec![
                vec![vec![
                    Value::Int(1),
                    Value::Text("AppDb".into()),
                    Value::Int(1),
                    Value::Int(level),
                ]],
                found(1, &[(5, "dbo", "t", "U", 0)], 1).remove(1),
            ];
            let steps = then(&reply).unwrap();
            let sql = &steps[0].sql;
            assert_eq!(sql.contains("FOR XML PATH"), xml, "{level}: {sql}");
            assert_eq!(sql.contains("STRING_AGG"), !xml, "{level}: {sql}");
        }
        let modern = joined(Aggregate::StringAgg, "c.name", "FROM t AS c", "c.id");
        assert_eq!(
            modern,
            "(SELECT STRING_AGG(CAST(c.name AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY \
             c.id) FROM t AS c)"
        );
        let old = joined(Aggregate::ForXml, "c.name", "FROM t AS c", "c.id");
        assert_eq!(
            old,
            "STUFF((SELECT N', ' + CAST(c.name AS nvarchar(max)) FROM t AS c ORDER BY c.id FOR \
             XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')"
        );
        let found: Found = vec![vec![vec![
            Value::Int(1),
            Value::Text("AppDb".into()),
            Value::Int(1),
            Value::Int(100),
        ]]];
        assert_eq!(compat(&found), Some(100));
    }

    // schema-commands.md §6.7: two sections, the object's names as literals, and a refusal
    // where SQL Server would show no dependencies at all.
    #[test]
    fn deps_shows_both_directions_or_says_why_not() {
        let steps = deps_steps(&object("P"), true).unwrap();
        assert_eq!(labels(&steps), ["uses", "used_by"]);
        let sql = &steps[0].sql;
        assert!(
            sql.starts_with(
                "DECLARE @id int = -5, @schema sysname = N'dbo', @name sysname = N'X';\n\
                 SELECT name, type, via, note FROM ("
            ),
            "{sql}"
        );
        assert_eq!(sql.matches(") AS u ORDER BY name;\n").count(), 2, "{sql}");
        // A caller-dependent reference is matched by name, never one that SQL Server failed
        // to resolve for another reason.
        assert!(
            sql.contains("d.referenced_id IS NULL AND d.is_caller_dependent = 1"),
            "{sql}"
        );
        let refused = deps_steps(&object("P"), false).unwrap_err();
        assert_eq!(refused.exit, Exit::SqlError);
        assert!(
            refused
                .message
                .starts_with("deps needs VIEW DEFINITION on the database"),
            "{}",
            refused.message
        );
        let found = |flag| -> Found {
            vec![vec![vec![
                Value::Int(1),
                Value::Text("AppDb".into()),
                Value::Int(flag),
            ]]]
        };
        assert!(view_definition(&found(1)));
        assert!(!view_definition(&found(0)));
        assert!(!view_definition(&Vec::new()));
        // A name as written: leading parts left out, an empty inner one kept.
        let sql = written("a", "b", "c", "d");
        assert!(
            sql.starts_with("CASE WHEN a IS NOT NULL THEN CONCAT(")
                && sql.contains("WHEN b IS NOT NULL THEN CONCAT(")
                && sql.ends_with("QUOTENAME(d) END END"),
            "{sql}"
        );
    }

    #[test]
    fn def_refuses_what_has_no_definition_to_show() {
        let steps = def_steps(&object("P"), Advice::Cli).unwrap();
        assert_eq!(labels(&steps), ["definition"]);
        // R30: the definition shown is the one checked.
        let sql = &steps[0].sql;
        assert!(
            sql.contains("DECLARE @definition nvarchar(max) = OBJECT_DEFINITION(@id);")
                && sql.contains("IF @definition IS NULL OR NOT EXISTS")
                && sql.contains("s.name = N'dbo' COLLATE Latin1_General_100_BIN2")
                && sql.contains("THROW 50000, N'the definition of dbo.X can''t be shown")
                && sql
                    .ends_with("SELECT N'dbo.X' AS name, N'X' AS type, @definition AS definition;"),
            "{sql}"
        );
        let table = def_steps(&object("U"), Advice::Cli).unwrap_err();
        assert!(
            table
                .message
                .contains("for a table use: sqlh describe dbo.X"),
            "{}",
            table.message
        );
        let hidden = def_steps(
            &Object {
                visible: false,
                ..object("P")
            },
            Advice::Cli,
        )
        .unwrap_err();
        assert!(
            hidden.message.contains("is not visible"),
            "{}",
            hidden.message
        );
        let synonym = Object {
            base: Some("[dbo].[Y]".into()),
            visible: false,
            ..object("SN")
        };
        let refused = def_steps(&synonym, Advice::Cli).unwrap_err();
        assert!(
            refused
                .message
                .contains("is a synonym for [dbo].[Y]; run: sqlh def [dbo].[Y]"),
            "{}",
            refused.message
        );
        // In sqlh mcp the refusals name the tools and their arguments.
        assert_eq!(
            def_steps(&object("U"), Advice::Mcp).unwrap_err().message,
            "definition shows views, procedures, functions and triggers; for a table use: \
             describe with object dbo.X"
        );
        assert_eq!(
            def_steps(&synonym, Advice::Mcp).unwrap_err().message,
            "dbo.X is a synonym for [dbo].[Y]; call definition with object [dbo].[Y]"
        );
    }

    #[test]
    fn describe_and_def_have_their_own_default_limits() {
        let mut limits = Limits::default();
        Command::Describe("x".into()).limits(&mut limits, false, false);
        assert_eq!((limits.max_rows, limits.max_cell), (2000, 4000));
        let mut limits = Limits::default();
        Command::Deps("x".into()).limits(&mut limits, false, false);
        assert_eq!((limits.max_rows, limits.max_cell), (2000, 4000));
        let mut limits = Limits::default();
        Command::Def("x".into()).limits(&mut limits, true, false);
        assert_eq!(
            (limits.max_rows, limits.max_cell),
            (Limits::default().max_rows, 0)
        );
        let mut limits = Limits::default();
        Command::Tables(None).limits(&mut limits, false, false);
        assert_eq!(limits.max_rows, Limits::default().max_rows);
    }
}
