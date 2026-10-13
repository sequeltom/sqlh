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
use crate::output::{Limits, Section};
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

/// The longest name `describe` and `def` take: `nvarchar(776)` holds four bracketed parts of
/// 128 characters, `PARSENAME`'s input.
const MAX_NAME_CHARS: usize = 776;

/// The longest pattern `tables` and `find` take.
const MAX_PATTERN_CHARS: usize = 500;

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
}

impl Command {
    /// The command a first positional word names, with the words after it, or `None` when the
    /// word isn't a command (it is then SQL, as before). `text` is `--text`, for `find` only.
    pub fn parse(words: &[String], text: bool) -> Option<Result<Command, String>> {
        let (first, rest) = words.split_first()?;
        let name = first.as_str();
        if !matches!(
            name,
            "info" | "dbs" | "tables" | "describe" | "find" | "def"
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
    /// constraint is the answer, not noise; `def` shows its definition whole, as far as
    /// `--max-output` allows. A flag still wins; a profile's setting doesn't, since it was
    /// chosen for queries.
    pub fn limits(&self, limits: &mut Limits, rows_given: bool, cell_given: bool) {
        let (rows, cell) = match self {
            Command::Describe(_) => (2000, 4000),
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
    pub fn input(&self) -> Result<Input, Failure> {
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
            Command::Describe(name) => lookup(name, false)?,
            Command::Def(name) => lookup(name, true)?,
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

/// A list aggregated server-side (schema-commands.md §3): `items` is a `SELECT N', ' + ...`
/// query; the result has no leading separator, and XML's entities aren't left in it.
fn joined(items: &str) -> String {
    format!("STUFF(({items} FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')")
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
        "SELECT s.name + N'.' + o.name AS name, \
         CASE o.type WHEN 'U' THEN N'table' ELSE N'view' END AS type, \
         CASE WHEN o.type = 'U' THEN (SELECT SUM(p.rows) FROM sys.partitions AS p \
         WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) END AS rows, \
         o.modify_date AS modified \
         FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 {filter} \
         ORDER BY s.name, o.name;"
    )
}

fn find_sql(pattern: &str, text: bool) -> String {
    let like = literal(&like_pattern(pattern));
    let mut sql = format!(
        "SELECT s.name + N'.' + o.name AS name, o.type_desc AS type \
         FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         WHERE o.is_ms_shipped = 0 \
         AND o.type IN ('U', 'V', 'P', 'PC', 'X', 'FN', 'FS', 'IF', 'TF', 'FT', 'AF', 'SN', 'TR', 'SO') \
         AND o.name {CI} LIKE {like} \
         UNION ALL \
         SELECT s.name + N'.' + ty.name, N'TYPE' \
         FROM sys.types AS ty JOIN sys.schemas AS s ON s.schema_id = ty.schema_id \
         WHERE ty.is_user_defined = 1 AND ty.name {CI} LIKE {like} \
         ORDER BY name;\n\
         SELECT s.name + N'.' + o.name AS [table], c.name AS [column], {ty} AS type \
         FROM sys.columns AS c JOIN sys.objects AS o ON o.object_id = c.object_id \
         JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
         WHERE o.is_ms_shipped = 0 AND o.type IN ('U', 'V', 'IF', 'TF', 'FT') \
         AND c.name {CI} LIKE {like} \
         ORDER BY [table], c.column_id;",
        ty = type_sql("c")
    );
    if text {
        // The line of the first match: its number (line breaks before it, plus one) and its
        // text, cut to 200 characters.
        let anywhere = literal(&format!("%{}%", like_pattern(pattern).trim_matches('%')));
        sql.push_str(&format!(
            "\nSELECT s.name + N'.' + o.name AS name, o.type_desc AS type, \
             (DATALENGTH(h.head) - DATALENGTH(REPLACE(h.head, NCHAR(10), N''))) / 2 + 1 AS line, \
             LEFT(REPLACE(SUBSTRING(m.definition, l.first, l.past - l.first), NCHAR(13), N''), 200) \
             AS text \
             FROM sys.sql_modules AS m JOIN sys.objects AS o ON o.object_id = m.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             CROSS APPLY (SELECT PATINDEX({anywhere}, m.definition {CI}) AS at) AS f \
             CROSS APPLY (SELECT LEFT(m.definition, f.at) AS head) AS h \
             CROSS APPLY (SELECT CHARINDEX(NCHAR(10), REVERSE(h.head)) AS back, \
             CHARINDEX(NCHAR(10), m.definition, f.at) AS next) AS b \
             CROSS APPLY (SELECT CASE WHEN b.back = 0 THEN 1 ELSE f.at - b.back + 2 END AS first, \
             CASE WHEN b.next = 0 THEN LEN(m.definition) + 1 ELSE b.next END AS past) AS l \
             WHERE o.is_ms_shipped = 0 AND f.at > 0 \
             ORDER BY name;"
        ));
    }
    sql
}

/// Looks `name` up for `describe` (or `def`, with `definition`): the name's parts, and the
/// objects it can mean (schema-commands.md §5, with the review's corrections):
/// 1. `OBJECT_ID`, which reads 1- and 2-part names, brackets and quotes, and an unqualified
///    name in the user's default schema, in the database's own collation.
/// 2. Failing that, the objects with that name in any schema, or in the schema given (an
///    explicit schema is never set aside), compared case-insensitively, so a case-sensitive
///    database still finds `Users` for `users`, and two that differ only in case are
///    ambiguous.
fn lookup(name: &str, definition: bool) -> Result<Plan, Failure> {
    if name.starts_with('#') {
        return Err(name_error(format!(
            "{name} is a temporary table, which only the session that made it can see; each \
             sqlh run is a new session"
        )));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Failure::usage(format!(
            "the name is longer than {MAX_NAME_CHARS} characters"
        )));
    }
    let sql = format!(
        "DECLARE @name nvarchar(776) = {name};\n\
         SELECT PARSENAME(@name, 4) AS server_part, PARSENAME(@name, 3) AS db_part, \
         DB_NAME() AS db, PARSENAME(@name, 1) AS object_part;\n\
         SELECT o.object_id, s.name AS [schema], o.name, RTRIM(o.type) AS type, o.type_desc, \
         CASE WHEN OBJECT_DEFINITION(o.object_id) IS NULL THEN 0 ELSE 1 END AS visible, \
         sn.base_object_name AS base \
         FROM sys.all_objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.synonyms AS sn ON sn.object_id = o.object_id \
         WHERE PARSENAME(@name, 4) IS NULL \
         AND (PARSENAME(@name, 3) IS NULL OR PARSENAME(@name, 3) = DB_NAME()) \
         AND (o.object_id = OBJECT_ID(@name) \
         OR (OBJECT_ID(@name) IS NULL AND o.parent_object_id = 0 \
         AND o.name {CI} = PARSENAME(@name, 1) {CI} \
         AND (PARSENAME(@name, 2) IS NULL OR s.name {CI} = PARSENAME(@name, 2) {CI}))) \
         ORDER BY s.name, o.name;",
        name = literal(name),
        CI = "COLLATE Latin1_General_100_CI_AS"
    );
    let asked = name.to_owned();
    let then = move |found: &Found| -> Result<Vec<Step>, Failure> {
        let object = resolve(&asked, found)?;
        if definition {
            def_steps(&object)
        } else {
            Ok(describe_steps(&object))
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
    /// `schema.name`, as the catalog has it.
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
fn resolve(asked: &str, found: &Found) -> Result<Object, Failure> {
    let unexpected = || Failure::new(Exit::Internal, "the name lookup returned unexpected rows");
    let parts = found
        .first()
        .and_then(|rows| rows.first())
        .ok_or_else(unexpected)?;
    let db = text(parts.get(2)).unwrap_or("?");
    let Some(object_part) = text(parts.get(3)) else {
        return Err(Failure::usage(format!(
            "{asked:?} is not an object name: at most four dot-separated parts, each plain or \
             in [brackets] or \"quotes\""
        )));
    };
    if text(parts.first()).is_some() {
        return Err(Failure::usage(format!(
            "{asked} names a linked server; schema commands look at the current database ({db})"
        )));
    }
    if let Some(other) = text(parts.get(1))
        && !other.eq_ignore_ascii_case(db)
    {
        return Err(Failure::usage(format!(
            "{asked} is in database {other}; schema commands look at the current database \
             ({db}): run with -d {other}"
        )));
    }
    let candidates: Vec<Object> = found
        .get(1)
        .ok_or_else(unexpected)?
        .iter()
        .map(|row| {
            Some(Object {
                id: int(row.first())?,
                qualified: format!("{}.{}", text(row.get(1))?, text(row.get(2))?),
                kind: text(row.get(3))?.to_owned(),
                type_desc: text(row.get(4))?.to_owned(),
                visible: int(row.get(5))? == 1,
                base: text(row.get(6)).map(str::to_owned),
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(unexpected)?;
    match candidates.len() {
        0 => Err(name_error(format!(
            "no object named {asked} in database {db}; try: sqlh find {object_part}"
        ))),
        1 => Ok(candidates.into_iter().next().ok_or_else(unexpected)?),
        n => {
            let mut names: Vec<&str> = candidates
                .iter()
                .take(AMBIGUOUS_SHOWN)
                .map(|o| o.qualified.as_str())
                .collect();
            let more = n.saturating_sub(AMBIGUOUS_SHOWN);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            names.sort_unstable();
            Err(name_error(format!(
                "ambiguous name {asked}: {}{more}; qualify it with the schema",
                names.join(", ")
            )))
        }
    }
}

/// What `describe` shows of an object, by its type (schema-commands.md §6.4).
fn describe_steps(object: &Object) -> Vec<Step> {
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
            add(Section::rows("indexes"), indexes_sql());
            add(Section::rows("foreign_keys"), foreign_keys_sql(false));
            add(Section::rows("referenced_by"), foreign_keys_sql(true));
            add(Section::rows("check_constraints"), CHECKS_SQL.to_owned());
            add(Section::rows("triggers"), triggers_sql());
        }
        "V" => {
            add(Section::rows("columns"), columns_sql());
            add(Section::rows("indexes"), indexes_sql());
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
        "SELECT s.name + N'.' + o.name AS name, o.type_desc AS type, {extra}\
         o.create_date AS created, o.modify_date AS modified, \
         CAST(ep.value AS nvarchar(4000)) AS description \
         FROM sys.all_objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
         {join}\
         LEFT JOIN sys.extended_properties AS ep ON ep.class = 1 AND ep.major_id = o.object_id \
         AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
         WHERE o.object_id = @id;"
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

fn indexes_sql() -> String {
    let keys = joined(
        "SELECT N', ' + c.name + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END \
         FROM sys.index_columns AS ic JOIN sys.all_columns AS c ON c.object_id = ic.object_id \
         AND c.column_id = ic.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id \
         AND (ic.key_ordinal > 0 OR (i.type IN (5, 6) AND ic.is_included_column = 0)) \
         ORDER BY ic.key_ordinal, ic.index_column_id",
    );
    let includes = joined(
        "SELECT N', ' + c.name \
         FROM sys.index_columns AS ic JOIN sys.all_columns AS c ON c.object_id = ic.object_id \
         AND c.column_id = ic.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id \
         AND ic.is_included_column = 1 ORDER BY ic.index_column_id",
    );
    format!(
        "SELECT i.name, i.type_desc AS type, i.is_unique AS [unique], i.is_primary_key AS pk, \
         {keys} AS keys, {includes} AS includes, i.filter_definition AS filter, \
         i.is_disabled AS disabled \
         FROM sys.indexes AS i WHERE i.object_id = @id AND i.index_id > 0 ORDER BY i.index_id;"
    )
}

/// The table's foreign keys, or (`referenced_by`) the ones that point at it.
fn foreign_keys_sql(referenced_by: bool) -> String {
    let columns = |table: &str, column: &str| {
        joined(&format!(
            "SELECT N', ' + c.name FROM sys.foreign_key_columns AS k \
             JOIN sys.all_columns AS c ON c.object_id = k.{table} AND c.column_id = k.{column} \
             WHERE k.constraint_object_id = fk.object_id ORDER BY k.constraint_column_id"
        ))
    };
    let own = columns("parent_object_id", "parent_column_id");
    let theirs = columns("referenced_object_id", "referenced_column_id");
    let common = "fk.delete_referential_action_desc AS on_delete, \
         fk.update_referential_action_desc AS on_update, \
         CASE WHEN fk.is_not_trusted = 0 AND fk.is_disabled = 0 THEN 1 ELSE 0 END AS trusted";
    if referenced_by {
        format!(
            "SELECT fk.name, OBJECT_SCHEMA_NAME(fk.parent_object_id) + N'.' \
             + OBJECT_NAME(fk.parent_object_id) AS [table], {own} AS columns, \
             {theirs} AS ref_columns, {common} \
             FROM sys.foreign_keys AS fk WHERE fk.referenced_object_id = @id \
             ORDER BY [table], fk.name;"
        )
    } else {
        format!(
            "SELECT fk.name, {own} AS columns, OBJECT_SCHEMA_NAME(fk.referenced_object_id) \
             + N'.' + OBJECT_NAME(fk.referenced_object_id) AS [references], \
             {theirs} AS ref_columns, {common} \
             FROM sys.foreign_keys AS fk WHERE fk.parent_object_id = @id ORDER BY fk.name;"
        )
    }
}

const CHECKS_SQL: &str = "SELECT name, ISNULL(definition, N'(hidden)') AS definition, \
    CASE WHEN is_not_trusted = 0 AND is_disabled = 0 THEN 1 ELSE 0 END AS trusted \
    FROM sys.check_constraints WHERE parent_object_id = @id ORDER BY name;";

fn triggers_sql() -> String {
    let events = joined(
        "SELECT N', ' + te.type_desc FROM sys.trigger_events AS te \
         WHERE te.object_id = tr.object_id ORDER BY te.type",
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
fn def_steps(object: &Object) -> Result<Vec<Step>, Failure> {
    let name = &object.qualified;
    if matches!(object.kind.as_str(), "U" | "S" | "IT") {
        return Err(name_error(format!(
            "def shows views, procedures, functions and triggers; for a table use: sqlh \
             describe {name}"
        )));
    }
    if let Some(base) = &object.base {
        return Err(name_error(format!(
            "{name} is a synonym for {base}; run: sqlh def {base}"
        )));
    }
    if !object.visible {
        return Err(name_error(format!(
            "the definition of {name} ({}) is not visible: it is encrypted, it has none (a \
             constraint, a CLR or extended procedure), or the login lacks VIEW DEFINITION",
            object.type_desc
        )));
    }
    Ok(vec![Step {
        sql: format!(
            "SELECT s.name + N'.' + o.name AS name, o.type_desc AS type, \
             OBJECT_DEFINITION(o.object_id) AS definition \
             FROM sys.all_objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             WHERE o.object_id = {};",
            object.id
        ),
        sections: vec![Section::definition("definition")],
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
        let Plan::Lookup { sql, .. } = lookup("it's", false).unwrap() else {
            panic!("a lookup");
        };
        assert!(
            sql.starts_with("DECLARE @name nvarchar(776) = N'it''s';\n"),
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

    fn found(parts: [Option<&str>; 4], objects: &[(i64, &str, &str, &str, i64)]) -> Found {
        let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.into()));
        vec![
            vec![parts.iter().map(|p| text(*p)).collect()],
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
                    ]
                })
                .collect(),
        ]
    }

    // schema-commands.md §5 with the review's corrections: one candidate is the object; none
    // and several are errors that say what to do; other databases and servers are refused.
    #[test]
    fn a_name_resolves_to_one_object_or_says_why_not() {
        let here = [None, None, Some("AppDb"), Some("Users")];
        let object = resolve("Users", &found(here, &[(5, "sales", "Users", "U", 0)])).unwrap();
        assert_eq!(object.id, 5);
        assert_eq!(object.qualified, "sales.Users");
        assert_eq!(object.kind, "U");

        let none = resolve(
            "Usrs",
            &found([None, None, Some("AppDb"), Some("Usrs")], &[]),
        )
        .unwrap_err();
        assert_eq!(
            none.message,
            "no object named Usrs in database AppDb; try: sqlh find Usrs"
        );
        assert_eq!(none.exit, Exit::SqlError);

        let two = resolve(
            "users",
            &found(
                here,
                &[
                    (5, "sales", "Users", "U", 0),
                    (6, "archive", "users", "U", 0),
                ],
            ),
        )
        .unwrap_err();
        assert_eq!(
            two.message,
            "ambiguous name users: archive.users, sales.Users; qualify it with the schema"
        );

        let other = resolve(
            "Other.dbo.T",
            &found([None, Some("Other"), Some("AppDb"), Some("T")], &[]),
        )
        .unwrap_err();
        assert_eq!(
            other.message,
            "Other.dbo.T is in database Other; schema commands look at the current database \
             (AppDb): run with -d Other"
        );
        assert_eq!(other.exit, Exit::Usage);
        let linked = resolve(
            "srv.db.dbo.T",
            &found([Some("srv"), Some("db"), Some("AppDb"), Some("T")], &[]),
        )
        .unwrap_err();
        assert!(
            linked.message.contains("names a linked server"),
            "{}",
            linked.message
        );
        let bad = resolve("a.b.c.d.e", &found([None, None, Some("AppDb"), None], &[])).unwrap_err();
        assert!(
            bad.message.contains("is not an object name"),
            "{}",
            bad.message
        );
        // The current database named explicitly is fine.
        assert!(
            resolve(
                "AppDb.sales.Users",
                &found(
                    [None, Some("AppDb"), Some("AppDb"), Some("Users")],
                    &[(5, "sales", "Users", "U", 0)]
                )
            )
            .is_ok()
        );
    }

    #[test]
    fn a_temporary_table_is_refused_before_anything_runs() {
        let Err(failure) = lookup("#orders", false) else {
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
            let steps = describe_steps(&object(kind));
            assert_eq!(labels(&steps), wanted, "{kind}");
            let sql = &steps[0].sql;
            assert!(sql.starts_with("DECLARE @id int = -5;\n"), "{sql}");
            assert_eq!(
                sql.matches(";\n").count(),
                wanted.len() + 1,
                "{kind}: {sql}"
            );
        }
        assert!(object_sql("SN").contains("base_object"));
        assert!(object_sql("U").contains("AS rows") && object_sql("U").contains("AS flags"));
        assert!(!object_sql("V").contains("AS rows"));
    }

    #[test]
    fn def_refuses_what_has_no_definition_to_show() {
        assert_eq!(labels(&def_steps(&object("P")).unwrap()), ["definition"]);
        let table = def_steps(&object("U")).unwrap_err();
        assert!(
            table
                .message
                .contains("for a table use: sqlh describe dbo.X"),
            "{}",
            table.message
        );
        let hidden = def_steps(&Object {
            visible: false,
            ..object("P")
        })
        .unwrap_err();
        assert!(
            hidden.message.contains("is not visible"),
            "{}",
            hidden.message
        );
        let synonym = def_steps(&Object {
            base: Some("[dbo].[Y]".into()),
            visible: false,
            ..object("SN")
        })
        .unwrap_err();
        assert!(
            synonym.message.contains("is a synonym for [dbo].[Y]"),
            "{}",
            synonym.message
        );
    }

    #[test]
    fn describe_and_def_have_their_own_default_limits() {
        let mut limits = Limits::default();
        Command::Describe("x".into()).limits(&mut limits, false, false);
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
