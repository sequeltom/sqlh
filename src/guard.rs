//! Read-only guard, layer 1 (DESIGN.md §7): a keyword classifier over a T-SQL token
//! stream. It fails closed: any write keyword outside strings, comments and quoted names
//! blocks the batch unless its target is a temp table (#t) or table variable (@t).
//! Layer 2, the rolled-back wrapper transaction, lives in main.rs.
//!
//! The tokens are read as a stream and none is kept (plan-history.md 2.14), so the guard's memory
//! doesn't depend on what the input looks like: see `Guard` and `Lexer`.
//!
//! Its work is bounded as well (plan-history.md R18): see `Work`.
//!
//! The guard reduces accidents. It is not a security boundary: database permissions are.

use std::cell::Cell;

#[derive(Debug, PartialEq, Eq)]
pub struct Blocked {
    /// 1-based line within the batch.
    pub line: u32,
    pub reason: String,
    /// How to get past the refusal; the output puts it after "nothing was run.".
    pub advice: String,
    /// Whether `--allow-write` lifts the refusal (it skips the classifier). `advice` then
    /// offers it, except for `FLUSH_PROC`, which is no use under a profile that refuses it
    /// (`advice_for`). False too for `wildcard_stats`: that call is no write, so the flag is no
    /// advice for it, though a run with writes allowed skips the classifier and so this check.
    /// And for `session_state`, which only `sqlh mcp` refuses, where the flag doesn't exist.
    pub lifted_by_write: bool,
    /// What to do short of writing, where there is something (`Use a #temp table`). For a
    /// refusal writing doesn't lift, the advice in `sqlh mcp`, which names no flag.
    pub instead: Option<String>,
}

impl Blocked {
    /// A refusal that `--allow-write` lifts, with nothing to try short of writing.
    fn write(line: u32, reason: String) -> Blocked {
        Blocked {
            line,
            reason,
            advice: WRITE_ADVICE.into(),
            lifted_by_write: true,
            instead: None,
        }
    }

    /// A refusal of what sqlh doesn't know, so it doesn't call it a write (decision 23). `tail`
    /// is the run's `Guard::as_written`.
    fn unknown(line: u32, reason: String, tail: &str) -> Blocked {
        Blocked {
            line,
            reason,
            advice: format!("Rerun with --allow-write{tail}"),
            lifted_by_write: true,
            instead: None,
        }
    }

    /// A refusal that `--allow-write` lifts, with `instead` to try first: the advice is
    /// `<instead>, or rerun with --allow-write<tail>`.
    fn write_or(line: u32, reason: String, instead: String, tail: &str) -> Blocked {
        Blocked {
            line,
            reason,
            advice: format!("{instead}, or rerun with --allow-write{tail}"),
            lifted_by_write: true,
            instead: Some(instead),
        }
    }

    /// The advice to print. Where writes are off for good (`off`), `--allow-write` can't help,
    /// so the advice never offers it: it says why writes are off, after what can be done
    /// instead. Under a profile whose `write_policy` is `deny`, `--allow-write` is a usage error
    /// (plan-history.md 2.16, from agent trial 2); `sqlh mcp` is read-only (owner decision 16).
    pub fn advice_for(&self, off: Option<&WritesOff>) -> String {
        // The reason, as a clause after what to try instead, and as a sentence on its own (the
        // program's name keeps its lower case).
        let (clause, sentence) = match off {
            Some(WritesOff::Mcp) if !self.lifted_by_write => {
                return self.instead.clone().unwrap_or_else(|| self.advice.clone());
            }
            Some(_) if !self.lifted_by_write => return self.advice.clone(),
            Some(WritesOff::Profile(profile)) => (
                format!("profile {profile} doesn't allow writes (write_policy = \"deny\")"),
                format!("Profile {profile} doesn't allow writes (write_policy = \"deny\")"),
            ),
            Some(WritesOff::Mcp) => (
                "sqlh mcp is read-only".into(),
                "sqlh mcp is read-only".into(),
            ),
            None => return self.advice.clone(),
        };
        match &self.instead {
            Some(instead) => format!("{instead}; {clause}"),
            None => sentence,
        }
    }
}

/// Why writes are off for a run, so that a refusal doesn't offer `--allow-write`
/// (`Blocked::advice_for`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritesOff {
    /// The profile's `write_policy` is `deny` (its name).
    Profile(String),
    /// `sqlh mcp`, which only reads.
    Mcp,
}

/// Advice for the refusals that `--allow-write` resolves.
pub const WRITE_ADVICE: &str = "Rerun with --allow-write if the write is intended";

/// Which checks apply to a batch.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// `--allow-write` (or a profile allowing writes): skip the write classifier.
    pub writes: bool,
    /// `--check`: refuse anything that could switch compile-only mode off.
    pub compile_only: bool,
    /// `sqlh mcp`, whose session goes on to the next call: refuse the session state its reset
    /// isn't known to clear (decision 30(e)). The write classifier reads this, and writes are
    /// never allowed there (`mcp::for_read_tools`).
    pub reused_session: bool,
}

impl Policy {
    /// True when no check applies, so the guard needn't run.
    pub fn is_open(self) -> bool {
        self.writes && !self.compile_only
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Bare identifier or keyword.
    Word,
    /// `@name` or `@@name`.
    Var,
    /// `#name` or `##name`.
    Temp,
    /// `[name]` or `"name"`.
    Quoted,
    Str,
    Num,
    Punct(char),
}

/// A token as a view of the input: nothing in it is copied.
#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    kind: Kind,
    /// As written, delimiters included.
    text: &'a str,
    /// What a name is compared by, in any case: the text of a word, variable or temp table,
    /// and a quoted name without its delimiters (an unterminated one has no closing delimiter
    /// to strip; the server reports the syntax error). Empty for the other kinds.
    name: &'a str,
    line: u32,
}

impl Token<'_> {
    /// True for a bare word spelled `keyword` in any case; `keyword` is upper case.
    fn is_word(&self, keyword: &str) -> bool {
        self.kind == Kind::Word && self.name.eq_ignore_ascii_case(keyword)
    }

    /// True for a bare word that `spells` the word `keyword`, where the servers don't reserve the
    /// word and so compare it by collation (DESIGN.md §7).
    fn spells_word(&self, keyword: &str) -> bool {
        self.kind == Kind::Word && spells(self.name, keyword)
    }

    /// True for a name, bare or quoted, that `spells` `name`, as any object's name is compared.
    fn spells_name(&self, name: &str) -> bool {
        matches!(self.kind, Kind::Word | Kind::Quoted) && spells(self.name, name)
    }
}

/// What a comparison by collation makes of a character, as near as sqlh can tell without the
/// collation (DESIGN.md §7): a full-width form is its ASCII letter, and what is neither a letter
/// nor a number beyond ASCII (a joiner, U+FEFF) is nothing.
fn folded(c: char) -> Option<char> {
    match c {
        c if c.is_ascii() => Some(c),
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0),
        c if c.is_alphanumeric() => Some(c),
        _ => None,
    }
}

/// Whether the servers could read `name` as `keyword` (upper case), comparing by collation: in any
/// case, with trailing spaces (`[sys ]` is `sys`), and `folded`. Only for what a match refuses, or
/// reports: a match that lets something through would make a new way past the guard.
fn spells(name: &str, keyword: &str) -> bool {
    let name = name.trim_end_matches(' ');
    if name.eq_ignore_ascii_case(keyword) {
        return true;
    }
    // Folding only ever shortens, so a word this short costs no second look.
    if name.len() <= keyword.len() || name.is_ascii() {
        return false;
    }
    let mut letters = name.chars().filter_map(folded);
    keyword
        .chars()
        .all(|k| letters.next().is_some_and(|c| c.eq_ignore_ascii_case(&k)))
        && letters.all(|c| c == ' ')
}

/// The entry of `list` (upper case) that `name` spells in any case.
fn find_entry<'l>(list: &[&'l str], name: &str) -> Option<&'l str> {
    list.iter()
        .copied()
        .find(|entry| entry.eq_ignore_ascii_case(name))
}

fn in_list(list: &[&str], name: &str) -> bool {
    find_entry(list, name).is_some()
}

/// The longest word the main loop acts on: OPENDATASOURCE, with 14 letters.
const KEYWORD_MAX: usize = 16;

/// `name` in ASCII upper case, `folded`, for a `match` on keywords whose arms can only refuse. A
/// word too long to be one gives "", which no arm matches, so a long word costs nothing, and so
/// does a word with a letter beyond ASCII in it, which no keyword has (`UPDATE时间`).
fn keyword<'b>(name: &str, buf: &'b mut [u8; KEYWORD_MAX]) -> &'b str {
    let len = if name.is_ascii() {
        let Some(room) = buf.get_mut(..name.len()) else {
            return "";
        };
        room.copy_from_slice(name.as_bytes());
        name.len()
    } else {
        let mut len = 0;
        for c in name.chars().filter_map(folded) {
            let (true, Some(slot)) = (c.is_ascii(), buf.get_mut(len)) else {
                return "";
            };
            *slot = c as u8;
            len += 1;
        }
        len
    };
    let Some(room) = buf.get_mut(..len) else {
        return "";
    };
    room.make_ascii_uppercase();
    // Upper-casing ASCII letters keeps valid UTF-8 valid, so this does not fail.
    std::str::from_utf8(room).unwrap_or("")
}

/// Words that block outright in read-only mode.
const BLOCKED: &[&str] = &[
    "BACKUP",
    "BULK",
    "CHECKPOINT",
    "COMMIT",
    "CONVERSATION",
    "DENY",
    "DISABLE",
    "ENABLE",
    "GRANT",
    "KILL",
    "MERGE",
    "OPENDATASOURCE",
    "OPENQUERY",
    "OPENROWSET",
    "RECEIVE",
    "RECONFIGURE",
    "RESTORE",
    "REVOKE",
    "ROLLBACK",
    "SAVE",
    "SEND",
    "SETUSER",
    "SHUTDOWN",
    "UPDATETEXT",
    "WAITFOR",
    "WRITETEXT",
];

/// Read-only system procedures that may be executed, each named exactly: a prefix such as
/// `xp_` would also admit `xp_cmdshell` and `xp_instance_regwrite`. A procedure is here
/// because it only reads, not because a login may run it: that is the server's call (PLAN.md,
/// Phase 6). Those that write when given one argument are in `ARG_CHECKS` too.
const PROCS: &[&str] = &[
    "SP_COLUMNS",
    "SP_CONFIGURE",
    "SP_DATABASES",
    "SP_DATATYPE_INFO",
    "SP_DEPENDS",
    "SP_DESCRIBE_FIRST_RESULT_SET",
    "SP_DESCRIBE_UNDECLARED_PARAMETERS",
    "SP_ENUMERRORLOGS",
    "SP_FKEYS",
    "SP_HELP",
    "SP_HELPCONSTRAINT",
    "SP_HELPDB",
    "SP_HELPFILE",
    "SP_HELPFILEGROUP",
    "SP_HELPINDEX",
    "SP_HELPROLE",
    "SP_HELPROLEMEMBER",
    "SP_HELPROTECT",
    "SP_HELPSTATS",
    "SP_HELPTEXT",
    "SP_HELPTRIGGER",
    "SP_HELPUSER",
    "SP_LOCK",
    "SP_PKEYS",
    "SP_READERRORLOG",
    "SP_SERVER_INFO",
    "SP_SPACEUSED",
    "SP_SPECIAL_COLUMNS",
    "SP_SPROC_COLUMNS",
    "SP_STORED_PROCEDURES",
    "SP_TABLES",
    "SP_WHO",
    "SP_WHO2",
    "XP_FIXEDDRIVES",
    "XP_INSTANCE_REGREAD",
    "XP_MSVER",
    "XP_READERRORLOG",
];

/// An allowlisted procedure that writes when it is given one argument, named or as the second
/// positional one.
struct ArgCheck {
    /// The procedure, upper case, as in `PROCS`.
    name: &'static str,
    /// The argument that makes it write, upper case.
    param: &'static str,
    /// Why the call is refused when it passes the argument.
    reason: &'static str,
    /// What to do instead.
    instead: &'static str,
}

const ARG_CHECKS: &[ArgCheck] = &[
    ArgCheck {
        name: "SP_CONFIGURE",
        param: "@CONFIGVALUE",
        reason: "sp_configure with @configvalue (named, or as the second argument) changes a \
                 server setting",
        instead: "Read sys.configurations to see every setting, advanced ones too",
    },
    ArgCheck {
        name: "SP_SPACEUSED",
        param: "@UPDATEUSAGE",
        reason: "sp_spaceused with @updateusage (named, or as the second argument) runs DBCC \
                 UPDATEUSAGE, which writes",
        instead: "Leave @updateusage out to only read sizes",
    },
];

/// The words of `STATEMENT_START` that T-SQL doesn't reserve, so a procedure's argument may
/// be one of them written bare, as a string (`EXEC sp_configure GET, 1`). Where an argument's
/// value goes, `passes_argument` reads one as the value, not as the next statement. That can
/// only refuse more: the main loop still checks every token, and a statement one of them
/// starts there is refused anyway (`BLOCKED`, or `GET` and `MOVE`, which start only
/// `CONVERSATION` statements) or is no statement (`THROW` must follow a `;`, which ends the
/// scan).
const UNRESERVED_STARTS: &[&str] = &[
    "DISABLE", "ENABLE", "GET", "MOVE", "RECEIVE", "SEND", "THROW",
];

/// `sp_query_store_flush_db`, whose reason for being run is served by a read, so its refusal
/// points there and offers no flag (decision 23, command-categories.md §3.2, F3).
const FLUSH_PROC: &str = "SP_QUERY_STORE_FLUSH_DB";
const FLUSH_INSTEAD: &str = "Read sys.query_store_runtime_stats instead: it has the rows not yet \
                             flushed too; aggregate by plan_id, execution_type and \
                             runtime_stats_interval_id";

/// The function one call of which `wildcard_stats` refuses, by the last part of its name.
const PHYSICAL_STATS: &str = "DM_DB_INDEX_PHYSICAL_STATS";

/// What a call of `PHYSICAL_STATS` made of its arguments (`read_stats_call`).
#[derive(Debug, PartialEq, Eq)]
enum StatsCall {
    Other,
    /// All databases, in this mode.
    Wildcard(&'static str),
    /// Its arguments name the function more than `MAX_STATS_NAMED` times.
    TooNested,
}

/// Reads a call of `PHYSICAL_STATS`, from just past its `(`. Only the literal is looked for, as
/// decision 30(d) has it: an expression in the first or the fifth place is left to the server.
///
/// Only the second to the fourth arguments are read to their end. A scan stops at the first
/// token of the first or the fifth that isn't its literal, and a call's own `sys` is such a token,
/// so a scan reads past another call only in those three, where it counts them. So no token is
/// read by more than `MAX_STATS_NAMED + 1` scans and the work stays linear, as in
/// `passes_argument`.
fn read_stats_call(scan: Lexer<'_>) -> StatsCall {
    let mut depth = 0usize;
    let mut argument = 0;
    let mut literal_read = false;
    let mut mode = None;
    let mut named = 0;
    for t in scan {
        match t.kind {
            Kind::Punct('(') => depth += 1,
            Kind::Punct(')') if depth > 0 => depth -= 1,
            Kind::Punct(')') => return mode.map_or(StatsCall::Other, StatsCall::Wildcard),
            // Six arguments are the server's error to report.
            Kind::Punct(',') if depth == 0 && argument == 4 => return StatsCall::Other,
            Kind::Punct(',') if depth == 0 => {
                argument += 1;
                literal_read = false;
            }
            _ if (1..=3).contains(&argument) => {
                if t.spells_name(PHYSICAL_STATS) {
                    named += 1;
                    if named > MAX_STATS_NAMED {
                        return StatsCall::TooNested;
                    }
                }
            }
            // A second token makes it an expression (`NULL + 1`).
            _ if literal_read => return StatsCall::Other,
            _ if argument == 0 => {
                if !t.is_word("NULL") && !t.is_word("DEFAULT") {
                    return StatsCall::Other;
                }
                literal_read = true;
            }
            _ => {
                mode = stats_mode(&t);
                if mode.is_none() {
                    return StatsCall::Other;
                }
                literal_read = true;
            }
        }
    }
    StatsCall::Other
}

/// The mode that scans, if `token` is the literal of one. The server takes it in any case, with
/// trailing spaces, split by a backslash at a line's end, and as `"DETAILED"` under `SET
/// QUOTED_IDENTIFIER OFF`; with that on it would be a column, which a call over every database
/// has no reason to be given.
fn stats_mode(token: &Token<'_>) -> Option<&'static str> {
    let value = match token.kind {
        Kind::Str => token
            .text
            .strip_prefix(['N', 'n'])
            .unwrap_or(token.text)
            .strip_prefix('\'')?
            .strip_suffix('\'')?,
        Kind::Quoted if token.text.starts_with('"') => token.name,
        _ => return None,
    };
    let joined;
    let value = if value.contains('\\') {
        joined = value.replace("\\\r\n", "").replace("\\\n", "");
        &joined
    } else {
        value
    };
    // The function reads its mode up to a NUL: `N'DETAILED' + NCHAR(0) + N'zz'` scans.
    let value = value.split('\0').next().unwrap_or(value);
    ["DETAILED", "SAMPLED"]
        .into_iter()
        .find(|mode| spells(value, mode))
}

/// The refusal of `sys.dm_db_index_physical_stats` over every database in a mode that scans
/// (decision 30(d), command-categories.md §6.10). It isn't a write, so no flag is offered in any
/// run: the rewrite is what gets past it. `-t` wouldn't do: it stops sqlh's wait, not the I/O and
/// the buffer-pool churn the call has caused by then.
fn wildcard_stats(line: u32, mode: &str) -> Blocked {
    // `SAMPLED` reads a 1% sample of an index, and the whole of one under 10,000 pages (the
    // function's documentation), so "every page" would be false of it.
    let reads = if mode == "SAMPLED" {
        "at least 1% of the pages"
    } else {
        "every page"
    };
    Blocked {
        line,
        reason: format!(
            "sys.dm_db_index_physical_stats(NULL, ..., '{mode}') would read {reads} of every \
             index in every database"
        ),
        advice: "Name the database and object, or use 'LIMITED'".into(),
        lifted_by_write: false,
        instead: None,
    }
}

/// The session state `SET`, at `after`, would leave, if it is one the reset isn't known to clear.
fn set_of_unknown_state(after: &Lexer<'_>) -> Option<&'static str> {
    // A reserved word, so no column: `SET identity_insert = 1` is already the server's syntax
    // error (156 on SQL Server 2016 SP3 and 2025).
    if after.word_is(0, "IDENTITY_INSERT") {
        return Some("SET IDENTITY_INSERT");
    }
    if !after.spells(0, "AUTOPILOT") {
        return None;
    }
    // Not reserved, so `UPDATE #t SET autopilot = 1`, `+=` and `.modify(...)` are a column's. A
    // comma never follows a column there, and the servers take `SET AUTOPILOT, AUTOPILOT ON`;
    // with any other option in the list, or AUTOPILOT not first, they refuse it (1067 or 195).
    match after.kind(1) {
        Some(Kind::Punct(c)) if c != ',' => None,
        _ => Some("SET AUTOPILOT"),
    }
}

/// The key `OPEN`, at `after`, would leave open, if it opens one. The servers take these bare
/// words only (`OPEN [MASTER] KEY` is a syntax error), and `OPEN [master]` is a cursor's.
fn open_of_key(after: &Lexer<'_>) -> Option<&'static str> {
    if !after.spells(1, "KEY") {
        return None;
    }
    if after.spells(0, "SYMMETRIC") {
        Some("OPEN SYMMETRIC KEY")
    } else if after.spells(0, "MASTER") {
        Some("OPEN MASTER KEY")
    } else {
        None
    }
}

/// The refusal in `sqlh mcp` of state that could reach the next call on the connection (decision
/// 30(e), command-categories.md §6.3), until a reset test covers it. The CLI's session ends with
/// its run, so the CLI runs these. `advice_for` shows the advice as it is: it names no flag.
fn session_state(line: u32, what: &str) -> Blocked {
    Blocked {
        line,
        reason: format!("{what} leaves session state sqlh mcp's reset isn't known to clear"),
        advice: "Run it with the sqlh CLI, whose session ends with the run".into(),
        lifted_by_write: false,
        instead: None,
    }
}

/// DBCC commands that only read.
const DBCC_READ: &[&str] = &[
    "INPUTBUFFER",
    "OPENTRAN",
    "SHOW_STATISTICS",
    "TRACESTATUS",
    "USEROPTIONS",
];

/// What to read in place of a DBCC command not in `DBCC_READ` (command-categories.md §6.9). The
/// guard runs before connecting, so it can't ask the server's version, and the refusal states
/// what the reader may not have or may not find in the docs. Each was run on SQL Server 2016 SP3
/// and 2025, except `sys.dm_db_page_info`, which 2016 lacks (decision 49).
const DBCC_INSTEAD: &[(&str, &str)] = &[
    ("DBINFO", "DATABASEPROPERTYEX(name, 'LastGoodCheckDbTime')"),
    ("IND", "sys.dm_db_database_page_allocations (undocumented)"),
    ("LOGINFO", "sys.dm_db_log_info"),
    ("PAGE", "sys.dm_db_page_info (SQL Server 2019 and later)"),
    ("PROCCACHE", "sys.dm_exec_cached_plans"),
    ("SHOWCONTIG", "sys.dm_db_index_physical_stats"),
    ("SHOWFILESTATS", "sys.dm_db_file_space_usage"),
];

/// The statements `ALTER` and `CREATE` make, for a refusal to name one (decision 23). An entry
/// that begins another comes after it, as the first match is the one named. Only these are
/// ever shown, never the input's words, so what follows `ALTER` can't turn up as a statement.
const DDL_STATEMENTS: &[&str] = &[
    "AGGREGATE",
    "APPLICATION ROLE",
    "ASSEMBLY",
    "ASYMMETRIC KEY",
    "AUTHORIZATION",
    "AVAILABILITY GROUP",
    "BROKER PRIORITY",
    "CERTIFICATE",
    "COLUMN ENCRYPTION KEY",
    "COLUMN MASTER KEY",
    "COLUMNSTORE INDEX",
    "CONTRACT",
    "CREDENTIAL",
    "CRYPTOGRAPHIC PROVIDER",
    "DATABASE AUDIT SPECIFICATION",
    "DATABASE ENCRYPTION KEY",
    "DATABASE SCOPED CONFIGURATION",
    "DATABASE SCOPED CREDENTIAL",
    "DATABASE",
    "DEFAULT",
    "ENDPOINT",
    "EVENT NOTIFICATION",
    "EVENT SESSION",
    "EXTERNAL DATA SOURCE",
    "EXTERNAL FILE FORMAT",
    "EXTERNAL LANGUAGE",
    "EXTERNAL LIBRARY",
    "EXTERNAL RESOURCE POOL",
    "EXTERNAL TABLE",
    "FULLTEXT CATALOG",
    "FULLTEXT INDEX",
    "FULLTEXT STOPLIST",
    "FUNCTION",
    "INDEX",
    "LOGIN",
    "MASTER KEY",
    "MESSAGE TYPE",
    "PARTITION FUNCTION",
    "PARTITION SCHEME",
    "PRIMARY XML INDEX",
    "PROC",
    "PROCEDURE",
    "QUEUE",
    "REMOTE SERVICE BINDING",
    "RESOURCE GOVERNOR",
    "RESOURCE POOL",
    "ROLE",
    "ROUTE",
    "RULE",
    "SCHEMA",
    "SEARCH PROPERTY LIST",
    "SECURITY POLICY",
    "SELECTIVE XML INDEX",
    "SEQUENCE",
    "SERVER AUDIT SPECIFICATION",
    "SERVER AUDIT",
    "SERVER CONFIGURATION",
    "SERVER ROLE",
    "SERVICE MASTER KEY",
    "SERVICE",
    "SPATIAL INDEX",
    "STATISTICS",
    "SYMMETRIC KEY",
    "SYNONYM",
    "TRIGGER",
    "TYPE",
    "USER",
    "VIEW",
    "WORKLOAD CLASSIFIER",
    "WORKLOAD GROUP",
    "XML INDEX",
    "XML SCHEMA COLLECTION",
];

/// The most words an entry of `DDL_STATEMENTS` has.
const DDL_WORDS: usize = 3;

/// `verb` and the statement of `DDL_STATEMENTS` the input names at `after`, or `verb` alone. It
/// reads a few tokens once, and only on the way to a refusal, which ends the pass.
fn ddl_statement(verb: &str, after: &Lexer<'_>) -> String {
    let mut lex = after.clone();
    let written: [&str; DDL_WORDS] = std::array::from_fn(|_| {
        lex.next()
            .filter(|t| t.kind == Kind::Word)
            .map_or("", |t| t.name)
    });
    let statement = DDL_STATEMENTS.iter().find(|statement| {
        statement
            .split(' ')
            .zip(written)
            .all(|(word, seen)| word.eq_ignore_ascii_case(seen))
    });
    match statement {
        Some(statement) => format!("{verb} {statement}"),
        None => verb.to_owned(),
    }
}

/// Words that can start a statement. A batch that starts with any other word is an
/// implicit procedure call (`sp_who2` alone runs the procedure).
const STATEMENT_START: &[&str] = &[
    "ALTER",
    "BACKUP",
    "BEGIN",
    "BREAK",
    "BULK",
    "CHECKPOINT",
    "CLOSE",
    "COMMIT",
    "CONTINUE",
    "CREATE",
    "DBCC",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DENY",
    "DISABLE",
    "DROP",
    "ELSE",
    "ENABLE",
    "END",
    "EXEC",
    "EXECUTE",
    "FETCH",
    "GET",
    "GOTO",
    "GRANT",
    "IF",
    "INSERT",
    "KILL",
    "MERGE",
    "MOVE",
    "OPEN",
    "PRINT",
    "RAISERROR",
    "READTEXT",
    "RECEIVE",
    "RECONFIGURE",
    "RESTORE",
    "RETURN",
    "REVERT",
    "REVOKE",
    "ROLLBACK",
    "SAVE",
    "SELECT",
    "SEND",
    "SET",
    "SETUSER",
    "SHUTDOWN",
    "THROW",
    "TRUNCATE",
    "UPDATE",
    "UPDATETEXT",
    "USE",
    "WAITFOR",
    "WHILE",
    "WITH",
    "WRITETEXT",
];

/// The classifier's work budget (DESIGN.md §7, plan-history.md R18), the backstop under the linear
/// scans below. Every token the classifier reads costs one unit, lookahead included, so
/// reading the same suffix again is paid for again; a read also costs one unit for each 32
/// bytes it went over, so a huge string or comment that a lookahead crosses over and over is
/// not free. Each pass over a batch (the write classifier, and `--check`'s NOEXEC search) gets
/// `WORK_BASE` plus `WORK_PER_BYTE` for each byte of the text. A plain pass costs at most 1
/// unit a byte (a token of one byte) and the costliest input found, a 10 MB `DROP TABLE` list,
/// 1.7, so no ordinary input comes near it: it is there for the shape nobody thought of. For
/// the largest batch it is 32 million units, which took 0.4 s at the fastest rate measured
/// (75 million units a second) and 1 s at the slowest (31 million): DESIGN.md §11,
/// notes/memory.md.
const WORK_BASE: u64 = 2_000_000;
const WORK_PER_BYTE: u64 = 3;

/// How many `TOP ( ... )` clauses of INSERT, UPDATE and DELETE may be open at once. The
/// classifier remembers each one until its `)` shows what the statement writes to, which is
/// how it reads the input once however deeply they nest. A real statement nests a few.
const MAX_OPEN_TOP: usize = 16_384;

/// How many parentheses (opening and closing) the arguments of a procedure in `ARG_CHECKS`,
/// which takes none, may have.
const MAX_ARG_PARENS: usize = 8;

/// How many times the second to the fourth arguments of `PHYSICAL_STATS` may name it. Nothing
/// real does this at all; the limit is what keeps `read_stats_call` linear.
const MAX_STATS_NAMED: usize = 8;

/// How much work a batch's classification has done, shared by every `Lexer` copy made for it.
struct Work {
    used: Cell<u64>,
    limit: u64,
    /// The line the work ran out on.
    line: Cell<u32>,
}

impl Work {
    /// The budget of one pass over `sql`: `base` plus `per_byte` for each byte.
    fn new(sql: &str, base: u64, per_byte: u64) -> Self {
        Work {
            used: Cell::new(0),
            limit: base.saturating_add(per_byte.saturating_mul(sql.len() as u64)),
            line: Cell::new(1),
        }
    }

    fn exceeded(&self) -> bool {
        self.used.get() > self.limit
    }

    /// Pays `units` for a read that ended on `line`.
    fn charge(&self, units: u64, line: u32) {
        let before = self.used.get();
        self.used.set(before.saturating_add(units));
        if before <= self.limit && self.exceeded() {
            self.line.set(line);
        }
    }

    /// The refusal for a batch whose classification used more than its budget.
    fn refusal(&self) -> Blocked {
        too_complex(
            self.line.get(),
            &format!(
                "it would take more than {} token reads to classify, which is the most one \
                 batch of this size is given",
                self.limit
            ),
        )
    }
}

/// The refusal for input the read-only check can't classify in bounded work. `--allow-write`
/// skips the classifier, so it lifts this one, as it does the others.
fn too_complex(line: u32, why: &str) -> Blocked {
    Blocked::write_or(
        line,
        format!("this input is too complex for the read-only check: {why}"),
        "Simplify it or split it into smaller batches with GO".into(),
        " if the write is intended",
    )
}

pub fn check(sql: &str, policy: Policy) -> Result<(), Blocked> {
    check_within(sql, policy, WORK_BASE, WORK_PER_BYTE)
}

/// `check` with the work budget of each pass spelled out, so that tests can spend one.
fn check_within(sql: &str, policy: Policy, base: u64, per_byte: u64) -> Result<(), Blocked> {
    if policy.is_open() {
        return Ok(());
    }
    if policy.compile_only {
        metered(sql, policy, base, per_byte, |guard| {
            guard.check_compile_only()
        })?;
    }
    if !policy.writes {
        metered(sql, policy, base, per_byte, |guard| guard.check_writes())?;
    }
    Ok(())
}

/// Runs one pass of the classifier over `sql` with a budget of its own. Once the budget is
/// spent every lexer reports the end of the input, so whatever the pass made of that is not an
/// answer: it is replaced by the refusal. A refusal found before then has not spent the
/// budget, and stands.
fn metered(
    sql: &str,
    policy: Policy,
    base: u64,
    per_byte: u64,
    pass: impl FnOnce(&Guard<'_>) -> Result<(), Blocked>,
) -> Result<(), Blocked> {
    let work = Work::new(sql, base, per_byte);
    let result = pass(&Guard {
        sql,
        work: &work,
        compile_only: policy.compile_only,
        reused_session: policy.reused_session,
    });
    if work.exceeded() {
        return Err(work.refusal());
    }
    result
}

/// Whether `sql` has the bare word `XACT_ABORT` among its tokens, which is how `SET XACT_ABORT`
/// is written (a comment, a string or a `[bracketed]` name is not one). Not a guard check: a
/// run whose input says it counts rows exactly (DESIGN.md §4.4), because with XACT_ABORT ON an
/// attention rolls the transaction back, and a counting that stopped early would end with the
/// wrapper transaction gone. Dynamic SQL in a string is not seen; that run is cut like any
/// other. One pass over the text, so it needs no budget.
pub fn mentions_xact_abort(sql: &str) -> bool {
    Lexer::new(sql).any(|t| t.spells_word("XACT_ABORT"))
}

/// The words of `STATEMENT_START` whose statements return nothing a reader would miss: no rows,
/// no messages. A stopped count that cancels one of them loses nothing it would show.
const SILENT_STARTS: &[&str] = &[
    "BEGIN",
    "BREAK",
    "CLOSE",
    "COMMIT",
    "CONTINUE",
    "DEALLOCATE",
    "DECLARE",
    "ELSE",
    "END",
    "IF",
    "OPEN",
    "RETURN",
    "REVERT",
    "ROLLBACK",
    "SAVE",
    "SET",
    "USE",
];

/// The words that make output come several times: a procedure's results, a loop's (`WHILE`,
/// `GOTO`), which repeats the statements in it, and a `DBCC` command's (`SHOW_STATISTICS`
/// returns its header, density vector and histogram as three results; R48). Each counts as
/// two.
const REPEATING_STARTS: &[&str] = &["DBCC", "EXEC", "EXECUTE", "GOTO", "WHILE"];

/// Whether `token`, after a `SET`, names a session option that adds output to the statements
/// after it: `STATISTICS` (`PROFILE` and `XML` add a result after each query's own, `IO` and
/// `TIME` messages) or a `SHOWPLAN_` option, whose plans come in place of each statement's
/// results (R48). `OFF` matches as well as `ON`: erring toward more output is the safe way.
fn adds_output(token: Token<'_>) -> bool {
    token.is_word("STATISTICS")
        || (token.kind == Kind::Word
            && token
                .name
                .get(..9)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("SHOWPLAN_")))
}

/// Whether `sql` sets a session option that adds output (`adds_output`, R48): it lasts for the
/// rest of the session, so the batches after this one can show more than their statements say,
/// and a stopped count in any of them could cancel it (`run::run_opened`). One pass over the
/// text, like `mentions_xact_abort`.
pub fn sets_output_modes(sql: &str) -> bool {
    let mut lex = Lexer::new(sql);
    while let Some(token) = lex.next() {
        if token.is_word("SET") && lex.ahead(0).is_some_and(adds_output) {
            return true;
        }
    }
    false
}

/// Whether more than one of the statements in `sql` can return rows or messages (decision 45,
/// DESIGN.md §4.4). A stopped count of hidden rows cancels the rest of its batch, so a batch
/// with one such statement loses nothing to it, and one with more gets the longer count
/// (`db::LATER_STATEMENTS_BUDGET`) so its later statements still run.
///
/// It errs toward yes, never toward no: a wrong yes costs a few seconds of counting, a wrong no
/// would cancel statements unseen. At parenthesis depth 0 it counts each word that starts a
/// statement, except the `SILENT_STARTS`; one of the `REPEATING_STARTS`, a `SET` of an option
/// that adds output (`adds_output`, R48), or a batch that is an implicit procedure call, counts
/// as two, since a procedure can return several results, a loop repeats its statements' output,
/// and such an option adds output to the statements after it (an earlier batch's is
/// `sets_output_modes`). Two words continue a
/// statement and aren't counted: the query after a common table expression (`WITH c AS (...)
/// SELECT`), and a `SELECT` after `UNION`, `EXCEPT` or `INTERSECT` (and `ALL`). A `WITH` starts
/// a common table expression only at the start of the batch or after `;`, `BEGIN` or `ELSE`,
/// as T-SQL requires; elsewhere it is a table hint, `ROLLUP`, `TIES` and the like. One pass
/// over the text, like `mentions_xact_abort`, so it needs no budget.
pub fn several_outputs(sql: &str) -> bool {
    let start = Lexer::new(sql);
    let mut outputs = match start.ahead(0) {
        Some(first) if first.kind == Kind::Quoted => 2,
        Some(first)
            if first.kind == Kind::Word
                && !in_list(STATEMENT_START, first.name)
                && start.kind(1) != Some(Kind::Punct(':')) =>
        {
            2
        }
        _ => 0,
    };
    let mut depth = 0usize;
    // Where a `WITH` may start a common table expression: the batch's start, or after `;`,
    // `BEGIN` or `ELSE`.
    let mut statement_boundary = true;
    let mut cte_query_next = false;
    let mut set_operand_next = false;
    let mut lex = start;
    while let Some(token) = lex.next() {
        let boundary = statement_boundary;
        statement_boundary = false;
        match token.kind {
            Kind::Punct('(') => {
                // A query in parentheses where a statement starts: `(SELECT 1) UNION ...`.
                if boundary && depth == 0 {
                    outputs += 1;
                }
                depth += 1;
                set_operand_next = false;
            }
            Kind::Punct(')') => depth = depth.saturating_sub(1),
            Kind::Punct(';') => statement_boundary = depth == 0,
            Kind::Word if depth == 0 => {
                if token.is_word("BEGIN") || token.is_word("ELSE") {
                    statement_boundary = true;
                }
                if token.is_word("UNION") || token.is_word("EXCEPT") || token.is_word("INTERSECT") {
                    set_operand_next = true;
                    continue;
                }
                if token.is_word("ALL") && set_operand_next {
                    continue;
                }
                let continues = (set_operand_next && token.is_word("SELECT"))
                    || (cte_query_next
                        && in_list(
                            &["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE"],
                            token.name,
                        ));
                set_operand_next = false;
                if continues {
                    cte_query_next = false;
                    continue;
                }
                if token.is_word("WITH") {
                    if boundary {
                        cte_query_next = true;
                        outputs += 1;
                    }
                } else if in_list(REPEATING_STARTS, token.name)
                    || (token.is_word("SET") && lex.ahead(0).is_some_and(adds_output))
                {
                    outputs += 2;
                } else if in_list(STATEMENT_START, token.name)
                    && !in_list(SILENT_STARTS, token.name)
                {
                    outputs += 1;
                }
            }
            _ => set_operand_next = false,
        }
        if outputs >= 2 {
            return true;
        }
    }
    false
}

/// What in a batch's text asks to read uncommitted data (`uncommitted_reads`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UncommittedReads {
    /// A table hint `NOLOCK`.
    pub nolock: bool,
    /// A table hint `READUNCOMMITTED`.
    pub readuncommitted: bool,
    /// `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED`.
    pub isolation: bool,
}

impl UncommittedReads {
    pub fn add(&mut self, other: UncommittedReads) {
        self.nolock |= other.nolock;
        self.readuncommitted |= other.readuncommitted;
        self.isolation |= other.isolation;
    }

    /// Records the hint `name` spells, if it does (`[NOLOCK ]`, as the server reads it).
    fn hint(&mut self, name: &str) {
        if spells(name, "NOLOCK") {
            self.nolock = true;
        } else if spells(name, "READUNCOMMITTED") {
            self.readuncommitted = true;
        }
    }
}

/// The table hints, for what may follow `NOLOCK` in a `WITH (...)` written with spaces
/// (`WITH (NOLOCK INDEX(1))`, deprecated but accepted). A column of `OPENJSON(...) WITH (nolock
/// int)` is followed by its type instead.
const TABLE_HINTS: &[&str] = &[
    "FORCESCAN",
    "FORCESEEK",
    "HOLDLOCK",
    "IGNORE_CONSTRAINTS",
    "IGNORE_TRIGGERS",
    "INDEX",
    "KEEPDEFAULTS",
    "KEEPIDENTITY",
    "NOEXPAND",
    "NOLOCK",
    "NOWAIT",
    "PAGLOCK",
    "READCOMMITTED",
    "READCOMMITTEDLOCK",
    "READPAST",
    "READUNCOMMITTED",
    "REPEATABLEREAD",
    "ROWLOCK",
    "SERIALIZABLE",
    "SNAPSHOT",
    "SPATIAL_WINDOW_MAX_CELLS",
    "TABLOCK",
    "TABLOCKX",
    "UPDLOCK",
    "XLOCK",
];

/// Reserved words that can follow a table's name, so none of them is its alias. `FROM t WHERE
/// (nolock) = 1` has no hint, nor has any statement that follows the table (`STATEMENT_START`).
const NOT_AN_ALIAS: &[&str] = &[
    "AND",
    "BY",
    "CASE",
    "CROSS",
    "EXCEPT",
    "FOR",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INTERSECT",
    "INTO",
    "IS",
    "LEFT",
    "LIKE",
    "NOT",
    "ON",
    "OPTION",
    "OR",
    "ORDER",
    "OUTER",
    "OUTPUT",
    "OVER",
    "PIVOT",
    "RIGHT",
    "TABLESAMPLE",
    "UNION",
    "UNPIVOT",
    "VALUES",
    "WHERE",
];

fn is_reserved(name: &str) -> bool {
    in_list(STATEMENT_START, name) || in_list(NOT_AN_ALIAS, name)
}

/// Where the scan of `uncommitted_reads` stands in naming a table. Only a table's name or alias
/// can take a hint without `WITH`, and `name (x)` means something else everywhere else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableSource {
    Off,
    /// Past `FROM`, `JOIN`, `USING`, or a comma between tables: a table's name comes next.
    Expected,
    Name,
    Dot,
    As,
    Alias,
    /// Past what can take a bare hint (a parenthesis, a `WITH`, a table variable): an alias may
    /// still come, and a comma still leads to the next table.
    Past,
}

impl TableSource {
    /// The state after `token` at the parenthesis depth of the table list, which isn't a
    /// parenthesis or a word that starts a list.
    fn after(self, token: Token<'_>) -> TableSource {
        use TableSource::*;
        match (self, token.kind) {
            (Off, _) => Off,
            // `tempdb..#t` and `dbo.#t` name the session's table too.
            (Dot, Kind::Word | Kind::Quoted | Kind::Temp) => Name,
            (Expected, Kind::Word) if is_reserved(token.name) => Off,
            (Expected, Kind::Word | Kind::Quoted | Kind::Temp) => Name,
            // `FROM @t (NOLOCK)` is a syntax error (1018), so it never reads.
            (Expected, Kind::Var) => Past,
            (Name, Kind::Word) if token.is_word("AS") => As,
            (Name, Kind::Word) if is_reserved(token.name) => Off,
            (Name | As, Kind::Word | Kind::Quoted) => Alias,
            (Past, Kind::Word) if !token.is_word("AS") && is_reserved(token.name) => Off,
            (Past, Kind::Word | Kind::Quoted) => Past,
            (Name | Dot, Kind::Punct('.')) => Dot,
            (Name | Alias | Past, Kind::Punct(',')) => Expected,
            _ => Off,
        }
    }
}

/// Whether a batch is the definition of a module: `CREATE [OR ALTER]` or `ALTER` of one. Such a
/// statement must start its batch, and it stores its text without running it.
fn defines_a_module(start: &Lexer<'_>) -> bool {
    let modules = ["FUNCTION", "PROC", "PROCEDURE", "TRIGGER", "VIEW"];
    let is_module = |n| start.word(n).is_some_and(|w| in_list(&modules, w));
    if start.word_is(0, "ALTER") {
        is_module(1)
    } else if start.word_is(0, "CREATE") {
        is_module(1) || (start.word_is(1, "OR") && start.word_is(2, "ALTER") && is_module(3))
    } else {
        false
    }
}

/// What in `sql` asks to read uncommitted data, for the run's note (decision 30(a), DESIGN.md
/// §4.3). Not a guard check: nothing is refused, and it runs with writes allowed too, where the
/// classifier doesn't.
///
/// A false note says rows may be dirty that aren't, so a word counts only where the server takes
/// it as a hint, as checked on SQL Server 2016 and 2025:
/// - `WITH (...)`, a word of that list at its own depth. `INDEX = nolock` names an index, and an
///   `OPENJSON` column is followed by its type.
/// - `t (NOLOCK)` or `t a (NOLOCK)` with nothing else in the parentheses, the only form the
///   server takes without `WITH`, after `FROM`, `JOIN`, `USING` or a comma that follows such a
///   table. `t nolock` is an alias, and reads committed. After `APPLY`, `f(nolock)` is more often
///   a function given a column, so that form isn't looked for there.
/// - `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` (or `TRAN`).
///
/// A `FROM` that is part of an expression starts no table list: `TRIM(x FROM y)` and `a IS
/// DISTINCT FROM b`.
///
/// The text is all it has: `FROM f(nolock)` with a column of an outer query as the argument
/// counts, a table list that goes on past a derived table or an `ON` doesn't, and neither does
/// dynamic SQL or a module's own hints. One pass over the text, with lookahead of a few tokens
/// and only on rare tokens, like `several_outputs`, so it needs no budget.
pub fn uncommitted_reads(sql: &str) -> UncommittedReads {
    let mut found = UncommittedReads::default();
    let mut lex = Lexer::new(sql);
    if defines_a_module(&lex) {
        return found;
    }
    let mut depth = 0usize;
    let mut source = TableSource::Off;
    let mut source_depth = 0usize;
    // The depth inside the `WITH (` being read.
    let mut hints: Option<usize> = None;
    let mut after_with = false;
    let mut after_equals = false;
    let mut after_trim = false;
    let mut after_distinct = false;
    // A bit per depth, set when the parenthesis that opened that depth is a `TRIM(`. Past 64 deep
    // a `TRIM(x FROM y)` reads as a table list, which at worst says too much (DESIGN.md §4.3).
    let mut trims = 0u64;
    let depth_bit = |depth: usize| 1u64.checked_shl(depth as u32).unwrap_or(0);
    while let Some(token) = lex.next() {
        let with = std::mem::take(&mut after_with);
        let trim = std::mem::take(&mut after_trim);
        let distinct = std::mem::take(&mut after_distinct);
        let equals = std::mem::replace(&mut after_equals, token.kind == Kind::Punct('='));
        let in_table_list = depth == source_depth;
        match token.kind {
            Kind::Punct('(') => {
                depth += 1;
                if trim {
                    trims |= depth_bit(depth);
                } else {
                    trims &= !depth_bit(depth);
                }
                if with && hints.is_none() {
                    hints = Some(depth);
                }
                if !in_table_list {
                    continue;
                }
                match source {
                    TableSource::Expected => source_depth = depth,
                    TableSource::Name | TableSource::Alias => {
                        if let Some(hint) = lex.ahead(0)
                            && matches!(hint.kind, Kind::Word | Kind::Quoted)
                            && lex.kind(1) == Some(Kind::Punct(')'))
                        {
                            found.hint(hint.name);
                        }
                        source = TableSource::Past;
                    }
                    TableSource::Past if with => {}
                    _ => source = TableSource::Off,
                }
            }
            Kind::Punct(')') => {
                if hints == Some(depth) {
                    hints = None;
                }
                if in_table_list {
                    source = TableSource::Off;
                }
                depth = depth.saturating_sub(1);
            }
            Kind::Word => {
                let mut buf = [0; KEYWORD_MAX];
                match keyword(token.name, &mut buf) {
                    "FROM" if distinct || trims & depth_bit(depth) != 0 => {}
                    "FROM" | "JOIN" | "USING" => {
                        source = TableSource::Expected;
                        source_depth = depth;
                        continue;
                    }
                    "TRIM" => after_trim = true,
                    "DISTINCT" => after_distinct = true,
                    "WITH" => {
                        after_with = true;
                        if in_table_list {
                            source = match source {
                                TableSource::Name | TableSource::Alias | TableSource::Past => {
                                    TableSource::Past
                                }
                                _ => TableSource::Off,
                            };
                        }
                        continue;
                    }
                    "NOLOCK" | "READUNCOMMITTED"
                        if hints == Some(depth)
                            && !equals
                            && lex.ahead(0).is_some_and(|next| {
                                matches!(next.kind, Kind::Punct(',' | ')'))
                                    || (next.kind == Kind::Word && in_list(TABLE_HINTS, next.name))
                            }) =>
                    {
                        found.hint(token.name);
                    }
                    "SET"
                        if lex
                            .word(0)
                            .is_some_and(|w| in_list(&["TRAN", "TRANSACTION"], w))
                            && lex.spells(1, "ISOLATION")
                            && lex.spells(2, "LEVEL")
                            && lex.spells(3, "READ")
                            && lex.spells(4, "UNCOMMITTED") =>
                    {
                        found.isolation = true;
                    }
                    _ => {}
                }
                if in_table_list {
                    source = source.after(token);
                }
            }
            _ => {
                if in_table_list {
                    source = source.after(token);
                }
            }
        }
    }
    found
}

/// The classifier reads the batch as a stream of tokens and keeps none of them: where it
/// needs to look ahead it reads on from a copy of its position (a `Lexer`). What it holds
/// therefore doesn't depend on how many tokens the input has (plan-history.md 2.14); only a
/// refusal's message can be as long as the name it quotes, and the open `TOP ( ... )` clauses
/// (at most `MAX_OPEN_TOP`, a few bytes each). Every read is paid for out of `work`.
struct Guard<'a> {
    sql: &'a str,
    work: &'a Work,
    /// `Policy::compile_only`.
    compile_only: bool,
    /// `Policy::reused_session`.
    reused_session: bool,
}

#[derive(Clone, Copy, Debug)]
enum Write {
    Insert,
    Update,
    Delete,
}

/// An INSERT, UPDATE or DELETE whose `TOP (` clause has not closed yet.
struct OpenTop {
    write: Write,
    /// The line of the write keyword, which is where a refusal points.
    line: u32,
    /// Where the keyword ends in the text: the order refusals are put in.
    at: usize,
    /// The parenthesis depth inside the clause, which its closing `)` leaves.
    depth: usize,
}

/// Keeps `blocked` as the refusal to give if it is the earliest in the text (`at` is where
/// its keyword ends).
fn hold(held: &mut Option<(usize, Blocked)>, at: usize, blocked: Blocked) {
    if held.as_ref().is_none_or(|(first, _)| at < *first) {
        *held = Some((at, blocked));
    }
}

impl<'a> Guard<'a> {
    fn lexer(&self) -> Lexer<'a> {
        Lexer::metered(self.sql, self.work)
    }

    /// What `--allow-write` would do with SQL sqlh doesn't know. `--check --allow-write` still
    /// only compiles (`RunPolicy::wrap`), so under `--check` the advice can't promise a run.
    fn as_written(&self) -> &'static str {
        if self.compile_only {
            " to compile it as written"
        } else {
            " to run it as written"
        }
    }

    /// Under `--check`, input must not touch NOEXEC: `SET NOEXEC OFF` would let the rest of
    /// the batch, and every later batch, run for real.
    fn check_compile_only(&self) -> Result<(), Blocked> {
        match self.lexer().find(|t| t.spells_word("NOEXEC")) {
            Some(token) => Err(Blocked {
                line: token.line,
                reason: "SET NOEXEC can't be used with --check: switching it off would run the \
                         SQL instead of only compiling it"
                    .into(),
                advice: "Remove SET NOEXEC (--check sets it already); write a column named \
                         NOEXEC as [NOEXEC]. --allow-write doesn't lift this"
                    .into(),
                lifted_by_write: false,
                instead: Some(
                    "Remove SET NOEXEC (the check tool sets it already); write a column named \
                     NOEXEC as [NOEXEC]"
                        .into(),
                ),
            }),
            None => Ok(()),
        }
    }

    fn check_writes(&self) -> Result<(), Blocked> {
        let start = self.lexer();
        if let Some(first) = start.ahead(0) {
            let is_label = start.kind(1) == Some(Kind::Punct(':'));
            let implicit_call = match first.kind {
                Kind::Word => !in_list(STATEMENT_START, first.name) && !is_label,
                Kind::Quoted => true,
                _ => false,
            };
            if implicit_call {
                self.check_proc(first.line, &start, "a procedure call", true)?;
            }
        }
        // `lex` is always just past `token`, which is where every lookahead starts.
        let mut lex = start;
        // The `TOP (` clauses of INSERT, UPDATE and DELETE that are still open, outermost
        // first. A write's target follows its clause's `)`, so instead of scanning ahead to it
        // from every such keyword (quadratic when they nest), the single pass counts
        // parentheses and finishes each check when its clause closes.
        let mut open: Vec<OpenTop> = Vec::new();
        let mut depth = 0usize;
        // The refusal that comes first in the text. One found while a clause is open is held
        // until the clauses before it have closed, because one of them may refuse too, and the
        // refusal is the earliest keyword's, as it is when each target is looked up at once.
        let mut held: Option<(usize, Blocked)> = None;
        while let Some(token) = lex.next() {
            match token.kind {
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => {
                    if open.last().is_some_and(|top| top.depth == depth)
                        && let Some(top) = open.pop()
                        && let Err(blocked) = self.finish_write(top.write, top.line, &lex)
                    {
                        hold(&mut held, top.at, blocked);
                    }
                    depth = depth.saturating_sub(1);
                }
                // Once something is held, a later keyword can't come first.
                Kind::Word | Kind::Quoted if held.is_none() => {
                    let checked = if token.kind == Kind::Word {
                        self.classify(&token, &lex, depth, &mut open)
                    } else {
                        self.check_physical_stats(&token, &lex)
                    };
                    if let Err(blocked) = checked {
                        if open.is_empty() {
                            return Err(blocked);
                        }
                        hold(&mut held, lex.pos, blocked);
                    }
                }
                _ => {}
            }
            if open.is_empty()
                && let Some((_, blocked)) = held.take()
            {
                return Err(blocked);
            }
        }
        // Clauses never closed read to the end of the input and found no target.
        while let Some(top) = open.pop() {
            if let Err(blocked) = self.finish_write(top.write, top.line, &lex) {
                hold(&mut held, top.at, blocked);
            }
        }
        match held {
            Some((_, blocked)) => Err(blocked),
            None => Ok(()),
        }
    }

    /// Classifies one word of the batch; `lex` is just past it. A write with a `TOP (` clause
    /// is checked when the clause closes: it is added to `open` here (`depth` is the
    /// parenthesis depth before the clause's own).
    fn classify(
        &self,
        token: &Token<'a>,
        lex: &Lexer<'a>,
        depth: usize,
        open: &mut Vec<OpenTop>,
    ) -> Result<(), Blocked> {
        let mut buf = [0; KEYWORD_MAX];
        let line = token.line;
        match keyword(token.name, &mut buf) {
            "INSERT" => self.write_after_top(Write::Insert, line, lex, depth, open)?,
            "INTO" => self.require_scratch(line, lex, "INTO")?,
            "UPDATE" => {
                if lex.word_is(0, "STATISTICS") {
                    return Err(self.blocked(line, "UPDATE STATISTICS"));
                }
                self.write_after_top(Write::Update, line, lex, depth, open)?;
            }
            "DELETE" => self.write_after_top(Write::Delete, line, lex, depth, open)?,
            "TRUNCATE" => self.require_table(line, lex, "TRUNCATE TABLE")?,
            "ALTER" if lex.word_is(0, "TABLE") => {
                self.require_scratch(line, &lex.after(1), "ALTER TABLE")?;
            }
            "ALTER" => return Err(self.blocked(line, &ddl_statement("ALTER", lex))),
            "CREATE" => {
                let mut j = lex.clone();
                while j
                    .word(0)
                    .is_some_and(|w| in_list(&["UNIQUE", "CLUSTERED", "NONCLUSTERED"], w))
                {
                    j = j.after(1);
                }
                if j.word_is(0, "TABLE") {
                    self.require_scratch(line, &j.after(1), "CREATE TABLE")?;
                } else if j.word_is(0, "INDEX") && j.word_is(2, "ON") {
                    // CREATE INDEX name ON #t
                    self.require_scratch(line, &j.after(3), "CREATE INDEX")?;
                } else if j.word_is(0, "OR") && j.word_is(1, "ALTER") {
                    let what = ddl_statement("CREATE OR ALTER", &j.after(2));
                    return Err(self.blocked(line, &what));
                } else {
                    return Err(self.blocked(line, &ddl_statement("CREATE", &j)));
                }
            }
            "DROP" => {
                if !lex.word_is(0, "TABLE") {
                    return Err(self.blocked(line, "DROP"));
                }
                let mut j = lex.after(1);
                if j.word_is(0, "IF") && j.word_is(1, "EXISTS") {
                    j = j.after(2);
                }
                loop {
                    self.require_scratch(line, &j, "DROP TABLE")?;
                    // Past the name just checked, then past a comma if one follows.
                    let mut next = j.clone();
                    next.next();
                    if next.next().is_none_or(|t| t.kind != Kind::Punct(',')) {
                        break;
                    }
                    j = next;
                }
            }
            "EXEC" | "EXECUTE" => self.check_exec(line, lex)?,
            "SYS" => self.check_physical_stats(token, lex)?,
            "SET" if self.reused_session => {
                if let Some(what) = set_of_unknown_state(lex) {
                    return Err(session_state(line, what));
                }
            }
            "OPEN" if self.reused_session => {
                if let Some(what) = open_of_key(lex) {
                    return Err(session_state(line, what));
                }
            }
            "BEGIN" => {
                let kinds = ["TRAN", "TRANSACTION", "DISTRIBUTED", "DIALOG"];
                let next = lex.word(0).unwrap_or("");
                if let Some(w) = kinds.iter().find(|kind| spells(next, kind)) {
                    return Err(self.blocked(line, &format!("BEGIN {w}")));
                }
            }
            "DBCC" => {
                let command = lex.word(0).unwrap_or("");
                if !in_list(DBCC_READ, command) {
                    return Err(self.unknown_dbcc(line, command, lex));
                }
            }
            // Sequence allocation survives a rollback, so the wrapper can't undo it.
            "NEXT" if lex.spells(0, "VALUE") && lex.word_is(1, "FOR") => {
                return Err(Blocked::write(
                    line,
                    format!(
                        "NEXT VALUE FOR {} advances a sequence, which a rollback can't undo",
                        self.name_at(&lex.after(2))
                    ),
                ));
            }
            word if BLOCKED.contains(&word) => {
                return Err(Blocked::write(
                    line,
                    format!(
                        "{word} is not allowed in read-only mode (if it is a column name, write it as [{}])",
                        token.text
                    ),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// `token` may be the `sys` of a call of `PHYSICAL_STATS`; `lex` is just past it. The call is
    /// found from its `sys`: the server answers a bare, `dbo.` or `master..` name with error 208,
    /// so `sys.` is the only way to the function, with a database in front or not.
    fn check_physical_stats(&self, token: &Token<'a>, lex: &Lexer<'a>) -> Result<(), Blocked> {
        if !token.spells_name("SYS") {
            return Ok(());
        }
        let mut call = lex.clone();
        let is_call = call.next().is_some_and(|t| t.kind == Kind::Punct('.'))
            && call.next().is_some_and(|t| t.spells_name(PHYSICAL_STATS))
            && call.next().is_some_and(|t| t.kind == Kind::Punct('('));
        if !is_call {
            return Ok(());
        }
        match read_stats_call(call) {
            StatsCall::Other => Ok(()),
            StatsCall::Wildcard(mode) => Err(wildcard_stats(token.line, mode)),
            StatsCall::TooNested => Err(too_complex(
                token.line,
                &format!(
                    "the arguments of sys.dm_db_index_physical_stats name it more than \
                     {MAX_STATS_NAMED} times"
                ),
            )),
        }
    }

    /// A write keyword (`what`, on `line`) whose target follows `lex`. After a `TOP n` or
    /// nothing, the target is checked at once. After `TOP (`, the clause is entered in `open`
    /// and the check is made at its closing parenthesis (see `check_writes`). `depth` is the
    /// parenthesis depth before the clause's own.
    fn write_after_top(
        &self,
        what: Write,
        line: u32,
        lex: &Lexer<'a>,
        depth: usize,
        open: &mut Vec<OpenTop>,
    ) -> Result<(), Blocked> {
        if !lex.word_is(0, "TOP") {
            return self.finish_write(what, line, lex);
        }
        if lex.kind(1) != Some(Kind::Punct('(')) {
            return self.finish_write(what, line, &lex.after(2));
        }
        if open.len() >= MAX_OPEN_TOP {
            return Err(too_complex(
                line,
                &format!("TOP ( clauses are nested more than {MAX_OPEN_TOP} deep"),
            ));
        }
        open.push(OpenTop {
            write: what,
            line,
            at: lex.pos,
            // The depth the main pass has once it has read the clause's own `(`.
            depth: depth + 1,
        });
        Ok(())
    }

    /// The write `what` on `line` has its target at `after`, past any `TOP` clause.
    fn finish_write(&self, what: Write, line: u32, after: &Lexer<'a>) -> Result<(), Blocked> {
        let (name, skipped) = match what {
            Write::Insert => ("INSERT", "INTO"),
            Write::Update => ("UPDATE", ""),
            Write::Delete => ("DELETE", "FROM"),
        };
        let target = if !skipped.is_empty() && after.word_is(0, skipped) {
            after.after(1)
        } else {
            after.clone()
        };
        self.require_scratch(line, &target, name)
    }

    /// The token at `target` must be a session temp table (`#t`) or a table variable (`@t`).
    /// Global temp tables (`##t`) are shared with other sessions, so writing to them blocks.
    /// `line` is the line of the keyword that makes the write.
    fn require_scratch(&self, line: u32, target: &Lexer<'a>, what: &str) -> Result<(), Blocked> {
        match target.ahead(0) {
            Some(t) if t.kind == Kind::Temp && t.text.starts_with("##") => Err(Blocked::write_or(
                line,
                format!(
                    "{what} {} would write to a global temp table, which other sessions share",
                    t.text
                ),
                "Use a #temp table".into(),
                "",
            )),
            Some(t) if matches!(t.kind, Kind::Temp | Kind::Var) => Ok(()),
            _ => Err(self.blocked(line, &format!("{what} {}", self.name_at(target)))),
        }
    }

    /// `KEYWORD TABLE #t` is allowed; anything else after KEYWORD blocks. `after` is the
    /// position just past the keyword, which is on `line`.
    fn require_table(&self, line: u32, after: &Lexer<'a>, what: &str) -> Result<(), Blocked> {
        if after.word_is(0, "TABLE") {
            self.require_scratch(line, &after.after(1), what)
        } else {
            Err(self.blocked(line, what))
        }
    }

    /// `after` is the position just past the EXEC keyword, which is on `line`; a refusal is
    /// reported there, whatever follows the keyword and wherever it is.
    fn check_exec(&self, line: u32, after: &Lexer<'a>) -> Result<(), Blocked> {
        let mut name = after.clone();
        match after.kind(0) {
            Some(Kind::Punct('(')) => return Err(self.blocked(line, "EXEC (dynamic SQL)")),
            Some(Kind::Var) if after.kind(1) == Some(Kind::Punct('=')) => name = after.after(2),
            Some(Kind::Var) => {
                return Err(self.blocked(line, "EXEC of a procedure named by a variable"));
            }
            _ => {}
        }
        if name.word_is(0, "AS") {
            return Err(self.blocked(line, "EXECUTE AS"));
        }
        self.check_proc(line, &name, "EXEC", false)
    }

    /// Checks the (possibly multi-part) procedure name starting at `start` against the
    /// allowlist. `line` is where a refusal is reported: the EXEC keyword's, or the first
    /// word of a batch. `batch_start` is true for the implicit call of a batch that begins
    /// with a word that is no statement keyword.
    fn check_proc(
        &self,
        line: u32,
        start: &Lexer<'a>,
        what: &str,
        batch_start: bool,
    ) -> Result<(), Blocked> {
        let mut parts = Dotted::new(start);
        let mut last = None;
        for part in parts.by_ref() {
            last = Some(part);
        }
        let end = parts.rest();
        let last = last.filter(|t| matches!(t.kind, Kind::Word | Kind::Quoted));
        let name = last.map_or("", |t| t.name);
        if !in_list(PROCS, name) {
            let shown = self.name_at(start);
            if name.eq_ignore_ascii_case(FLUSH_PROC) {
                let reason = if batch_start {
                    shown
                } else {
                    format!("{what} {shown}")
                };
                // sqlh does know this one, so its reason doesn't say it can't tell.
                return Err(Blocked {
                    line,
                    reason: format!(
                        "{reason} isn't on sqlh's list of read-only procedures: it writes Query \
                         Store's in-memory data to disk"
                    ),
                    advice: FLUSH_INSTEAD.into(),
                    lifted_by_write: true,
                    instead: Some(FLUSH_INSTEAD.into()),
                });
            }
            if batch_start {
                // A batch that starts with an unknown word is run as a procedure call by SQL
                // Server, so a typo like `SELEC 1` lands here too.
                return Err(Blocked::write_or(
                    line,
                    format!(
                        "{shown} starts the batch, so SQL Server would run it as a stored \
                         procedure (a typo?); it isn't on sqlh's list of read-only procedures"
                    ),
                    "Fix it if it's a typo".into(),
                    self.as_written(),
                ));
            }
            return Err(Blocked::unknown(
                line,
                format!(
                    "{what} {shown} isn't on sqlh's list of read-only procedures, so sqlh can't \
                     tell what it does"
                ),
                self.as_written(),
            ));
        }
        if !self.local_system_qualifier(start) {
            // The policy, not a claim about what the name resolves to: it may well be the
            // system procedure, but another schema, database or linked server can name a
            // different one.
            return Err(Blocked::write_or(
                line,
                format!(
                    "{what} {}: only a bare, sys. or master. name is taken as the system \
                     procedure (another schema, database or linked server can name a different one)",
                    self.name_at(start)
                ),
                format!("Call it as sys.{}", last.map_or("", |t| t.text)),
                self.as_written(),
            ));
        }
        if let Some(arg) = ARG_CHECKS
            .iter()
            .find(|arg| arg.name.eq_ignore_ascii_case(name))
        {
            // `name;number` calls a numbered procedure (EXECUTE's `module_name [;number]`):
            // the arguments come after the number. No statement starts with a number, so the
            // `;` is no statement's end.
            let args = if end.kind(0) == Some(Kind::Punct(';')) && end.kind(1) == Some(Kind::Num) {
                end.after(2)
            } else {
                end
            };
            match self.passes_argument(&args, arg.param) {
                Some(false) => {}
                Some(true) => {
                    return Err(Blocked::write_or(
                        line,
                        arg.reason.into(),
                        arg.instead.into(),
                        "",
                    ));
                }
                None => {
                    return Err(too_complex(
                        line,
                        &format!(
                            "the arguments of {} have more than {MAX_ARG_PARENS} parentheses",
                            arg.name.to_ascii_lowercase()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Only these qualifications reach the real system procedure: none, `sys.`, `master.sys.`,
    /// `master.dbo.` and `master..`. Others can name a user procedure that shares the name, or
    /// a procedure on a linked server.
    fn local_system_qualifier(&self, start: &Lexer<'a>) -> bool {
        // The parts of the name, the last of which is the procedure itself. Only the first
        // three can matter, so the rest are counted and not kept.
        let mut head = [""; 3];
        let mut count = 0usize;
        let mut after_dot = true;
        let mut push = |part: &'a str| {
            if let Some(slot) = head.get_mut(count) {
                *slot = part;
            }
            count += 1;
        };
        for t in Dotted::new(start) {
            if t.kind == Kind::Punct('.') {
                if after_dot {
                    push("");
                }
                after_dot = true;
            } else {
                push(t.name);
                after_dot = false;
            }
        }
        // Without the procedure name: [], ["SYS"] or ["MASTER", "SYS" | "DBO" | ""].
        match count.saturating_sub(1) {
            0 => true,
            1 => head[0].eq_ignore_ascii_case("SYS"),
            2 => head[0].eq_ignore_ascii_case("MASTER") && in_list(&["SYS", "DBO", ""], head[1]),
            _ => false,
        }
    }

    /// Whether a call's arguments pass `param`, named or as the second positional parameter,
    /// or `None` if they have more than `MAX_ARG_PARENS` parentheses. `args_start` is the
    /// position just past the procedure's name, one of `ARG_CHECKS`.
    ///
    /// The scan ends at a `;` or a statement keyword outside parentheses, so the next call's
    /// own scan starts where this one stops. A scan can only read on to a later call (an
    /// `EXEC` is a statement keyword) with a parenthesis open, so each scan that reaches the
    /// `k`-th call after it has read a parenthesis from each of those calls' arguments, the
    /// ones in between included. With at most `MAX_ARG_PARENS` of them in a scan, no token is
    /// read by more than that many scans, and the work stays linear.
    fn passes_argument(&self, args_start: &Lexer<'a>, param: &str) -> Option<bool> {
        let mut positional = 0;
        let mut depth = 0usize;
        let mut parens = 0usize;
        let mut arg_start = true;
        // Just past a named argument's `=`, where its value comes.
        let mut after_equals = false;
        let mut scan = args_start.clone();
        while let Some(t) = scan.next() {
            if matches!(t.kind, Kind::Punct('(' | ')')) {
                parens += 1;
                if parens > MAX_ARG_PARENS {
                    return None;
                }
            }
            // Where a value goes, an unreserved word is the value (`sp_configure GET, 1`):
            // it ends no statement.
            let value = (arg_start || after_equals) && in_list(UNRESERVED_STARTS, t.name);
            after_equals = depth == 0 && t.kind == Kind::Punct('=');
            match t.kind {
                Kind::Punct(';') if depth == 0 => break,
                Kind::Word if depth == 0 && !value && in_list(STATEMENT_START, t.name) => break,
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => depth = depth.saturating_sub(1),
                Kind::Punct(',') if depth == 0 => {
                    arg_start = true;
                    continue;
                }
                _ => {}
            }
            if arg_start {
                arg_start = false;
                let named = t.kind == Kind::Var && scan.kind(0) == Some(Kind::Punct('='));
                // The server finds a parameter by its collation, under which an ignored or
                // folded character (`@updateusage` with a joiner in it, a full-width letter)
                // still names this one. Neither procedure has a parameter that needs one, so any
                // other is the server's error 8145 and refusing it costs nothing.
                if named && (t.name.eq_ignore_ascii_case(param) || !t.name.is_ascii()) {
                    return Some(true);
                }
                if !named {
                    positional += 1;
                    if positional >= 2 {
                        // The answer can't change now; the rest is not read.
                        return Some(true);
                    }
                }
            }
        }
        Some(false)
    }

    /// The dotted name starting at `at`, as written, for messages.
    fn name_at(&self, at: &Lexer<'a>) -> String {
        let name: String = Dotted::new(at).map(|t| t.text).collect();
        if name.is_empty() {
            "?".to_owned()
        } else {
            name
        }
    }

    /// A refusal for `what`, at the keyword on `line`.
    fn blocked(&self, line: u32, what: &str) -> Blocked {
        Blocked::write(line, format!("{what} would write"))
    }

    /// The refusal of a DBCC command not in `DBCC_READ`. `command` is the word at `after`, the
    /// position just past `DBCC`: empty when that is no bare word.
    fn unknown_dbcc(&self, line: u32, command: &str, after: &Lexer<'a>) -> Blocked {
        let shown = if command.is_empty() {
            self.name_at(after)
        } else {
            command.to_ascii_uppercase()
        };
        let reason = format!("DBCC {shown} isn't on sqlh's list of read-only DBCC commands");
        match DBCC_INSTEAD
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(command))
        {
            Some((_, read)) => Blocked::write_or(line, reason, format!("Read {read} instead"), ""),
            None => Blocked::unknown(line, reason, self.as_written()),
        }
    }
}

/// The tokens of a dotted name (`db..obj` allowed): name parts and dots.
struct Dotted<'a> {
    lex: Lexer<'a>,
    expect_part: bool,
}

impl<'a> Dotted<'a> {
    fn new(at: &Lexer<'a>) -> Self {
        Dotted {
            lex: at.clone(),
            expect_part: true,
        }
    }

    /// The position after the name, as far as it has been read.
    fn rest(&self) -> Lexer<'a> {
        self.lex.clone()
    }
}

impl<'a> Iterator for Dotted<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let mut scan = self.lex.clone();
        let t = scan.next()?;
        match t.kind {
            Kind::Punct('.') => self.expect_part = true,
            Kind::Word | Kind::Quoted | Kind::Temp | Kind::Var if self.expect_part => {
                self.expect_part = false
            }
            _ => return None,
        }
        self.lex = scan;
        Some(t)
    }
}

/// The tokens of a batch, read one at a time. A position in the input is the `Lexer` itself
/// (a reference and two numbers), so the classifier looks ahead by cloning it and reading on
/// from the copy. Nothing is stored per token.
#[derive(Clone)]
struct Lexer<'a> {
    sql: &'a str,
    /// Byte offset where the next token, or the whitespace and comments before it, starts.
    pos: usize,
    /// 1-based line at `pos`.
    line: u32,
    /// What every read is charged to, if anything: once it is spent, the lexer and all its
    /// copies report the end of the input.
    work: Option<&'a Work>,
}

impl<'a> Lexer<'a> {
    /// A lexer that is not charged for its reads: for a single pass over the text.
    fn new(sql: &'a str) -> Self {
        Lexer {
            sql,
            pos: 0,
            line: 1,
            work: None,
        }
    }

    /// A lexer whose reads, and its copies', are charged to `work`.
    fn metered(sql: &'a str, work: &'a Work) -> Self {
        Lexer {
            work: Some(work),
            ..Lexer::new(sql)
        }
    }

    /// The token `n` places on (0 is the next one); the position doesn't move.
    fn ahead(&self, n: usize) -> Option<Token<'a>> {
        self.clone().nth(n)
    }

    /// The position `n` tokens on, or the end of the input if it has fewer.
    fn after(&self, n: usize) -> Lexer<'a> {
        let mut at = self.clone();
        for _ in 0..n {
            if at.next().is_none() {
                break;
            }
        }
        at
    }

    fn kind(&self, n: usize) -> Option<Kind> {
        self.ahead(n).map(|t| t.kind)
    }

    /// The name of the bare word `n` places on, as written.
    fn word(&self, n: usize) -> Option<&'a str> {
        self.ahead(n)
            .filter(|t| t.kind == Kind::Word)
            .map(|t| t.name)
    }

    fn word_is(&self, n: usize, keyword: &str) -> bool {
        self.ahead(n).is_some_and(|t| t.is_word(keyword))
    }

    /// `Token::spells_word`, of the token `n` places on.
    fn spells(&self, n: usize, keyword: &str) -> bool {
        self.ahead(n).is_some_and(|t| t.spells_word(keyword))
    }
}

/// An ASCII byte that continues a name. Beyond ASCII, `ends_a_word` decides.
fn word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$'
}

/// What the servers read as whitespace beyond ASCII, where they also read every control character
/// and the NUL as whitespace: a keyword next to one of these is that keyword. From parsing every
/// code point with `sys.dm_exec_describe_first_result_set` (SQL Server 2016 SP3 and 2025 gave the
/// same set, DESIGN.md §7). U+200B and U+0085 are here, U+200C, U+200D, U+180E and U+FEFF aren't.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{85}' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200B}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// What the servers take as a currency sign beyond ASCII, from the same parse. Each is a `money`
/// literal by itself or with a number after it, whatever comes next, unlike `$`, which starts a
/// pseudocolumn's name unless a number follows (`$action`, `$DELETE`: error 126).
fn is_currency(c: char) -> bool {
    matches!(
        c,
        '\u{A2}'..='\u{A5}'
            | '\u{9F2}'
            | '\u{9F3}'
            | '\u{E3F}'
            | '\u{17DB}'
            | '\u{20A0}'..='\u{20B1}'
            | '\u{FDFC}'
            | '\u{FE69}'
            | '\u{FF04}'
            | '\u{FFE0}'
            | '\u{FFE1}'
            | '\u{FFE5}'
            | '\u{FFE6}'
    )
}

/// Where the name that goes on from `i` ends. Every other character the servers don't take in a
/// name is a syntax error there, so calling it part of the word hides nothing that would run.
fn word_end(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    while let Some(&b) = bytes.get(i) {
        if b < 0x80 {
            if !word_char(b) {
                break;
            }
            i += 1;
        } else {
            match sql.get(i..).and_then(|rest| rest.chars().next()) {
                Some(c) if !is_space(c) && !is_currency(c) => i += c.len_utf8(),
                _ => break,
            }
        }
    }
    i
}

fn digits_end(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    i
}

/// Where the digits and decimal point of a number from `i` end.
fn decimal_end(bytes: &[u8], i: usize) -> usize {
    let i = digits_end(bytes, i);
    if bytes.get(i) == Some(&b'.') {
        digits_end(bytes, i + 1)
    } else {
        i
    }
}

/// Where a `money` literal ends, from just past its sign. The servers take spaces (U+0020 only)
/// after the sign, then a `+` or `-`, then a number with no exponent: `$ -1.5EXEC` is a literal
/// and a keyword, and `$1eDELETE` is `$1` with an alias. With no number, the sign is the whole
/// literal.
fn money_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while bytes.get(i) == Some(&b' ') {
        i += 1;
    }
    if matches!(bytes.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let end = decimal_end(bytes, i);
    if end == i { from } else { end }
}

/// Where the number from `i` ends. The servers end one at the first byte that can't continue it,
/// so what comes next is a token of its own however close it is: `1DELETE` and `0xINSERT` are a
/// number and a keyword, and `1e` and `1e+5` are whole numbers.
fn number_end(bytes: &[u8], i: usize) -> usize {
    if matches!(bytes.get(i..i + 2), Some(b"0x" | b"0X")) {
        let mut i = i + 2;
        while bytes.get(i).is_some_and(u8::is_ascii_hexdigit) {
            i += 1;
        }
        return i;
    }
    let mut i = decimal_end(bytes, i);
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        let digits = if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i + 1
        } else {
            i
        };
        if bytes.get(digits).is_some_and(u8::is_ascii_digit) {
            i = digits_end(bytes, digits);
        }
    }
    i
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let Some(work) = self.work else {
            return self.read();
        };
        if work.exceeded() {
            return None;
        }
        let from = self.pos;
        let token = self.read();
        // One unit a read, and one for each 32 bytes it went over (a read that ends the input
        // counts too).
        work.charge(1 + ((self.pos - from) >> 5) as u64, self.line);
        token
    }
}

impl<'a> Lexer<'a> {
    /// The next token, uncharged.
    fn read(&mut self) -> Option<Token<'a>> {
        let sql = self.sql;
        let bytes = sql.as_bytes();
        let mut line = self.line;
        let mut i = self.pos;
        while i < bytes.len() {
            let c = bytes[i];
            let next = bytes.get(i + 1).copied();
            let start = i;
            let start_line = line;
            // Whether a string or quoted name found its closing delimiter.
            let mut closed = true;
            let kind = match c {
                b'\n' => {
                    line += 1;
                    i += 1;
                    continue;
                }
                // Control characters too, VT and NUL among them (`is_space`).
                c if c <= b' ' => {
                    i += 1;
                    continue;
                }
                // A lone CR ends the comment on the servers as well, though it starts no new
                // line in their numbering, so the count above stays with LF.
                b'-' if next == Some(b'-') => {
                    while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                        i += 1;
                    }
                    continue;
                }
                b'/' if next == Some(b'*') => {
                    let mut depth = 1;
                    i += 2;
                    while i < bytes.len() && depth > 0 {
                        match (bytes[i], bytes.get(i + 1).copied()) {
                            (b'*', Some(b'/')) => {
                                depth -= 1;
                                i += 2;
                            }
                            (b'/', Some(b'*')) => {
                                depth += 1;
                                i += 2;
                            }
                            (b'\n', _) => {
                                line += 1;
                                i += 1;
                            }
                            _ => i += 1,
                        }
                    }
                    continue;
                }
                b'\'' => {
                    (i, closed) = skip_quoted(bytes, i + 1, b'\'', &mut line);
                    Kind::Str
                }
                b'N' | b'n' if next == Some(b'\'') => {
                    (i, closed) = skip_quoted(bytes, i + 2, b'\'', &mut line);
                    Kind::Str
                }
                b'[' => {
                    (i, closed) = skip_quoted(bytes, i + 1, b']', &mut line);
                    Kind::Quoted
                }
                b'"' => {
                    (i, closed) = skip_quoted(bytes, i + 1, b'"', &mut line);
                    Kind::Quoted
                }
                b'@' | b'#' => {
                    i = word_end(sql, i + 1);
                    if c == b'@' { Kind::Var } else { Kind::Temp }
                }
                c if c.is_ascii_digit() => {
                    i = number_end(bytes, i);
                    Kind::Num
                }
                // `$action`, `$DELETE` and the like go on as a name (error 126 if it is none). `\`
                // is a currency sign to the servers, the yen's in some code pages.
                b'\\' => {
                    i = money_end(bytes, i + 1);
                    Kind::Num
                }
                b'$' if !next.is_some_and(|n| word_char(n) && !n.is_ascii_digit()) => {
                    i = money_end(bytes, i + 1);
                    Kind::Num
                }
                c if word_char(c) => {
                    i = word_end(sql, i + 1);
                    Kind::Word
                }
                c if c >= 0x80 => {
                    let Some(ch) = sql.get(i..).and_then(|rest| rest.chars().next()) else {
                        break;
                    };
                    i += ch.len_utf8();
                    if is_space(ch) {
                        continue;
                    }
                    if is_currency(ch) {
                        i = money_end(bytes, i);
                        Kind::Num
                    } else {
                        i = word_end(sql, i);
                        Kind::Word
                    }
                }
                _ => {
                    // Every byte >= 0x80 went to the arm above, so `c` is ASCII here.
                    i += 1;
                    Kind::Punct(c as char)
                }
            };
            let text = &sql[start..i];
            let name = match kind {
                Kind::Word | Kind::Var | Kind::Temp => text,
                // Both delimiters are single ASCII bytes, so these slices fall on char
                // boundaries.
                Kind::Quoted if closed => &text[1..text.len() - 1],
                Kind::Quoted => &text[1..],
                _ => "",
            };
            self.pos = i;
            self.line = line;
            return Some(Token {
                kind,
                text,
                name,
                line: start_line,
            });
        }
        self.pos = i;
        self.line = line;
        None
    }
}

/// Returns the index just past the closing quote (doubled quotes are escapes), and whether
/// the closing quote was found before the end of the input.
fn skip_quoted(bytes: &[u8], mut i: usize, close: u8, line: &mut u32) -> (usize, bool) {
    while i < bytes.len() {
        match bytes[i] {
            b if b == close => {
                if bytes.get(i + 1) == Some(&close) {
                    i += 2;
                    continue;
                }
                return (i + 1, true);
            }
            b'\n' => *line += 1,
            _ => {}
        }
        i += 1;
    }
    (bytes.len(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_ONLY: Policy = Policy {
        writes: false,
        compile_only: false,
        reused_session: false,
    };

    fn allowed(sql: &str) {
        assert_eq!(check(sql, READ_ONLY), Ok(()), "should be allowed: {sql}");
    }

    fn blocked(sql: &str) -> Blocked {
        check(sql, READ_ONLY).expect_err(&format!("should be blocked: {sql}"))
    }

    #[test]
    fn reads_are_allowed() {
        allowed("SELECT TOP 5 * FROM dbo.Orders WHERE Status = 'DELETE' ORDER BY 1");
        allowed("WITH c AS (SELECT 1 AS x) SELECT x FROM c");
        allowed("DECLARE @n int = 1; SET @n += 1; PRINT @n; IF @n > 1 BEGIN SELECT @n END");
        allowed(
            "SELECT user_updates, delete_referential_action, [Update] FROM sys.dm_db_index_usage_stats",
        );
        allowed("-- UPDATE dbo.T\n/* DELETE /* nested */ FROM x */ SELECT 1");
        allowed("SET NOCOUNT ON; SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; SELECT 1");
        allowed("USE master; SELECT name FROM sys.databases");
        allowed("BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH SELECT ERROR_MESSAGE() END CATCH");
        allowed("SELECT * FROM t FOR JSON PATH");
        allowed("DBCC SHOW_STATISTICS ('dbo.T', IX_T)");
    }

    #[test]
    fn scratch_writes_are_allowed() {
        allowed(
            "CREATE TABLE #t (id int); INSERT INTO #t VALUES (1); UPDATE #t SET id = 2; DELETE FROM #t; DROP TABLE #t",
        );
        allowed(
            "DECLARE @t TABLE (id int); INSERT @t SELECT 1; UPDATE TOP (1) @t SET id = 3; DELETE @t",
        );
        allowed(
            "SELECT name INTO #names FROM sys.objects; TRUNCATE TABLE #names; DROP TABLE IF EXISTS #a, #b",
        );
        allowed("CREATE NONCLUSTERED INDEX ix ON #t (id); ALTER TABLE #t ADD x int");
        allowed("INSERT INTO #who EXEC sp_who2");
        allowed(
            "DECLARE c CURSOR FOR SELECT 1; OPEN c; FETCH NEXT FROM c INTO @x; CLOSE c; DEALLOCATE c",
        );
    }

    #[test]
    fn procedures() {
        allowed("EXEC sp_help 'dbo.Orders'");
        allowed("EXECUTE master.dbo.sp_who2");
        allowed("EXEC @rc = sys.sp_helptext N'dbo.v'");
        allowed("sp_who2");
        allowed("[sys].[sp_columns] 'Orders'");
        allowed("label: SELECT 1");
        assert_eq!(
            blocked("EXEC dbo.PurgeOrders").reason,
            "EXEC dbo.PurgeOrders isn't on sqlh's list of read-only procedures, so sqlh can't \
             tell what it does"
        );
        assert!(blocked("dbo.PurgeOrders 5").reason.starts_with(
            "dbo.PurgeOrders starts the batch, so SQL Server would run it as a stored procedure"
        ));
        assert!(blocked("SELEC 1").reason.contains("(a typo?)"));
        blocked("EXEC ('DELETE FROM t')");
        blocked("EXEC @proc");
        blocked("EXEC sp_executesql N'SELECT 1'");
        blocked("EXECUTE AS USER = 'dbo'");
        blocked("EXEC sp_spaceused @updateusage = 'true'");
        blocked("EXEC xp_cmdshell 'dir'");
    }

    #[test]
    fn writes_are_blocked() {
        let b = blocked("SELECT 1\nUPDATE dbo.Orders SET x = 1");
        assert_eq!(
            b,
            Blocked {
                line: 2,
                reason: "UPDATE dbo.Orders would write".into(),
                advice: WRITE_ADVICE.into(),
                lifted_by_write: true,
                instead: None,
            }
        );
        assert_eq!(
            blocked("insert into [dbo].[T] values (1)").reason,
            "INSERT [dbo].[T] would write"
        );
        assert_eq!(
            blocked("SELECT * INTO dbo.Copy FROM t").reason,
            "INTO dbo.Copy would write"
        );
        blocked("WITH c AS (SELECT * FROM t) DELETE FROM c");
        blocked("DELETE t FROM dbo.T t JOIN #x ON 1 = 1");
        blocked("MERGE #t AS t USING s ON 1 = 0 WHEN NOT MATCHED THEN INSERT VALUES (1);");
        blocked("TRUNCATE TABLE dbo.T");
        blocked("CREATE PROCEDURE p AS SELECT 1");
        blocked("CREATE INDEX ix ON dbo.T (a)");
        blocked("ALTER DATABASE x SET OFFLINE");
        blocked("DROP TABLE #a, dbo.b");
        blocked("DROP VIEW v");
        blocked("UPDATE STATISTICS dbo.T");
        blocked("DBCC FREEPROCCACHE");
        blocked("SELECT * FROM OPENQUERY(srv, 'SELECT 1')");
        blocked("WAITFOR DELAY '00:00:10'");
        blocked("GRANT SELECT ON t TO u");
    }

    #[test]
    fn transaction_control_is_blocked() {
        blocked("BEGIN TRAN; SELECT 1");
        blocked("BEGIN TRANSACTION x");
        blocked("COMMIT");
        blocked("ROLLBACK TRANSACTION");
        blocked("SAVE TRAN s");
    }

    #[test]
    fn strings_and_quoted_names_hide_keywords() {
        allowed("SELECT 'UPDATE dbo.T SET x = 1', N'EXEC x', [DROP], \"DELETE\" FROM t");
        allowed("SELECT 'it''s; DELETE FROM t' AS s");
        let b = blocked("SELECT 'a\nb'\nDELETE FROM t");
        assert_eq!(b.line, 3);
    }

    #[test]
    fn blanket_word_mentions_brackets() {
        let b = blocked("SELECT Send FROM t");
        assert!(b.reason.contains("[Send]"), "{}", b.reason);
    }

    // F03: side effects a rollback can't undo, and procedures that aren't what they seem.
    #[test]
    fn sequences_are_blocked() {
        let b = blocked("SELECT NEXT VALUE FOR dbo.OrderSeq");
        assert!(
            b.reason.contains("dbo.OrderSeq") && b.reason.contains("rollback can't undo"),
            "{}",
            b.reason
        );
        blocked("DECLARE @n bigint = NEXT VALUE FOR s;");
        allowed("FETCH NEXT FROM c INTO @x");
    }

    #[test]
    fn global_temp_tables_are_blocked() {
        for sql in [
            "INSERT INTO ##shared VALUES (1)",
            "UPDATE ##shared SET x = 1",
            "DELETE FROM ##shared",
            "SELECT 1 AS x INTO ##shared",
            "CREATE TABLE ##shared (x int)",
            "DROP TABLE #mine, ##shared",
            "TRUNCATE TABLE ##shared",
        ] {
            assert!(blocked(sql).reason.contains("global temp table"), "{sql}");
        }
        allowed("SELECT * FROM ##shared");
    }

    #[test]
    fn only_local_system_procedures() {
        allowed("EXEC sp_who");
        allowed("EXEC sys.sp_who");
        allowed("EXEC master.sys.sp_who");
        allowed("EXEC master.dbo.sp_who");
        allowed("EXEC master..sp_who");
        allowed("EXEC [master].[sys].[sp_who]");
        for sql in [
            "EXEC Lnk.master.sys.sp_who",
            "EXEC sales.sp_who",
            "EXEC dbo.sp_who",
            "EXEC otherdb.sys.sp_who",
            "EXEC otherdb..sp_who",
            "Lnk.master.sys.sp_who",
        ] {
            let b = blocked(sql);
            assert!(
                b.reason.contains("only a bare, sys. or master. name"),
                "{sql}: {}",
                b.reason
            );
            assert_eq!(
                b.advice, "Call it as sys.sp_who, or rerun with --allow-write to run it as written",
                "{sql}"
            );
        }
    }

    // R04: each refusal says how to get past it; --allow-write only where it helps.
    #[test]
    fn refusals_carry_their_own_advice() {
        assert_eq!(
            blocked("INSERT ##shared VALUES (1)").advice,
            "Use a #temp table, or rerun with --allow-write"
        );
        assert!(
            blocked("EXEC sp_spaceused N'dbo.T', N'true'")
                .advice
                .starts_with("Leave @updateusage out")
        );
        assert!(
            blocked("SELEC 1")
                .advice
                .starts_with("Fix it if it's a typo")
        );
        assert_eq!(blocked("DELETE FROM dbo.T").advice, WRITE_ADVICE);
        assert_eq!(blocked("SELECT NEXT VALUE FOR dbo.S").advice, WRITE_ADVICE);
    }

    // plan-history.md 2.16 (agent trial 2): under a profile whose write_policy is deny,
    // --allow-write is a usage error, so no refusal may offer it. The alternative short of writing
    // stays, and the advice says the profile doesn't allow writes. Without a deny profile it is as
    // before.
    #[test]
    fn refusals_under_a_deny_profile_never_offer_allow_write() {
        let denied = "profile p doesn't allow writes (write_policy = \"deny\")";
        for (sql, expected) in [
            (
                "DELETE FROM dbo.T",
                "Profile p doesn't allow writes (write_policy = \"deny\")".to_owned(),
            ),
            (
                "SELECT NEXT VALUE FOR dbo.S",
                "Profile p doesn't allow writes (write_policy = \"deny\")".to_owned(),
            ),
            (
                "INSERT ##shared VALUES (1)",
                format!("Use a #temp table; {denied}"),
            ),
            (
                "EXEC sp_spaceused N'dbo.T', N'true'",
                format!("Leave @updateusage out to only read sizes; {denied}"),
            ),
            ("SELEC 1", format!("Fix it if it's a typo; {denied}")),
            (
                "EXEC dbo.sp_who",
                format!("Call it as sys.sp_who; {denied}"),
            ),
        ] {
            let b = blocked(sql);
            assert_eq!(
                b.advice_for(Some(&WritesOff::Profile("p".into()))),
                expected,
                "{sql}"
            );
            assert!(
                !b.advice_for(Some(&WritesOff::Profile("p".into())))
                    .contains("--allow-write"),
                "{sql}"
            );
            assert_eq!(b.advice_for(None), b.advice, "{sql}");
        }
        // Too complex to classify: simplify, nothing about writes.
        let b = too_complex(1, "test");
        assert_eq!(
            b.advice_for(Some(&WritesOff::Profile("p".into()))),
            format!("Simplify it or split it into smaller batches with GO; {denied}")
        );
        // A refusal --allow-write doesn't lift keeps its own advice.
        let check_mode = Policy {
            writes: false,
            compile_only: true,
            reused_session: false,
        };
        let b = check("SET NOEXEC OFF", check_mode).expect_err("refused");
        assert_eq!(
            b.advice_for(Some(&WritesOff::Profile("p".into()))),
            b.advice
        );
        // sqlh mcp is read-only: the same shape, its own reason (owner decision 16).
        let mcp = Some(&WritesOff::Mcp);
        assert_eq!(
            blocked("DELETE FROM dbo.T").advice_for(mcp),
            "sqlh mcp is read-only"
        );
        assert_eq!(
            blocked("INSERT ##shared VALUES (1)").advice_for(mcp),
            "Use a #temp table; sqlh mcp is read-only"
        );
        // The check tool's refusal names no flag.
        assert_eq!(
            b.advice_for(mcp),
            "Remove SET NOEXEC (the check tool sets it already); write a column named NOEXEC as \
             [NOEXEC]"
        );
    }

    /// The refusal of `sql` as an agent reads it: the reason, then the advice in a run where
    /// `--allow-write` works, under a deny profile, and in `sqlh mcp`.
    fn worded(sql: &str) -> (String, String, String, String) {
        let b = blocked(sql);
        (
            b.reason.clone(),
            b.advice_for(None),
            b.advice_for(Some(&WritesOff::Profile("p".into()))),
            b.advice_for(Some(&WritesOff::Mcp)),
        )
    }

    // Decision 23, command-categories.md §6.9: what sqlh doesn't know isn't called a write.
    #[test]
    fn unknown_procedures_are_not_called_writes() {
        const DENIED: &str = "Profile p doesn't allow writes (write_policy = \"deny\")";
        const AS_WRITTEN: &str = "Rerun with --allow-write to run it as written";
        const UNKNOWN: &str =
            "isn't on sqlh's list of read-only procedures, so sqlh can't tell what it does";
        for (sql, shown) in [
            ("EXEC sp_WhoIsActive", "EXEC sp_WhoIsActive"),
            ("execute dbo.sp_BlitzCache", "EXEC dbo.sp_BlitzCache"),
            (
                "EXEC master.dbo.sp_WhoIsActive",
                "EXEC master.dbo.sp_WhoIsActive",
            ),
            ("EXEC [dbo].[usp_x] 1", "EXEC [dbo].[usp_x]"),
            ("EXEC @rc = dbo.usp_x", "EXEC dbo.usp_x"),
            ("INSERT #t EXEC dbo.usp_x", "EXEC dbo.usp_x"),
            ("EXEC sp_executesql N'SELECT 1'", "EXEC sp_executesql"),
            ("EXEC xp_cmdshell 'dir'", "EXEC xp_cmdshell"),
            ("EXEC", "EXEC ?"),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!("{shown} {UNKNOWN}"),
                    AS_WRITTEN.to_owned(),
                    DENIED.to_owned(),
                    "sqlh mcp is read-only".to_owned(),
                ),
                "{sql}"
            );
        }
        const IMPLICIT: &str = "starts the batch, so SQL Server would run it as a stored \
                                procedure (a typo?); it isn't on sqlh's list of read-only \
                                procedures";
        for (sql, shown) in [
            ("sp_WhoIsActive", "sp_WhoIsActive"),
            ("sp_WhoIsActive @find_block_leaders = 1", "sp_WhoIsActive"),
            ("dbo.Purge 5", "dbo.Purge"),
            ("[dbo].[Purge]", "[dbo].[Purge]"),
            ("SELEC 1", "SELEC"),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!("{shown} {IMPLICIT}"),
                    "Fix it if it's a typo, or rerun with --allow-write to run it as written".to_owned(),
                    "Fix it if it's a typo; profile p doesn't allow writes (write_policy = \"deny\")"
                        .to_owned(),
                    "Fix it if it's a typo; sqlh mcp is read-only".to_owned(),
                ),
                "{sql}"
            );
        }
        // What sqlh does say about a call stays what it was.
        for (sql, reason) in [
            ("EXEC ('DELETE FROM t')", "EXEC (dynamic SQL) would write"),
            (
                "EXEC @p",
                "EXEC of a procedure named by a variable would write",
            ),
            ("EXECUTE AS USER = 'x'", "EXECUTE AS would write"),
        ] {
            assert_eq!(blocked(sql).reason, reason, "{sql}");
        }
    }

    // §3.2, F3: a flush isn't needed to read recent data, so the refusal says what to read.
    #[test]
    fn the_query_store_flush_gets_a_read_in_place_of_a_flag() {
        const READ: &str = "Read sys.query_store_runtime_stats instead: it has the rows not yet \
                            flushed too; aggregate by plan_id, execution_type and \
                            runtime_stats_interval_id";
        for (sql, shown) in [
            (
                "EXEC sp_query_store_flush_db",
                "EXEC sp_query_store_flush_db",
            ),
            (
                "exec SYS.SP_QUERY_STORE_FLUSH_DB",
                "EXEC SYS.SP_QUERY_STORE_FLUSH_DB",
            ),
            (
                "EXEC master.sys.sp_query_store_flush_db",
                "EXEC master.sys.sp_query_store_flush_db",
            ),
            ("sp_query_store_flush_db", "sp_query_store_flush_db"),
            (
                "[sys].[sp_query_store_flush_db]",
                "[sys].[sp_query_store_flush_db]",
            ),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!(
                        "{shown} isn't on sqlh's list of read-only procedures: it writes Query \
                         Store's in-memory data to disk"
                    ),
                    READ.to_owned(),
                    format!("{READ}; profile p doesn't allow writes (write_policy = \"deny\")"),
                    format!("{READ}; sqlh mcp is read-only"),
                ),
                "{sql}"
            );
        }
        // Its neighbours, which change what is stored, are plain unknown procedures.
        assert_eq!(
            blocked("EXEC sys.sp_query_store_force_plan 1, 2").advice,
            "Rerun with --allow-write to run it as written"
        );
        // --allow-write skips the classifier, for this as for the rest.
        let writes = Policy {
            writes: true,
            compile_only: false,
            reused_session: false,
        };
        assert_eq!(check("EXEC sys.sp_query_store_flush_db", writes), Ok(()));
    }

    // §6.9's replacements, in each form the advice takes.
    #[test]
    fn unknown_dbcc_commands_name_their_replacement_first() {
        const UNKNOWN: &str = "isn't on sqlh's list of read-only DBCC commands";
        for (sql, command, read) in [
            (
                "DBCC DBINFO WITH TABLERESULTS",
                "DBINFO",
                "DATABASEPROPERTYEX(name, 'LastGoodCheckDbTime')",
            ),
            (
                "DBCC IND (N'Sales', N'dbo.Orders', -1)",
                "IND",
                "sys.dm_db_database_page_allocations (undocumented)",
            ),
            ("DBCC LOGINFO", "LOGINFO", "sys.dm_db_log_info"),
            (
                "dbcc page (N'Sales', 1, 100, 3)",
                "PAGE",
                "sys.dm_db_page_info (SQL Server 2019 and later)",
            ),
            ("DBCC PROCCACHE", "PROCCACHE", "sys.dm_exec_cached_plans"),
            (
                "DBCC ShowContig",
                "SHOWCONTIG",
                "sys.dm_db_index_physical_stats",
            ),
            (
                "DBCC SHOWFILESTATS",
                "SHOWFILESTATS",
                "sys.dm_db_file_space_usage",
            ),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!("DBCC {command} {UNKNOWN}"),
                    format!("Read {read} instead, or rerun with --allow-write"),
                    format!(
                        "Read {read} instead; profile p doesn't allow writes (write_policy = \
                         \"deny\")"
                    ),
                    format!("Read {read} instead; sqlh mcp is read-only"),
                ),
                "{sql}"
            );
        }
        for (sql, command) in [
            ("DBCC FREEPROCCACHE", "FREEPROCCACHE"),
            ("DBCC WRITEPAGE (N'Sales', 1, 100, 0, 1, 0x00)", "WRITEPAGE"),
            ("DBCC SQLPERF(LOGSPACE)", "SQLPERF"),
            ("DBCC CHECKDB WITH ESTIMATEONLY", "CHECKDB"),
            ("DBCC MEMORYSTATUS", "MEMORYSTATUS"),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!("DBCC {command} {UNKNOWN}"),
                    "Rerun with --allow-write to run it as written".to_owned(),
                    "Profile p doesn't allow writes (write_policy = \"deny\")".to_owned(),
                    "sqlh mcp is read-only".to_owned(),
                ),
                "{sql}"
            );
        }
        for (command, _) in DBCC_INSTEAD {
            assert_eq!(*command, command.to_ascii_uppercase());
            assert!(
                !in_list(DBCC_READ, command),
                "{command} is on the read list"
            );
        }
    }

    // The probes command-categories.md §6.9 names (Appendix A), and how far a name is read.
    #[test]
    fn ddl_refusals_name_the_statement() {
        for (probe, sql, statement) in [
            (
                "C6",
                "ALTER RESOURCE GOVERNOR RESET STATISTICS",
                "ALTER RESOURCE GOVERNOR",
            ),
            (
                "Q15",
                "ALTER DATABASE CURRENT SET QUERY_STORE CLEAR",
                "ALTER DATABASE",
            ),
            (
                "Q16",
                "ALTER DATABASE CURRENT SET QUERY_STORE = ON",
                "ALTER DATABASE",
            ),
            (
                "Q22",
                "ALTER DATABASE SCOPED CONFIGURATION CLEAR PROCEDURE_CACHE 0x0600",
                "ALTER DATABASE SCOPED CONFIGURATION",
            ),
            (
                "Q27",
                "ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 4",
                "ALTER DATABASE SCOPED CONFIGURATION",
            ),
            (
                "E8",
                "CREATE STATISTICS s ON #work (c)",
                "CREATE STATISTICS",
            ),
            ("E9", "ALTER INDEX ALL ON #work REBUILD", "ALTER INDEX"),
            (
                "E25",
                "ALTER INDEX IX_Orders_CustomerId ON dbo.Orders REBUILD WITH (ONLINE = ON \
                 (WAIT_AT_LOW_PRIORITY (MAX_DURATION = 1 MINUTES, ABORT_AFTER_WAIT = BLOCKERS)))",
                "ALTER INDEX",
            ),
            (
                "F10",
                "ALTER AVAILABILITY GROUP ag FAILOVER",
                "ALTER AVAILABILITY GROUP",
            ),
            (
                "F22",
                "CREATE EVENT SESSION s ON SERVER ADD EVENT sqlserver.sql_batch_completed",
                "CREATE EVENT SESSION",
            ),
            (
                "F23",
                "ALTER EVENT SESSION [system_health] ON SERVER STATE = STOP",
                "ALTER EVENT SESSION",
            ),
            (
                "I9",
                "ALTER EVENT SESSION system_health ON SERVER DROP EVENT \
                 sqlserver.xml_deadlock_report",
                "ALTER EVENT SESSION",
            ),
            (
                "J12",
                "ALTER INDEX IX_Orders_CustomerId ON dbo.Orders REORGANIZE",
                "ALTER INDEX",
            ),
            (
                "J34",
                "ALTER DATABASE Sales SET SINGLE_USER WITH ROLLBACK IMMEDIATE",
                "ALTER DATABASE",
            ),
        ] {
            assert_eq!(
                worded(sql),
                (
                    format!("{statement} would write"),
                    WRITE_ADVICE.to_owned(),
                    "Profile p doesn't allow writes (write_policy = \"deny\")".to_owned(),
                    "sqlh mcp is read-only".to_owned(),
                ),
                "{probe}: {sql}"
            );
        }
        for (sql, reason) in [
            // In any case and however it is spaced, in sqlh's own spelling.
            (
                "alter event session s on server",
                "ALTER EVENT SESSION would write",
            ),
            (
                "ALTER /* a */ DATABASE -- b\n SCOPED\nCONFIGURATION SET MAXDOP = 4",
                "ALTER DATABASE SCOPED CONFIGURATION would write",
            ),
            // The longest statement that fits, and a database that only sounds like one.
            (
                "ALTER DATABASE SCOPED CREDENTIAL c WITH IDENTITY = 'x'",
                "ALTER DATABASE SCOPED CREDENTIAL would write",
            ),
            (
                "ALTER DATABASE SCOPED SET READ_ONLY",
                "ALTER DATABASE would write",
            ),
            (
                "ALTER DATABASE audit SET READ_ONLY",
                "ALTER DATABASE would write",
            ),
            (
                "ALTER SERVER AUDIT SPECIFICATION s WITH (STATE = ON)",
                "ALTER SERVER AUDIT SPECIFICATION would write",
            ),
            (
                "ALTER SERVER AUDIT s WITH (STATE = ON)",
                "ALTER SERVER AUDIT would write",
            ),
            (
                "ALTER SERVICE MASTER KEY REGENERATE",
                "ALTER SERVICE MASTER KEY would write",
            ),
            ("ALTER SERVICE s", "ALTER SERVICE would write"),
            ("ALTER LOGIN x DISABLE", "ALTER LOGIN would write"),
            ("ALTER PROC p AS SELECT 1", "ALTER PROC would write"),
            ("ALTER VIEW v AS SELECT 1", "ALTER VIEW would write"),
            (
                "CREATE PROCEDURE p AS SELECT 1",
                "CREATE PROCEDURE would write",
            ),
            (
                "CREATE LOGIN x WITH PASSWORD = 'y'",
                "CREATE LOGIN would write",
            ),
            (
                "CREATE XML SCHEMA COLLECTION c AS ''",
                "CREATE XML SCHEMA COLLECTION would write",
            ),
            (
                "CREATE OR ALTER VIEW v AS SELECT 1",
                "CREATE OR ALTER VIEW would write",
            ),
            ("CREATE OR ALTER", "CREATE OR ALTER would write"),
            // Past what CREATE INDEX may start with.
            (
                "CREATE CLUSTERED COLUMNSTORE INDEX ix ON #t",
                "CREATE COLUMNSTORE INDEX would write",
            ),
            (
                "CREATE PRIMARY XML INDEX ix ON #t (c)",
                "CREATE PRIMARY XML INDEX would write",
            ),
            (
                "CREATE UNIQUE STATISTICS s ON t (c)",
                "CREATE STATISTICS would write",
            ),
            ("CREATE INDEX a.b ON #t (c)", "CREATE INDEX would write"),
            ("CREATE UNIQUE CLUSTERED", "CREATE would write"),
            // What the input says past the keyword isn't taken for a statement: only sqlh's
            // own names are shown, so a truncated or unknown one stays bare, and is refused.
            ("ALTER", "ALTER would write"),
            ("CREATE", "CREATE would write"),
            ("ALTER EVENT", "ALTER would write"),
            ("ALTER EVENT dbo.s", "ALTER would write"),
            ("ALTER [EVENT] SESSION s", "ALTER would write"),
            ("ALTER 'EVENT SESSION'", "ALTER would write"),
            ("ALTER EVENT 'x' SESSION", "ALTER would write"),
            ("ALTER NOSUCH x", "ALTER would write"),
            ("CREATE NOSUCH x", "CREATE would write"),
            // Named as before.
            (
                "ALTER TABLE dbo.T ADD x int",
                "ALTER TABLE dbo.T would write",
            ),
            ("ALTER TABLE", "ALTER TABLE ? would write"),
            (
                "CREATE TABLE dbo.T (x int)",
                "CREATE TABLE dbo.T would write",
            ),
            (
                "CREATE INDEX ix ON dbo.T (a)",
                "CREATE INDEX dbo.T would write",
            ),
            ("UPDATE STATISTICS dbo.T", "UPDATE STATISTICS would write"),
        ] {
            assert_eq!(blocked(sql).reason, reason, "{sql}");
        }
        assert_eq!(
            blocked("ALTER TABLE ##g ADD x int").reason,
            "ALTER TABLE ##g would write to a global temp table, which other sessions share"
        );
        // Only the words of a refusal change: what passed before still does.
        allowed("ALTER TABLE #t ADD x int");
        allowed("ALTER TABLE @t ADD x int");
        allowed("CREATE TABLE #t (x int)");
        allowed("CREATE UNIQUE CLUSTERED INDEX ix ON #t (x)");
        assert_eq!(blocked("SELECT 1\nALTER\nINDEX ix ON #t REBUILD").line, 2);

        for (i, statement) in DDL_STATEMENTS.iter().enumerate() {
            assert_eq!(*statement, statement.to_ascii_uppercase());
            assert!(statement.split(' ').count() <= DDL_WORDS, "{statement}");
            // The first match is the one named, so no entry may hide a longer one after it.
            for later in &DDL_STATEMENTS[i + 1..] {
                assert!(
                    !later.starts_with(&format!("{statement} ")),
                    "{statement} hides {later}"
                );
            }
        }
    }

    // A name is lookahead under the budget, read once on the way to a refusal.
    #[test]
    fn naming_a_statement_reads_a_few_tokens_once() {
        let filler = "SELECT 1\n".repeat(100_000);
        for sql in [
            format!("ALTER EVENT SESSION s\n{filler}"),
            format!("CREATE OR ALTER VIEW v\n{filler}"),
            format!("ALTER NOSUCH x\n{filler}"),
            format!("DBCC PAGE\n{filler}"),
        ] {
            assert!(work_of(&sql) < 30, "{}: {}", &sql[..20], work_of(&sql));
        }
        // Inside TOP clauses that must close first, and past the budget, the answer is as before.
        let n = 10_000;
        let nested = format!(
            "{}ALTER EVENT SESSION s{}",
            "UPDATE TOP (".repeat(n),
            ") @t ".repeat(n)
        );
        assert_eq!(blocked(&nested).reason, "ALTER EVENT SESSION would write");
        assert!(work_of(&nested) < 12 * n as u64, "{}", work_of(&nested));
        let many = "ALTER NOSUCH x\n".repeat(100_000);
        assert!(work_of(&many) < 30, "{}", work_of(&many));
        let spent = check_within("ALTER EVENT SESSION s", READ_ONLY, 0, 0).expect_err("refused");
        assert!(spent.reason.contains("too complex"), "{}", spent.reason);
    }

    // Decision 30(d), command-categories.md §6.10.
    const STATS_DETAILED: &str = "sys.dm_db_index_physical_stats(NULL, ..., 'DETAILED') would \
                                  read every page of every index in every database";
    const STATS_SAMPLED: &str = "sys.dm_db_index_physical_stats(NULL, ..., 'SAMPLED') would read \
                                 at least 1% of the pages of every index in every database";
    const STATS_REWRITE: &str = "Name the database and object, or use 'LIMITED'";

    #[test]
    fn the_wildcard_stats_call_is_refused_in_every_spelling_the_server_takes() {
        let cases: &[(&str, u32, &str)] = &[
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'SAMPLED')",
                1,
                STATS_SAMPLED,
            ),
            // The first argument's two literals; the other arguments don't matter to the rule.
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(DEFAULT, DEFAULT, DEFAULT, DEFAULT, \
                 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(null, 5, 1, 1, 'SAMPLED')",
                1,
                STATS_SAMPLED,
            ),
            // Any case, of the name, the keyword and the mode.
            (
                "select * from SYS.DM_DB_INDEX_PHYSICAL_STATS(Null, null, NULL, nULL, 'detailed')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM Sys.Dm_Db_Index_Physical_Stats(default, 1, 1, 1, 'SaMpLeD')",
                1,
                STATS_SAMPLED,
            ),
            // Quoted and qualified, as the server takes them, trailing spaces in a name too.
            (
                "SELECT * FROM [sys].[dm_db_index_physical_stats](NULL, NULL, NULL, NULL, \
                 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM \"sys\".\"DM_DB_INDEX_PHYSICAL_STATS\"(NULL, NULL, NULL, NULL, \
                 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.[dm_db_index_physical_stats](NULL, NULL, NULL, NULL, 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM [sys ].[dm_db_index_physical_stats  ](NULL, NULL, NULL, NULL, \
                 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM master.sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, \
                 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM [Sales DB].[sys].[dm_db_index_physical_stats](NULL, NULL, NULL, \
                 NULL, 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            // How the mode is written: N'', n'', trailing spaces, a line continued by a backslash,
            // "" under QUOTED_IDENTIFIER OFF.
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAI\\\nLED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, N'SAM\\\r\nPLED')",
                1,
                STATS_SAMPLED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, N'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, n'sampled')",
                1,
                STATS_SAMPLED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED  ')",
                1,
                STATS_DETAILED,
            ),
            (
                "SET QUOTED_IDENTIFIER OFF; SELECT * FROM sys.dm_db_index_physical_stats(NULL, \
                 NULL, NULL, NULL, \"DETAILED\")",
                1,
                STATS_DETAILED,
            ),
            // Comments and newlines between any two tokens; the line is the name's.
            (
                "SELECT *\nFROM /* a */ sys -- b\n . /* c */ dm_db_index_physical_stats /* d */\n\
                 ( -- e\n NULL /* f */ , NULL,\n NULL, NULL\n, /* 'LIMITED' */ 'DETAILED' -- g\n)",
                2,
                STATS_DETAILED,
            ),
            // Parentheses, around a literal and inside the arguments before the fifth.
            (
                "SELECT * FROM sys.dm_db_index_physical_stats((NULL), (NULL), ((NULL)), NULL, \
                 ('DETAILED'))",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(((NULL)), NULL, NULL, NULL, \
                 (('SAMPLED')))",
                1,
                STATS_SAMPLED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, OBJECT_ID((N'dbo.T')), \
                 ISNULL((SELECT MAX(x) FROM (VALUES (1), (2)) AS v (x)), (1)), COALESCE(NULL, 1, \
                 2), 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            // Wherever a table can be: a subquery, an APPLY, a later statement, a CTE, a branch.
            (
                "SELECT (SELECT COUNT(*) FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, \
                 NULL, 'DETAILED')) AS n",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM (SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, \
                 NULL, 'SAMPLED')) AS s",
                1,
                STATS_SAMPLED,
            ),
            (
                "SELECT * FROM sys.databases AS d\nCROSS APPLY \
                 sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED') AS s",
                2,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.objects AS o\nOUTER APPLY \
                 sys.dm_db_index_physical_stats(DEFAULT, o.object_id, NULL, NULL, 'DETAILED')",
                2,
                STATS_DETAILED,
            ),
            (
                "SELECT 1;\nSELECT 2\nSELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, \
                 NULL, NULL, 'DETAILED')",
                3,
                STATS_DETAILED,
            ),
            (
                "WITH c AS (SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, \
                 'DETAILED')) SELECT * FROM c",
                1,
                STATS_DETAILED,
            ),
            (
                "IF 1 = 1 SELECT * INTO #t FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, \
                 NULL, 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            // The second of two calls, and one inside another's arguments, either way round.
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'LIMITED')\n\
                 SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
                2,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(NULL, (SELECT MAX(object_id) FROM \
                 sys.dm_db_index_physical_stats(DB_ID(), NULL, NULL, NULL, 'LIMITED')), NULL, \
                 NULL, 'DETAILED')",
                1,
                STATS_DETAILED,
            ),
            (
                "SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(), (SELECT MAX(object_id) \
                 FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'SAMPLED')), NULL, \
                 NULL, 'LIMITED')",
                1,
                STATS_SAMPLED,
            ),
        ];
        for (sql, line, reason) in cases {
            let b = blocked(sql);
            assert_eq!((b.line, b.reason.as_str()), (*line, *reason), "{sql}");
            assert_eq!(b.advice, STATS_REWRITE, "{sql}");
        }
    }

    #[test]
    fn the_wildcard_stats_refusal_offers_no_flag_anywhere() {
        let b = blocked("SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED')");
        for off in [
            None,
            Some(WritesOff::Mcp),
            Some(WritesOff::Profile("d".into())),
        ] {
            assert_eq!(b.advice_for(off.as_ref()), STATS_REWRITE, "{off:?}");
        }
    }

    // Decision 30(e), command-categories.md §6.3.
    const MCP: Policy = Policy {
        writes: false,
        compile_only: false,
        reused_session: true,
    };
    const MCP_CHECK: Policy = Policy {
        writes: false,
        compile_only: true,
        reused_session: true,
    };
    const STATE_ADVICE: &str = "Run it with the sqlh CLI, whose session ends with the run";

    fn state_reason(what: &str) -> String {
        format!("{what} leaves session state sqlh mcp's reset isn't known to clear")
    }

    #[test]
    fn a_reused_session_refuses_the_state_its_reset_is_not_known_to_clear() {
        const AUTOPILOT: &str = "SET AUTOPILOT";
        const IDENTITY: &str = "SET IDENTITY_INSERT";
        const SYMMETRIC: &str = "OPEN SYMMETRIC KEY";
        const MASTER: &str = "OPEN MASTER KEY";
        let cases: &[(&str, u32, &str)] = &[
            ("SET AUTOPILOT ON", 1, AUTOPILOT),
            ("SET AUTOPILOT OFF", 1, AUTOPILOT),
            ("set autopilot on", 1, AUTOPILOT),
            ("SeT aUtOpIlOt On;", 1, AUTOPILOT),
            (";SET AUTOPILOT ON", 1, AUTOPILOT),
            ("SET/**/AUTOPILOT--x\n ON", 1, AUTOPILOT),
            ("SET\n\n  AUTOPILOT\nON", 1, AUTOPILOT),
            ("/* SET NOCOUNT ON */ SET AUTOPILOT ON", 1, AUTOPILOT),
            // The servers take this one list; the rest of these are their errors to give, and
            // refusing them loses nothing.
            ("SET AUTOPILOT, AUTOPILOT ON", 1, AUTOPILOT),
            ("SET AUTOPILOT, NOCOUNT ON", 1, AUTOPILOT),
            ("SET AUTOPILOT", 1, AUTOPILOT),
            ("BEGIN SET AUTOPILOT ON END", 1, AUTOPILOT),
            ("IF 1 = 0 SET AUTOPILOT ON", 1, AUTOPILOT),
            ("SELECT 1;\nSET AUTOPILOT ON", 2, AUTOPILOT),
            ("SELECT 1\nSET NOCOUNT ON\n\nSET AUTOPILOT ON", 4, AUTOPILOT),
            ("UPDATE #t SET a = 1\nSET AUTOPILOT ON", 2, AUTOPILOT),
            ("SET IDENTITY_INSERT #t ON", 1, IDENTITY),
            ("SET IDENTITY_INSERT #t OFF", 1, IDENTITY),
            ("set identity_insert #t on", 1, IDENTITY),
            ("SET IDENTITY_INSERT dbo.T ON", 1, IDENTITY),
            ("SET IDENTITY_INSERT [#t] ON", 1, IDENTITY),
            ("SET IDENTITY_INSERT \"#t\" ON", 1, IDENTITY),
            ("SET IDENTITY_INSERT ..#t ON", 1, IDENTITY),
            ("SET IDENTITY_INSERT tempdb..#t ON", 1, IDENTITY),
            ("SET /*x*/ IDENTITY_INSERT -- y\n #t ON", 1, IDENTITY),
            (
                "CREATE TABLE #t (id int IDENTITY, c int);\nSET IDENTITY_INSERT #t ON;",
                2,
                IDENTITY,
            ),
            (
                "OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE c",
                1,
                SYMMETRIC,
            ),
            (
                "open symmetric key k decryption by certificate c",
                1,
                SYMMETRIC,
            ),
            (
                "OPEN /*a*/ SYMMETRIC --b\n KEY k DECRYPTION BY CERTIFICATE c",
                1,
                SYMMETRIC,
            ),
            (
                "OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE c;\nSELECT DECRYPTBYKEY(s) FROM t",
                1,
                SYMMETRIC,
            ),
            (
                "SELECT 1\nOPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE c",
                2,
                SYMMETRIC,
            ),
            ("OPEN MASTER KEY DECRYPTION BY PASSWORD = 'x'", 1, MASTER),
            ("oPeN mAsTeR kEy DECRYPTION BY PASSWORD = 'x'", 1, MASTER),
            (
                "SELECT 1;\n\nOPEN MASTER KEY DECRYPTION BY PASSWORD = 'x'",
                3,
                MASTER,
            ),
        ];
        for (sql, line, what) in cases {
            for policy in [MCP, MCP_CHECK] {
                let b = check(sql, policy).expect_err(&format!("should be refused: {sql}"));
                assert_eq!(
                    (b.line, b.reason.as_str()),
                    (*line, state_reason(what).as_str()),
                    "{sql:?}"
                );
                assert_eq!(b.advice_for(Some(&WritesOff::Mcp)), STATE_ADVICE, "{sql:?}");
            }
            // The CLI keeps them, whatever else applies to the run.
            allowed(sql);
            for writes in [false, true] {
                for compile_only in [false, true] {
                    let cli = Policy {
                        writes,
                        compile_only,
                        reused_session: false,
                    };
                    assert_eq!(check(sql, cli), Ok(()), "{sql:?}, {cli:?}");
                }
            }
        }
    }

    // Each of these leaves nothing the reset isn't known to clear, or is no statement at all, so
    // refusing it would be a false refusal of a legal read. `autopilot` isn't a reserved word.
    #[test]
    fn a_reused_session_allows_what_only_looks_like_that_state() {
        for sql in [
            // A column or variable, not the option: the servers ran `=`, `+=` and `.modify` on #t.
            "UPDATE #t SET autopilot = 1",
            "UPDATE #t SET autopilot=1, a = 2",
            "UPDATE #t SET a = 2, autopilot = 1",
            "UPDATE #t SET autopilot += 3",
            "UPDATE #t SET autopilot.modify('delete /a')",
            "UPDATE @t SET autopilot.x = 1",
            "DECLARE @autopilot int; SET @autopilot = 1",
            "SELECT autopilot, identity_insert = 1 FROM dbo.T",
            "SELECT [SET] autopilot FROM dbo.T",
            "SELECT [OPEN] master FROM dbo.T",
            // Not the statement: quoted, a string, a comment.
            "SELECT 'SET AUTOPILOT ON', N'OPEN MASTER KEY', 'SET IDENTITY_INSERT #t ON'",
            "SELECT 1 -- SET AUTOPILOT ON\n/* OPEN SYMMETRIC KEY k */",
            // What the reset is shown to clear, or the setup sets again.
            "SET NOCOUNT ON",
            "SET ROWCOUNT 2",
            "SET STATISTICS IO, TIME ON",
            "SET SHOWPLAN_XML ON",
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            "SET DEADLOCK_PRIORITY LOW",
            // A cursor, which the reset is shown to close, whatever it is called.
            "OPEN c",
            "OPEN GLOBAL c",
            "OPEN [master]",
            "OPEN [symmetric]",
            "OPEN [key]",
            "OPEN GLOBAL [key]",
            // Closing a key leaves none open, and decision 30(e) names only the opening.
            "CLOSE SYMMETRIC KEY k",
            "CLOSE ALL SYMMETRIC KEYS",
            "CLOSE MASTER KEY",
            "CLOSE c",
            // The servers' errors (102, 156, 1067 on both): nothing they could leave.
            "SET [AUTOPILOT] ON",
            "SET \"AUTOPILOT\" ON",
            "SET [IDENTITY_INSERT] #t ON",
            "SET NOCOUNT, AUTOPILOT ON",
            "OPEN [SYMMETRIC] KEY k DECRYPTION BY CERTIFICATE c",
            "OPEN SYMMETRIC [KEY] k DECRYPTION BY CERTIFICATE c",
            "OPEN [MASTER] KEY DECRYPTION BY PASSWORD = 'x'",
        ] {
            assert_eq!(check(sql, MCP), Ok(()), "{sql:?}");
        }
    }

    // The new check adds refusals and takes none away, and the earliest keyword's stays first.
    #[test]
    fn the_session_state_refusal_takes_its_place_in_the_text() {
        const AUTOPILOT: &str = "SET AUTOPILOT ON";
        let reason = state_reason("SET AUTOPILOT");
        let first = |sql: &str| check(sql, MCP).expect_err("refused");
        let b = first(&format!("DELETE dbo.T\n{AUTOPILOT}"));
        assert_eq!((b.line, b.reason.as_str()), (1, "DELETE dbo.T would write"));
        let b = first(&format!("{AUTOPILOT}\nDELETE dbo.T"));
        assert_eq!((b.line, b.reason.as_str()), (1, reason.as_str()));
        let b = first(&format!("DELETE TOP (SELECT 1) FROM #t\n{AUTOPILOT}"));
        assert_eq!((b.line, b.reason.as_str()), (2, reason.as_str()));
        let b = first(&format!("WAITFOR DELAY '00:00:01'\n{AUTOPILOT}"));
        assert!(
            b.reason.starts_with("WAITFOR is not allowed"),
            "{}",
            b.reason
        );
        for (sql, reason) in [
            (
                "ALTER MASTER KEY REGENERATE",
                "ALTER MASTER KEY would write",
            ),
            ("DROP MASTER KEY", "DROP would write"),
            (
                "CREATE SYMMETRIC KEY k WITH ALGORITHM = AES_256",
                "CREATE SYMMETRIC KEY would write",
            ),
            ("UPDATE dbo.T SET autopilot = 1", "UPDATE dbo.T would write"),
        ] {
            assert_eq!(first(sql).reason, reason, "{sql:?}");
            assert_eq!(blocked(sql).reason, reason, "{sql:?}");
        }
    }

    // A few tokens read per `SET` or `OPEN`, whatever follows and however many there are, and
    // the budget stands behind them as it does for the other lookups.
    #[test]
    fn looking_for_session_state_takes_linear_work() {
        let work_in_mcp = |sql: &str| {
            let work = Work::new(sql, WORK_BASE, WORK_PER_BYTE);
            let result = Guard {
                sql,
                work: &work,
                compile_only: false,
                reused_session: true,
            }
            .check_writes();
            assert_eq!(result, Ok(()), "{}", &sql[..20]);
            work.used.get()
        };
        let n = 100_000;
        for (statement, most) in [
            ("SET ", 4),
            ("SET AUTOPILOT = ", 8),
            ("OPEN ", 4),
            ("OPEN MASTER ", 7),
            ("OPEN c\n", 5),
            ("UPDATE #t SET autopilot = 1\n", 16),
        ] {
            let sql = statement.repeat(n);
            let used = work_in_mcp(&sql);
            assert!(used < most * n as u64, "{statement:?}: {used}");
        }
        let spent = check_within("SET AUTOPILOT ON", MCP, 0, 0).expect_err("refused");
        assert!(spent.reason.contains("too complex"), "{}", spent.reason);
    }

    // Each of these reads, or is the server's to refuse; none is decision 30(d)'s call.
    #[test]
    fn stats_calls_that_are_not_the_wildcard_scan_are_allowed() {
        for sql in [
            // A mode that doesn't scan: the rewrite the refusal offers.
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'LIMITED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, N'limited')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, NULL)",
            "SELECT * FROM sys.dm_db_index_physical_stats(DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT)",
            // A database named: the other rewrite.
            "SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(), NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(N'Sales'), OBJECT_ID(N'dbo.T'), \
             NULL, NULL, 'SAMPLED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(7, NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM sys.databases AS d CROSS APPLY \
             sys.dm_db_index_physical_stats(d.database_id, NULL, NULL, NULL, 'DETAILED')",
            // A variable can't be seen (E3): DESIGN.md §7 says so.
            "DECLARE @m sysname = N'DETAILED';\nSELECT * FROM \
             sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, @m)",
            "DECLARE @d int = NULL;\nSELECT * FROM sys.dm_db_index_physical_stats(@d, NULL, NULL, \
             NULL, 'DETAILED')",
            // Nor can an expression, or a literal other than the two decision 30(d) names.
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DET' + 'AILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL + 1, NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(CAST(NULL AS int), 1, 1, 1, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(N'typo'), 1, 1, 1, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(0, NULL, NULL, NULL, 'DETAILED')",
            // A column called NULL, or a mode that is no mode.
            "SELECT * FROM t CROSS APPLY sys.dm_db_index_physical_stats([NULL], 1, 1, 1, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, [DETAILED])",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, ' DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILEDX')",
            // Too few or too many arguments, or none, or a call never closed: the server's error.
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL)",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL)",
            "SELECT * FROM sys.dm_db_index_physical_stats()",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED', 1)",
            "SELECT * FROM sys.dm_db_index_physical_stats",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED'",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED",
            // Text, not a call.
            "SELECT 1 -- sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
            "SELECT 1 /* sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED') */",
            "SELECT 'sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, ''DETAILED'')'",
            "EXEC sp_describe_first_result_set N'SELECT * FROM \
             sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, ''DETAILED'')'",
            // Other names. The server has no function by the first three (error 208), so one by
            // that name is someone's own.
            "SELECT * FROM dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM dbo.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM master..dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')",
            "SELECT * FROM sys.dm_db_index_operational_stats(NULL, NULL, NULL, NULL)",
            "SELECT * FROM sys.other(NULL, NULL, NULL, NULL, 'DETAILED')",
            "SELECT sys, [sys], s.sys, [sys].x FROM t AS sys",
        ] {
            allowed(sql);
        }
        // The classifier's other rules still hold past a call it lets by.
        assert_eq!(
            blocked(
                "SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(), 1, 1, 1, 'LIMITED')\n\
                 DELETE dbo.T"
            )
            .reason,
            "DELETE dbo.T would write"
        );
    }

    // It lives in the write classifier: --check runs that, and a run with writes allowed doesn't
    // (DESIGN.md §7).
    #[test]
    fn the_wildcard_stats_call_is_a_read_only_runs_refusal() {
        let sql = "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED')";
        let check_only = Policy {
            writes: false,
            compile_only: true,
            reused_session: false,
        };
        assert_eq!(check(sql, check_only).unwrap_err().reason, STATS_DETAILED);
        for writes in [
            Policy {
                writes: true,
                compile_only: false,
                reused_session: false,
            },
            Policy {
                writes: true,
                compile_only: true,
                reused_session: false,
            },
        ] {
            assert_eq!(check(sql, writes), Ok(()), "{writes:?}");
        }
    }

    // The earliest keyword's refusal, here as everywhere, with a `TOP (` clause open too.
    #[test]
    fn the_wildcard_stats_refusal_takes_its_place_in_the_text() {
        const CALL: &str = "sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'DETAILED')";
        let b = blocked(&format!("DELETE dbo.T\nSELECT * FROM {CALL}"));
        assert_eq!((b.line, b.reason.as_str()), (1, "DELETE dbo.T would write"));
        let b = blocked(&format!("SELECT * FROM {CALL}\nDELETE dbo.T"));
        assert_eq!((b.line, b.reason.as_str()), (1, STATS_DETAILED));
        // A write whose target comes after the call starts earlier in the text, so it comes first.
        let b = blocked(&format!("DELETE TOP (SELECT 1 FROM {CALL}) FROM dbo.T"));
        assert_eq!((b.line, b.reason.as_str()), (1, "DELETE dbo.T would write"));
        // A write that passes leaves the call to be refused when the clause closes.
        let b = blocked(&format!("DELETE TOP (SELECT 1 FROM {CALL}) FROM #t"));
        assert_eq!((b.line, b.reason.as_str()), (1, STATS_DETAILED));
    }

    // The bounds under decision 30(d)'s lookahead: `read_stats_call` says why these hold.
    #[test]
    fn reading_stats_calls_takes_linear_work() {
        // The real shape, about 8 MB of it: one bounded scan a call, on top of the pass.
        for call in [
            "DB_ID(), NULL, NULL, NULL, 'DETAILED'",
            "NULL, NULL, NULL, NULL, 'LIMITED'",
        ] {
            let n = 100_000;
            let sql = format!("SELECT * FROM sys.dm_db_index_physical_stats({call});\n").repeat(n);
            allowed(&sql);
            assert!(work_of(&sql) < 60 * n as u64, "{call}: {}", work_of(&sql));
        }
        // A refusal after all of that is found, on its own line.
        let sql = format!(
            "{}SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'SAMPLED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, 1, 1, 1, 'LIMITED')\n"
                .repeat(100_000)
        );
        let b = blocked(&sql);
        assert_eq!((b.line, b.reason.as_str()), (100_001, STATS_SAMPLED));

        // Unclosed calls, each with the next in its arguments: the quadratic shape, cut off by
        // the first call's scan, however long the input is.
        let open = "sys.dm_db_index_physical_stats(NULL, ";
        let too_deep = format!(
            "this input is too complex for the read-only check: the arguments of \
             sys.dm_db_index_physical_stats name it more than {MAX_STATS_NAMED} times"
        );
        for n in [MAX_STATS_NAMED + 2, 1_000, 100_000, 250_000] {
            let sql = format!("SELECT * FROM {}", open.repeat(n));
            let b = blocked(&sql);
            assert_eq!((b.line, b.reason.as_str()), (1, too_deep.as_str()), "{n}");
            assert!(b.advice.contains("--allow-write"), "{}", b.advice);
            assert!(work_of(&sql) < 100, "{n}: {}", work_of(&sql));
        }
        // At the limit they pass, and a long tail costs each scan one read a token at most.
        let nested = format!("SELECT * FROM {}", open.repeat(MAX_STATS_NAMED + 1));
        allowed(&nested);
        let scans = MAX_STATS_NAMED as u64 + 1;
        for n in [10_000, 50_000, 100_000] {
            let sql = format!("{nested}{}", "1 + ".repeat(n));
            allowed(&sql);
            let tokens = 2 * n as u64;
            let main_pass = tokens;
            assert!(
                work_of(&sql) <= scans * tokens + main_pass + 1_000,
                "{n}: {}",
                work_of(&sql)
            );
        }
        // The budget stands behind all this.
        let sql = format!("{nested}{}", "1 + ".repeat(100_000));
        let b = check_within(&sql, READ_ONLY, 1_000, 0).expect_err("over the budget");
        assert!(b.reason.contains("token reads"), "{}", b.reason);

        // A literal's parentheses are read by one scan, since the next call's `sys` stops it, in
        // the first argument and in the fifth.
        let n = 100_000;
        let sql = "sys.dm_db_index_physical_stats((((((((".repeat(n);
        let sql = format!("SELECT * FROM {sql}");
        allowed(&sql);
        assert!(work_of(&sql) < 30 * n as u64, "{}", work_of(&sql));
        let sql = "sys.dm_db_index_physical_stats(NULL, 1, 1, 1, (((((((( ".repeat(n);
        let sql = format!("SELECT * FROM {sql}");
        allowed(&sql);
        assert!(work_of(&sql) < 60 * n as u64, "{}", work_of(&sql));
        // A mention of `sys` alone costs a look at the next token or three, no more.
        let sql = format!("SELECT {}", "sys.sys.".repeat(n));
        allowed(&sql);
        assert!(work_of(&sql) < 12 * n as u64, "{}", work_of(&sql));
    }

    #[test]
    fn sp_spaceused_update_usage_is_blocked() {
        allowed("EXEC sp_spaceused");
        allowed("EXEC sp_spaceused N'dbo.T'");
        allowed("EXEC sp_spaceused @objname = N'dbo.T'");
        allowed("EXEC sp_spaceused N'dbo.T', @oneresultset = 1");
        allowed("EXEC sp_spaceused N'dbo.T'; SELECT 1, 2");
        allowed("EXEC sp_spaceused N'dbo.T'\nSELECT 1, 2");
        for sql in [
            "EXEC sys.sp_spaceused N'dbo.T', N'true'",
            "EXEC sp_spaceused @objname = N'dbo.T', @updateusage = 'true'",
            "EXEC sp_spaceused @updateusage = N'true'",
            "EXEC sp_spaceused NULL, 'true'",
            // A procedure number, and a bare value spelled like an unreserved statement
            // keyword, are read past (found in review, 2026-10-23).
            "EXEC sp_spaceused;1 N'dbo.T', 'true'",
            "EXEC sp_spaceused ; 1 @updateusage = 'true'",
            "EXEC sp_spaceused Move, 'true'",
            "EXEC sp_spaceused @objname = Move, @updateusage = 'true'",
        ] {
            assert!(blocked(sql).reason.contains("UPDATEUSAGE"), "{sql}");
        }
        allowed("EXEC sp_spaceused;1 N'dbo.T'");
    }

    // PLAN.md Phase 6, the interim step: the error log, registry reads, drives, version and
    // the configuration listing, each named exactly; the writing neighbours stay blocked.
    #[test]
    fn diagnostic_extended_procedures_are_allowed_by_exact_name() {
        for sql in [
            "EXEC xp_readerrorlog",
            "EXEC xp_readerrorlog 0, 1, N'Login failed', NULL, NULL, NULL, N'desc'",
            "EXEC sys.xp_readerrorlog 1, 2",
            "EXEC master.dbo.xp_readerrorlog 0, 1, N'error'",
            "EXEC master.sys.xp_readerrorlog",
            "EXEC master..xp_readerrorlog",
            "EXEC [master].[sys].[xp_readerrorlog] 0",
            "xp_readerrorlog 0, 1",
            "EXEC sp_readerrorlog",
            "EXEC sys.sp_readerrorlog 1, 1, N'deadlock'",
            "EXEC sp_enumerrorlogs 1",
            "EXEC master.sys.sp_enumerrorlogs",
            "sp_readerrorlog",
            "DECLARE @dir nvarchar(4000);\n\
             EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE',\n\
             N'Software\\Microsoft\\MSSQLServer\\MSSQLServer', N'BackupDirectory', @dir OUTPUT;\n\
             SELECT @dir",
            "EXEC @rc = sys.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'k', N'v'",
            "EXEC xp_fixeddrives",
            "EXEC master.sys.xp_fixeddrives",
            "EXEC xp_msver",
            "EXEC xp_msver N'ProductVersion'",
            "EXEC sp_configure",
            "EXEC sp_configure 'max server memory (MB)'",
            "EXEC sys.sp_configure @configname = N'max degree of parallelism'",
            "EXEC sp_configure N'cost threshold for parallelism'; SELECT 1, 2",
            "EXEC sp_configure\nSELECT 1, 2",
            "EXEC sp_configure; THROW 50000, N'stop', 1",
            "EXEC sp_configure;1",
            "EXEC sp_configure;1 N'max server memory (MB)'",
            "CREATE TABLE #log (d datetime, p nvarchar(50), t nvarchar(max));\n\
             INSERT INTO #log EXEC xp_readerrorlog 0, 1;\n\
             SELECT TOP (20) * FROM #log ORDER BY d DESC",
        ] {
            allowed(sql);
        }
        // Neighbours that write, run commands or read any key stay blocked, as do names that
        // only share a prefix with an allowed one.
        for (sql, what) in [
            ("EXEC xp_cmdshell 'dir'", "EXEC xp_cmdshell"),
            (
                "EXEC master.dbo.xp_instance_regwrite N'HKEY_LOCAL_MACHINE', N'k', N'v', N'REG_SZ', N'x'",
                "EXEC master.dbo.xp_instance_regwrite",
            ),
            (
                "EXEC xp_regwrite N'HKEY_LOCAL_MACHINE', N'k', N'v', N'REG_SZ', N'x'",
                "EXEC xp_regwrite",
            ),
            (
                "EXEC xp_regdeletevalue N'HKEY_LOCAL_MACHINE', N'k', N'v'",
                "EXEC xp_regdeletevalue",
            ),
            (
                "EXEC xp_regread N'HKEY_LOCAL_MACHINE', N'k', N'v'",
                "EXEC xp_regread",
            ),
            ("EXEC xp_readerrorlogs", "EXEC xp_readerrorlogs"),
            ("EXEC xp_readerror", "EXEC xp_readerror"),
            ("EXEC sp_configure_peer", "EXEC sp_configure_peer"),
        ] {
            let b = blocked(sql);
            assert_eq!(
                b.reason,
                format!(
                    "{what} isn't on sqlh's list of read-only procedures, so sqlh can't tell \
                     what it does"
                ),
                "{sql}"
            );
        }
        // Only the local system procedure counts, as for the sp_ ones.
        for sql in [
            "EXEC dbo.xp_readerrorlog",
            "EXEC otherdb.dbo.xp_readerrorlog",
            "EXEC Lnk.master.dbo.xp_readerrorlog",
            "Lnk.master.sys.xp_readerrorlog 0, 1",
            "EXEC tools.sp_configure",
        ] {
            assert!(
                blocked(sql)
                    .reason
                    .contains("only a bare, sys. or master. name"),
                "{sql}"
            );
        }
        // Stacked statements are each checked: an allowed call doesn't carry a write.
        for (sql, line) in [
            ("EXEC xp_readerrorlog 0, 1; DELETE dbo.T", 1),
            ("EXEC xp_msver\nUPDATE dbo.T SET x = 1", 2),
            ("EXEC sp_readerrorlog\nEXEC xp_cmdshell 'dir'", 2),
            ("EXEC xp_fixeddrives; EXEC sp_configure\nRECONFIGURE", 2),
        ] {
            assert_eq!(blocked(sql).line, line, "{sql}");
        }
    }

    #[test]
    fn sp_configure_with_a_value_is_blocked() {
        for sql in [
            "EXEC sp_configure 'show advanced options', 1",
            "EXEC sys.sp_configure N'max server memory (MB)', 4096; RECONFIGURE",
            "EXEC sp_configure @configname = N'xp_cmdshell', @configvalue = 1",
            "EXEC sp_configure @configvalue = 0",
            "EXEC master.dbo.sp_configure NULL, 1",
            "sp_configure 'max degree of parallelism', 4",
            // The procedure number (`name;number`): its arguments follow it.
            "EXEC sp_configure;1 'show advanced options', 1",
            "sp_configure;1 'show advanced options', 1",
            "EXEC master.dbo.sp_configure ;1 @configvalue = 1",
            "EXEC sp_configure /* n */ ; /* n */ 1 'x', 1",
            // A bare value spelled like a statement keyword T-SQL doesn't reserve.
            "EXEC sp_configure GET, 1",
            "EXEC sp_configure throw, 1",
            "EXEC sp_configure GET, @configvalue = 1",
            "EXEC sp_configure @configname = MOVE, @configvalue = 1",
            "EXEC sp_configure send, 1",
        ] {
            let b = blocked(sql);
            assert_eq!(
                b.reason,
                "sp_configure with @configvalue (named, or as the second argument) changes a \
                 server setting",
                "{sql}"
            );
            assert_eq!(
                b.advice,
                "Read sys.configurations to see every setting, advanced ones too, or rerun \
                 with --allow-write",
                "{sql}"
            );
        }
        let parens = "(".repeat(MAX_ARG_PARENS);
        assert_eq!(
            blocked(&format!("EXEC sp_configure {parens}(")).reason,
            format!(
                "this input is too complex for the read-only check: the arguments of \
                 sp_configure have more than {MAX_ARG_PARENS} parentheses"
            )
        );
    }

    // Decision 45: a batch gets the longer count when a stopped count could cancel output from
    // a later statement. It may say yes where nothing would be lost (the cost is a longer
    // count), never no where something would.
    #[test]
    fn several_outputs_counts_the_statements_a_stop_could_cancel() {
        for sql in [
            // The client's case: three queries, no GO.
            "SELECT * FROM dbo.Account; SELECT 'second statement ran'; SELECT COUNT(*) FROM t",
            "SELECT a FROM t SELECT b FROM u",
            "select a from t\nselect b from u",
            // A procedure can return several results.
            "EXEC sp_helpdb",
            "EXECUTE dbo.p @a = 1",
            "sp_helpdb",
            "[dbo].[p]",
            "SET NOCOUNT ON; EXEC sp_who2",
            // Messages count too.
            "SELECT * FROM t; PRINT 'done'",
            "SELECT * FROM t\nRAISERROR('x', 10, 1)",
            "DBCC TRACESTATUS; SELECT 1",
            // Both branches, either may run after the other's statements.
            "IF @x = 1 SELECT a FROM t ELSE SELECT b FROM t",
            "BEGIN TRY SELECT a FROM t END TRY BEGIN CATCH SELECT ERROR_MESSAGE() END CATCH",
            "WHILE @i < 3 BEGIN SELECT @i; SET @i += 1 END; SELECT 2",
            // A loop repeats its one SELECT: a stop in one pass cancels the passes after it.
            "DECLARE @i int = 0; WHILE @i < 3 BEGIN SELECT * FROM t; SET @i += 1; END",
            "again: SELECT * FROM t; IF @@ROWCOUNT > 0 GOTO again",
            // A second CTE statement is a second statement.
            "WITH c AS (SELECT 1 AS a) SELECT * FROM c; WITH d AS (SELECT 2 AS b) SELECT * FROM d",
            "SELECT 1; WITH c AS (SELECT 1 AS a) SELECT * FROM c",
            // A hint's WITH starts nothing, but what follows the statement still counts.
            "SELECT * FROM t WITH (NOLOCK) SELECT 2",
            "SELECT * FROM t WHERE x IN (1, 2) SELECT 2",
            // Over-counting is allowed: the assignment returns nothing, but costs only a count.
            "DECLARE @n int; SELECT @n = COUNT(*) FROM t; SELECT * FROM t",
            "INSERT #t SELECT a FROM t; SELECT * FROM #t",
            // A query in parentheses is a statement too.
            "(SELECT a FROM t); SELECT b FROM u",
            "SELECT b FROM u; (SELECT a FROM t)",
            // R48: options that add output to every query after them, and DBCC commands, whose
            // results come several to a statement (SHOW_STATISTICS: header, density,
            // histogram).
            "SET STATISTICS XML ON; SELECT * FROM t",
            "SET STATISTICS PROFILE ON\nSELECT * FROM t",
            "set statistics io, time on; select * from t",
            "SET SHOWPLAN_XML ON",
            "SET STATISTICS XML OFF; SELECT * FROM t",
            "DBCC SHOW_STATISTICS ('dbo.t', 'ix_t')",
            "DBCC SHOW_STATISTICS ('dbo.t', 'ix_t') WITH HISTOGRAM",
        ] {
            assert!(several_outputs(sql), "{sql}");
        }
        for sql in [
            "SELECT * FROM t",
            "SELECT a FROM t;",
            "SET NOCOUNT ON; SELECT * FROM t",
            "DECLARE @d date = GETDATE(); SELECT * FROM t WHERE d < @d",
            "USE msdb; SELECT * FROM dbo.sysjobhistory",
            "DECLARE @x int = (SELECT COUNT(*) FROM t); SELECT * FROM u",
            "SELECT * FROM t WITH (NOLOCK)",
            "SELECT TOP (5) WITH TIES a FROM t ORDER BY a",
            "SELECT a, COUNT(*) FROM t GROUP BY a WITH ROLLUP",
            "WITH c AS (SELECT 1 AS a) SELECT * FROM c",
            "WITH c (a) AS (SELECT 1), d AS (SELECT 2 AS b) SELECT * FROM c, d",
            "SELECT a FROM t UNION SELECT a FROM u",
            "SELECT a FROM t UNION ALL SELECT a FROM u EXCEPT SELECT a FROM v",
            "SELECT a FROM t INTERSECT (SELECT a FROM u)",
            "(SELECT a FROM t) UNION (SELECT a FROM u)",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = t.a)",
            "IF @x = 1 SELECT a FROM t",
            "SELECT 'EXEC p; SELECT 2' AS s -- SELECT 3",
            "/* SELECT 1 */ SELECT 2",
            "label: SELECT 1",
            "SET NOCOUNT ON; SET LOCK_TIMEOUT 0; SELECT * FROM t",
            "SELECT 'SET STATISTICS XML ON' AS s",
            "",
        ] {
            assert!(!several_outputs(sql), "{sql}");
        }
    }

    // Decision 30(a). The forms were each run on SQL Server 2016 and 2025: what they accept as a
    // hint counts, and what they read as a name doesn't.
    #[test]
    fn uncommitted_reads_finds_what_the_server_takes_as_a_hint() {
        const NONE: UncommittedReads = UncommittedReads {
            nolock: false,
            readuncommitted: false,
            isolation: false,
        };
        const NOLOCK: UncommittedReads = UncommittedReads {
            nolock: true,
            ..NONE
        };
        const HINT: UncommittedReads = UncommittedReads {
            readuncommitted: true,
            ..NONE
        };
        const SET: UncommittedReads = UncommittedReads {
            isolation: true,
            ..NONE
        };
        for (sql, expected) in [
            // WITH, in any case and any place in its list.
            ("SELECT * FROM t WITH (NOLOCK)", NOLOCK),
            ("select * from t with(nolock)", NOLOCK),
            ("SELECT * FROM t WITH (READUNCOMMITTED)", HINT),
            ("SELECT * FROM dbo.t AS a WITH (INDEX(1), NOLOCK)", NOLOCK),
            ("SELECT * FROM t WITH (NOLOCK, INDEX(1))", NOLOCK),
            ("SELECT * FROM t WITH (NOLOCK INDEX(1))", NOLOCK),
            ("SELECT * FROM t WITH (INDEX(1) NOLOCK)", NOLOCK),
            ("SELECT * FROM t WITH /* c */ ( -- d\n NOLOCK )", NOLOCK),
            ("SELECT * FROM a JOIN b WITH (NOLOCK) ON 1 = 1", NOLOCK),
            ("SELECT * FROM t CROSS APPLY u WITH (NOLOCK)", NOLOCK),
            ("IF EXISTS (SELECT 1 FROM t WITH (NOLOCK)) SELECT 1", NOLOCK),
            ("DECLARE c CURSOR FOR SELECT a FROM t WITH (NOLOCK)", NOLOCK),
            ("UPDATE #t SET a = 1 FROM u WITH (NOLOCK)", NOLOCK),
            ("MERGE #t USING s WITH (NOLOCK) ON 1 = 0", NOLOCK),
            // Without WITH: alone in its parentheses, on a table or its alias.
            ("SELECT * FROM t (NOLOCK)", NOLOCK),
            ("SELECT * FROM t(nolock)", NOLOCK),
            ("SELECT * FROM t (READUNCOMMITTED)", HINT),
            ("SELECT * FROM dbo.t (NOLOCK)", NOLOCK),
            ("SELECT * FROM master..t (NOLOCK)", NOLOCK),
            ("SELECT * FROM [dbo].[t] [a] (NOLOCK)", NOLOCK),
            ("SELECT * FROM #t (NOLOCK)", NOLOCK),
            ("SELECT * FROM t a (NOLOCK)", NOLOCK),
            ("SELECT * FROM t AS a (NOLOCK)", NOLOCK),
            ("SELECT * FROM t ([NOLOCK])", NOLOCK),
            ("SELECT * FROM t (\"NOLOCK\")", NOLOCK),
            ("SELECT * FROM nolock (NOLOCK)", NOLOCK),
            ("SELECT * FROM t nolock (NOLOCK)", NOLOCK),
            ("SELECT * FROM a JOIN b (NOLOCK) ON 1 = 1", NOLOCK),
            (
                "SELECT * FROM a LEFT HASH JOIN b c (NOLOCK) ON 1 = 1",
                NOLOCK,
            ),
            ("SELECT * FROM (a (NOLOCK) JOIN b ON 1 = 1)", NOLOCK),
            ("SELECT * FROM (SELECT * FROM t (NOLOCK)) AS d", NOLOCK),
            ("MERGE #t USING s (NOLOCK) ON 1 = 0", NOLOCK),
            ("MERGE #t USING s a (NOLOCK) ON 1 = 0", NOLOCK),
            // A comma goes on to the next table, past an alias or another's hint.
            ("SELECT * FROM a, b (NOLOCK)", NOLOCK),
            ("SELECT * FROM a x, b y (NOLOCK)", NOLOCK),
            ("SELECT * FROM a (ROWLOCK), b (NOLOCK)", NOLOCK),
            ("SELECT * FROM a (ROWLOCK) x, b (NOLOCK)", NOLOCK),
            ("SELECT * FROM a (ROWLOCK) AS x, b (NOLOCK)", NOLOCK),
            ("SELECT * FROM a WITH (ROWLOCK), b (NOLOCK)", NOLOCK),
            ("SELECT * FROM a x WITH (ROWLOCK), b (NOLOCK)", NOLOCK),
            (
                "DECLARE @a TABLE (i int); SELECT * FROM @a, b (NOLOCK)",
                NOLOCK,
            ),
            // The level, however it is spaced.
            ("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED", SET),
            ("set tran isolation level read uncommitted; select 1", SET),
            (
                "SET TRANSACTION /* a */ ISOLATION LEVEL READ -- b\n UNCOMMITTED",
                SET,
            ),
            // All of them, however many times.
            (
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; \
                 SELECT * FROM a WITH (NOLOCK), b (NOLOCK), c WITH (READUNCOMMITTED)",
                UncommittedReads {
                    nolock: true,
                    readuncommitted: true,
                    isolation: true,
                },
            ),
            // A name, a string or a comment.
            ("SELECT nolock FROM t", NONE),
            ("SELECT 1 AS nolock", NONE),
            ("SELECT 1 AS readuncommitted", NONE),
            ("SELECT t.nolock FROM t ORDER BY nolock", NONE),
            ("SELECT [NOLOCK] FROM t", NONE),
            ("SELECT 'WITH (NOLOCK)', N'(NOLOCK)' FROM t", NONE),
            ("SELECT 1 -- FROM t WITH (NOLOCK)\nFROM t", NONE),
            ("SELECT 1 /* FROM t (NOLOCK) */ FROM t", NONE),
            (
                "SELECT 'SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED'",
                NONE,
            ),
            // An alias, which leaves the read committed.
            ("SELECT * FROM t nolock", NONE),
            ("SELECT * FROM t AS nolock", NONE),
            ("SELECT * FROM nolock", NONE),
            ("SELECT * FROM t nolock WHERE 1 = 1", NONE),
            (
                "SELECT * FROM a nolock JOIN b readuncommitted ON 1 = 1",
                NONE,
            ),
            ("SELECT * FROM a nolock, b readuncommitted", NONE),
            // A column in parentheses, where no table is being named.
            ("SELECT (nolock) FROM t", NONE),
            ("SELECT ISNULL(nolock, 0) FROM t", NONE),
            ("SELECT COUNT(nolock), f (nolock) FROM t", NONE),
            ("SELECT * FROM t WHERE (nolock) = 1", NONE),
            ("SELECT * FROM t x WHERE (nolock) = 1", NONE),
            ("SELECT * FROM t WHERE a IN (nolock)", NONE),
            ("SELECT * FROM t GROUP BY (nolock)", NONE),
            ("SELECT * FROM t ORDER BY a, (nolock)", NONE),
            ("SELECT * FROM t x ORDER BY a, f (nolock)", NONE),
            ("SELECT * FROM a JOIN b ON (nolock) = 1", NONE),
            ("SELECT a FROM t UNION (SELECT nolock FROM u)", NONE),
            ("SELECT * FROM t SELECT (nolock)", NONE),
            ("SELECT * FROM t IF (nolock) = 1 PRINT 1", NONE),
            ("SELECT * FROM t WHILE (nolock) = 1 PRINT 1", NONE),
            ("SELECT * FROM t OPTION (nolock)", NONE),
            ("SELECT * FROM t PIVOT (MAX(a) FOR b IN (nolock)) p", NONE),
            ("SELECT * FROM t TABLESAMPLE (nolock)", NONE),
            // A column list.
            ("CREATE TABLE #t (nolock int)", NONE),
            ("INSERT #t (nolock) SELECT 1", NONE),
            ("INSERT INTO #t (nolock) VALUES (1)", NONE),
            ("CREATE INDEX i ON #t (nolock)", NONE),
            ("WITH c (nolock) AS (SELECT 1) SELECT nolock FROM c", NONE),
            ("SELECT * FROM (VALUES (1)) v(nolock)", NONE),
            ("SELECT * FROM (VALUES (1)) AS v (nolock)", NONE),
            ("SELECT * FROM (SELECT 1 AS a) d (nolock)", NONE),
            ("SELECT * FROM (SELECT 1 AS a) AS d (nolock)", NONE),
            ("SELECT * FROM f(1) a (nolock)", NONE),
            ("SELECT * FROM f(1) AS a (nolock)", NONE),
            // A function's arguments: the server takes no list without WITH, and after APPLY the
            // left side's column is the usual reading.
            ("SELECT * FROM f(1, nolock)", NONE),
            ("SELECT * FROM f(a, nolock)", NONE),
            ("SELECT * FROM f(nolock, 1)", NONE),
            ("SELECT * FROM t (ROWLOCK, NOLOCK)", NONE),
            ("SELECT * FROM t CROSS APPLY f(nolock)", NONE),
            ("SELECT * FROM t OUTER APPLY dbo.f(nolock) x", NONE),
            // What else WITH ( opens.
            ("SELECT * FROM t WITH (INDEX = nolock)", NONE),
            ("SELECT * FROM t WITH (INDEX(nolock))", NONE),
            ("SELECT * FROM t WITH (INDEX = (nolock))", NONE),
            ("SELECT * FROM t WITH (FORCESEEK(nolock(a)))", NONE),
            ("SELECT * FROM OPENJSON(@j) WITH (nolock int)", NONE),
            (
                "SELECT * FROM OPENJSON(@j) WITH (a int, nolock int '$.x')",
                NONE,
            ),
            (
                "SELECT * FROM OPENJSON(@j) WITH (nolock nvarchar(9), b int)",
                NONE,
            ),
            ("CREATE INDEX i ON #t (a) WITH (nolock = ON)", NONE),
            ("WITH XMLNAMESPACES ('u' AS nolock) SELECT 1", NONE),
            ("EXEC sp_help WITH RESULT SETS ((nolock int))", NONE),
            // A statement that only stores its text.
            ("CREATE VIEW v AS SELECT * FROM t WITH (NOLOCK)", NONE),
            ("create or alter view v as select * from t (nolock)", NONE),
            (
                "/* c */ CREATE PROC p AS SELECT * FROM t WITH (NOLOCK)",
                NONE,
            ),
            (
                "CREATE PROCEDURE p AS SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
                NONE,
            ),
            (
                "ALTER FUNCTION f() RETURNS int AS BEGIN RETURN (SELECT 1 FROM t (NOLOCK)) END",
                NONE,
            ),
            (
                "ALTER TRIGGER r ON t AFTER UPDATE AS SELECT 1 FROM u WITH (NOLOCK)",
                NONE,
            ),
            // Other levels and other SETs.
            ("SET TRANSACTION ISOLATION LEVEL READ COMMITTED", NONE),
            ("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", NONE),
            ("SET LOCK_TIMEOUT 0", NONE),
            ("UPDATE #t SET uncommitted = 1", NONE),
            // Unfinished input, which the server refuses.
            ("SELECT * FROM t WITH (NOLOCK", NONE),
            ("SELECT * FROM t (NOLOCK", NONE),
            ("SELECT * FROM", NONE),
            ("", NONE),
            // Dynamic SQL is text the scan doesn't read.
            ("EXEC ('SELECT * FROM t WITH (NOLOCK)')", NONE),
        ] {
            assert_eq!(uncommitted_reads(sql), expected, "{sql}");
        }
    }

    // The limits DESIGN.md §4.3 states, held so that a change to them is a change to the contract.
    #[test]
    fn uncommitted_reads_goes_only_as_far_as_the_text_shows() {
        // The server reads these as a table function given an outer column: the text can't tell.
        assert!(uncommitted_reads("SELECT (SELECT 1 FROM f(nolock)) FROM t").nolock);
        // The table list isn't followed past a derived table or an ON: these are missed.
        assert!(!uncommitted_reads("SELECT * FROM (SELECT 1 AS x) a, b (NOLOCK)").nolock);
        assert!(!uncommitted_reads("SELECT * FROM a JOIN b ON 1 = 1, c (NOLOCK)").nolock);
        // A bare hint after APPLY is missed, for the sake of a function given a column.
        assert!(!uncommitted_reads("SELECT * FROM a CROSS APPLY b (NOLOCK)").nolock);
    }

    // The scan has no budget, so the shapes that make it look ahead must stay linear; a quadratic
    // read of these would take hours.
    #[test]
    fn uncommitted_reads_stays_linear_on_the_shapes_that_look_ahead() {
        for piece in [
            "WITH (NOLOCK ",
            "FROM t (",
            "FROM t (NOLOCK ",
            "SET TRANSACTION ISOLATION LEVEL READ ",
            "(",
            ")",
            ", ",
        ] {
            let sql = piece.repeat(200_000);
            let started = std::time::Instant::now();
            uncommitted_reads(&sql);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "{piece}: {:?}",
                started.elapsed()
            );
        }
    }

    // R48: a SET of an option that adds output lasts for the session, so the run treats every
    // batch from its own on as one whose stop could cancel output.
    #[test]
    fn options_that_add_output_are_found_for_the_batches_after_them() {
        for sql in [
            "SET STATISTICS PROFILE ON",
            "SET STATISTICS XML ON; SELECT 1",
            "set statistics io on",
            "SET STATISTICS IO, TIME ON",
            "SET SHOWPLAN_ALL ON",
            "SET SHOWPLAN_TEXT ON",
            "set showplan_xml on",
            "SET STATISTICS XML OFF",
        ] {
            assert!(sets_output_modes(sql), "{sql}");
        }
        for sql in [
            "SELECT * FROM t",
            "SET NOCOUNT ON",
            "SELECT 'SET STATISTICS XML ON' AS s -- SET STATISTICS IO ON",
            "/* SET SHOWPLAN_XML ON */ SELECT 1",
            "DBCC SHOW_STATISTICS ('dbo.t', 'ix_t')",
            "SELECT * FROM sys.stats",
            "",
        ] {
            assert!(!sets_output_modes(sql), "{sql}");
        }
    }

    // plan-history.md 2.15: input that says XACT_ABORT gets exact counting.
    #[test]
    fn xact_abort_is_found_among_the_tokens() {
        for sql in [
            "SET XACT_ABORT ON",
            "select 1\nset xact_abort on\nselect 2",
            "SET NOCOUNT ON; SET /* x */ Xact_Abort OFF;",
            "CREATE PROC p AS SET XACT_ABORT ON",
        ] {
            assert!(mentions_xact_abort(sql), "{sql}");
        }
        for sql in [
            "SELECT 1",
            "SELECT 'XACT_ABORT' AS [XACT_ABORT] -- XACT_ABORT",
            "/* SET XACT_ABORT ON */ SELECT 1",
            "SELECT XACT_ABORT_2 FROM t",
            "",
        ] {
            assert!(!mentions_xact_abort(sql), "{sql}");
        }
    }

    // F04: --check must not be switchable back to execution by its own input.
    #[test]
    fn compile_only_refuses_noexec() {
        let check_writes = Policy {
            writes: true,
            compile_only: true,
            reused_session: false,
        };
        let check_read_only = Policy {
            writes: false,
            compile_only: true,
            reused_session: false,
        };
        for policy in [check_writes, check_read_only] {
            for sql in [
                "SET NOEXEC OFF",
                "SET NOEXEC ON",
                "select 1\nset noexec off\nselect 2",
            ] {
                let b = check(sql, policy).expect_err(sql);
                assert!(b.reason.contains("--check"), "{}", b.reason);
                // R04: --allow-write can't lift this refusal, so the advice mustn't suggest it.
                assert!(b.advice.starts_with("Remove SET NOEXEC"), "{}", b.advice);
                assert!(!b.advice.contains("Rerun"), "{}", b.advice);
            }
            assert_eq!(
                check("SELECT 'NOEXEC' AS [NOEXEC] -- NOEXEC", policy),
                Ok(())
            );
        }
        assert_eq!(check("INSERT dbo.T VALUES (1)", check_writes), Ok(()));
        assert!(check("INSERT dbo.T VALUES (1)", check_read_only).is_err());
        let open = Policy {
            writes: true,
            compile_only: false,
            reused_session: false,
        };
        assert_eq!(check("SET NOEXEC OFF; DROP TABLE dbo.T", open), Ok(()));
    }

    // F05: malformed input must never panic.
    #[test]
    fn unterminated_quotes_do_not_panic() {
        for sql in [
            "SELECT [é",
            "SELECT \"é",
            "SELECT 'é",
            "SELECT N'é",
            "[é",
            "\"",
            "[",
            "[]]",
            "SELECT [a]]é",
        ] {
            let _ = check(sql, READ_ONLY);
            let _ = check(
                sql,
                Policy {
                    writes: true,
                    compile_only: true,
                    reused_session: false,
                },
            );
        }
        // An unterminated string hides everything after it; the server then reports the
        // syntax error, and nothing in the batch runs.
        allowed("SELECT 'unterminated; DELETE FROM t");
    }

    #[test]
    fn arbitrary_input_never_panics() {
        const PIECES: &[&str] = &[
            "[",
            "]",
            "]]",
            "'",
            "''",
            "\"",
            "N'",
            "/*",
            "*/",
            "--",
            "\n",
            "\r\n",
            " ",
            "é",
            "😀",
            "SELECT",
            "EXEC",
            "sp_spaceused",
            ",",
            "(",
            ")",
            ";",
            ".",
            "..",
            "#",
            "##",
            "@",
            "@x",
            "=",
            "NEXT",
            "VALUE",
            "FOR",
            "NOEXEC",
            "INSERT",
            "ALTER",
            "CREATE",
            "DBCC",
            "INTO",
            "TOP",
            "GO",
            "x",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 12) as usize;
            let sql: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let _ = check(&sql, READ_ONLY);
            let _ = check(
                &sql,
                Policy {
                    writes: true,
                    compile_only: true,
                    reused_session: false,
                },
            );
        }
    }

    // plan-history.md 2.14: the guard reads the tokens as a stream and keeps none of them. The
    // tests below pin the stream itself, the case-insensitive matching that replaced the uppercased
    // copy of every token, and inputs with millions of tokens.
    type Seen<'a> = (Kind, &'a str, &'a str, u32);

    fn lex(sql: &str) -> Vec<Seen<'_>> {
        Lexer::new(sql)
            .map(|t| (t.kind, t.text, t.name, t.line))
            .collect()
    }

    #[test]
    fn the_lexer_reads_each_kind_of_token() {
        use Kind::*;
        assert_eq!(
            lex("SELECT [a b], \"c\"\"d\", N'it''s', @v, @@r, #t, ##g, 1.5e3, x.y;"),
            vec![
                (Word, "SELECT", "SELECT", 1),
                (Quoted, "[a b]", "a b", 1),
                (Punct(','), ",", "", 1),
                (Quoted, "\"c\"\"d\"", "c\"\"d", 1),
                (Punct(','), ",", "", 1),
                (Str, "N'it''s'", "", 1),
                (Punct(','), ",", "", 1),
                (Var, "@v", "@v", 1),
                (Punct(','), ",", "", 1),
                (Var, "@@r", "@@r", 1),
                (Punct(','), ",", "", 1),
                (Temp, "#t", "#t", 1),
                (Punct(','), ",", "", 1),
                (Temp, "##g", "##g", 1),
                (Punct(','), ",", "", 1),
                (Num, "1.5e3", "", 1),
                (Punct(','), ",", "", 1),
                (Word, "x", "x", 1),
                (Punct('.'), ".", "", 1),
                (Word, "y", "y", 1),
                (Punct(';'), ";", "", 1),
            ]
        );
        // Words take any byte of a multi-byte character, and the name keeps the case written.
        assert_eq!(
            lex("naïve é😀x"),
            vec![(Word, "naïve", "naïve", 1), (Word, "é😀x", "é😀x", 1),]
        );
        assert_eq!(lex(""), vec![]);
        assert_eq!(lex(" \r\n\t -- only a comment\n/* and another */ "), vec![]);
    }

    #[test]
    fn tokens_carry_the_line_they_start_on() {
        use Kind::*;
        // Comments (nested ones too), strings and quoted names span lines; a token's line is
        // the one it starts on, and the lines it spans count for the tokens after it.
        assert_eq!(
            lex("a -- c\n/* x\n/* nested */\n*/ b 'q\nr' [s\nt] c"),
            vec![
                (Word, "a", "a", 1),
                (Word, "b", "b", 4),
                (Str, "'q\nr'", "", 4),
                (Quoted, "[s\nt]", "s\nt", 5),
                (Word, "c", "c", 6),
            ]
        );
        assert_eq!(
            lex("x\r\ny\n\nz"),
            vec![
                (Word, "x", "x", 1),
                (Word, "y", "y", 2),
                (Word, "z", "z", 4),
            ]
        );
    }

    #[test]
    fn the_lexer_ends_malformed_input_at_its_end() {
        use Kind::*;
        // An unterminated string or name runs to the end; a quoted name then has no closing
        // delimiter to strip, and a doubled closing delimiter at the end is still an escape.
        assert_eq!(
            lex("SELECT [é"),
            vec![(Word, "SELECT", "SELECT", 1), (Quoted, "[é", "é", 1)]
        );
        assert_eq!(lex("[a]]"), vec![(Quoted, "[a]]", "a]]", 1)]);
        assert_eq!(lex("\"x"), vec![(Quoted, "\"x", "x", 1)]);
        assert_eq!(lex("'abc\ndef"), vec![(Str, "'abc\ndef", "", 1)]);
        assert_eq!(lex("[]"), vec![(Quoted, "[]", "", 1)]);
        assert_eq!(lex("a /* never closed\nb"), vec![(Word, "a", "a", 1)]);
        assert_eq!(lex("N'"), vec![(Str, "N'", "", 1)]);
    }

    #[test]
    fn a_lexer_is_a_position_that_can_be_copied() {
        fn text(token: Option<Token<'_>>) -> Option<&str> {
            token.map(|t| t.text)
        }
        let lex = Lexer::new("a b\nc");
        assert_eq!(text(lex.ahead(0)), Some("a"));
        assert_eq!(text(lex.ahead(2)), Some("c"));
        assert_eq!(text(lex.ahead(3)), None);
        assert_eq!(lex.ahead(2).map(|t| t.line), Some(2));
        assert_eq!(text(lex.after(1).ahead(0)), Some("b"));
        assert_eq!(text(lex.after(2).ahead(0)), Some("c"));
        assert_eq!(text(lex.after(3).ahead(0)), None);
        assert_eq!(text(lex.after(1_000).ahead(0)), None);
        assert_eq!(lex.after(0).ahead(0).map(|t| t.line), Some(1));
        // Looking ahead moved nothing.
        assert_eq!(text(lex.ahead(0)), Some("a"));
        assert!(lex.word_is(0, "A") && !lex.word_is(1, "A") && lex.word(1) == Some("b"));
        assert_eq!(lex.kind(0), Some(Kind::Word));
        assert_eq!(Lexer::new("(").kind(0), Some(Kind::Punct('(')));
        assert_eq!(Lexer::new("'a'").word(0), None);
    }

    #[test]
    fn keywords_are_upper_cased_into_a_small_buffer() {
        let mut buf = [0; KEYWORD_MAX];
        assert_eq!(keyword("insert", &mut buf), "INSERT");
        assert_eq!(keyword("InSeRt", &mut buf), "INSERT");
        // No keyword has a letter beyond ASCII, so a word with one is no keyword.
        assert_eq!(keyword("é_x", &mut buf), "");
        assert_eq!(keyword("", &mut buf), "");
        let longest = "a".repeat(KEYWORD_MAX);
        assert_eq!(keyword(&longest, &mut buf), longest.to_ascii_uppercase());
        // One byte more than a keyword can have: no arm of the classifier takes "".
        assert_eq!(keyword(&"a".repeat(KEYWORD_MAX + 1), &mut buf), "");
        for list in [BLOCKED, PROCS, DBCC_READ, STATEMENT_START] {
            for entry in list {
                assert_eq!(*entry, entry.to_ascii_uppercase(), "lists are upper case");
            }
        }
        // An argument check applies only to a procedure the allowlist admits.
        for arg in ARG_CHECKS {
            assert!(PROCS.contains(&arg.name), "{} isn't in PROCS", arg.name);
            assert_eq!(arg.param, arg.param.to_ascii_uppercase(), "{}", arg.param);
        }
        for entry in BLOCKED {
            assert!(
                entry.len() <= KEYWORD_MAX,
                "{entry} doesn't fit KEYWORD_MAX"
            );
        }
    }

    /// Alternating case: neither all upper nor all lower.
    fn alternating(word: &str) -> String {
        word.chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect()
    }

    #[test]
    fn every_list_matches_in_any_case() {
        for word in BLOCKED {
            for spelled in [word.to_ascii_lowercase(), alternating(word)] {
                let b = blocked(&format!("SELECT {spelled} FROM t"));
                assert_eq!(
                    b.reason,
                    format!(
                        "{word} is not allowed in read-only mode (if it is a column name, write it as [{spelled}])"
                    )
                );
            }
        }
        for proc in PROCS {
            for spelled in [proc.to_ascii_lowercase(), alternating(proc)] {
                allowed(&format!("EXEC {spelled}"));
                allowed(&format!("exec Sys.{spelled}"));
                allowed(&format!("EXEC [master].[dbo].[{spelled}]"));
                // An implicit call is the same word as the batch's first.
                allowed(&spelled);
            }
        }
        for command in DBCC_READ {
            allowed(&format!("dbcc {}", command.to_ascii_lowercase()));
            allowed(&format!("DbCc {}", alternating(command)));
        }
        assert_eq!(
            blocked("dbcc freeproccache").reason,
            "DBCC FREEPROCCACHE isn't on sqlh's list of read-only DBCC commands"
        );
        // No command to name, or none that is a bare word: refused all the same.
        assert_eq!(
            blocked("dbcc").reason,
            "DBCC ? isn't on sqlh's list of read-only DBCC commands"
        );
        assert_eq!(
            blocked("DBCC [OPENTRAN]").reason,
            "DBCC [OPENTRAN] isn't on sqlh's list of read-only DBCC commands"
        );
        assert_eq!(
            blocked("DBCC 'x'").reason,
            "DBCC ? isn't on sqlh's list of read-only DBCC commands"
        );
        for word in STATEMENT_START {
            for spelled in [word.to_ascii_lowercase(), alternating(word)] {
                // Whatever else the batch does, it isn't taken for a procedure call.
                if let Err(b) = check(&format!("{spelled} x"), READ_ONLY) {
                    assert!(!b.reason.contains("starts the batch"), "{spelled}: {b:?}");
                }
            }
        }
        assert!(blocked("selectx 1").reason.contains("starts the batch"));
        for (sql, what) in [
            ("begin tran", "BEGIN TRAN"),
            ("Begin Transaction", "BEGIN TRANSACTION"),
            ("BEGIN distributed TRAN", "BEGIN DISTRIBUTED"),
            ("begin dialog conversation @h", "BEGIN DIALOG"),
        ] {
            assert_eq!(blocked(sql).reason, format!("{what} would write"), "{sql}");
        }
        assert!(
            check(
                "sET nOeXeC oN",
                Policy {
                    writes: true,
                    compile_only: true,
                    reused_session: false,
                }
            )
            .is_err()
        );
        assert!(
            blocked("exec sp_spaceused @UpdateUsage = 1")
                .reason
                .contains("UPDATEUSAGE")
        );
        assert!(
            blocked("select next value for dbo.s")
                .reason
                .contains("dbo.s")
        );
    }

    #[test]
    fn long_words_are_names_not_keywords() {
        let long = "x".repeat(100_000);
        allowed(&format!("SELECT {long} FROM t"));
        allowed(&format!("SELECT [{long}], '{long}' FROM {long}"));
        // Past the keyword buffer, but a name the procedure list has.
        allowed("exec sp_describe_undeclared_parameters N'select 1'");
        allowed("EXEC SP_DESCRIBE_UNDECLARED_PARAMETERS N'select 1'");
        // Words that begin like a keyword, and are longer than one, are not that keyword.
        allowed("SELECT INSERTXXXXXXXXXXXXXXXXX, UPDATEUPDATEUPDATEUPDATE, DROPPED, INTOO FROM t");
        allowed("SELECT OPENDATASOURCES, OPENDATASOURCEXX, SENDER, CREATED FROM t");
        let b = blocked("SELECT OPENDATASOURCE");
        assert!(b.reason.starts_with("OPENDATASOURCE is not allowed"));
        // A name's message carries the name as written, whatever its length.
        let b = blocked(&format!("EXEC {long}"));
        assert_eq!(
            b.reason,
            format!(
                "EXEC {long} isn't on sqlh's list of read-only procedures, so sqlh can't tell \
                 what it does"
            )
        );
        assert_eq!(
            blocked(&format!("{long} 1")).reason,
            format!(
                "{long} starts the batch, so SQL Server would run it as a stored procedure (a typo?); \
                 it isn't on sqlh's list of read-only procedures"
            )
        );
        assert_eq!(
            blocked(&format!("DBCC {long}")).reason,
            format!(
                "DBCC {} isn't on sqlh's list of read-only DBCC commands",
                long.to_ascii_uppercase()
            )
        );
        // The name of a DDL statement is sqlh's own, so a long word after it isn't shown.
        assert_eq!(
            blocked(&format!("ALTER {long}")).reason,
            "ALTER would write"
        );
        assert_eq!(
            blocked(&format!("CREATE {long} ON t")).reason,
            "CREATE would write"
        );
    }

    // The refusals' line numbers are the contract of plan-history.md 2.14's restructuring: the line
    // of the keyword that makes the write, through comments and multi-line strings and names.
    #[test]
    fn refusals_name_the_line_of_their_keyword() {
        for (sql, line) in [
            ("-- c\n\nPurgeIt 1", 3),
            ("/* a\nb */ Purge 1", 2),
            ("SELECT 1\n\n  INSERT dbo.T VALUES (1)", 3),
            ("SELECT 1\nINSERT ##g VALUES (1)", 2),
            ("SELECT 'a\nb'\nSELECT NEXT VALUE FOR s", 3),
            ("SELECT 1\n/* x\n*/ SELECT Send", 3),
            ("SELECT 1\nEXEC dbo.sp_who", 2),
            ("SELECT 1;\nEXEC sp_spaceused N'a',\n N'true'", 2),
            ("SELECT 1\nDBCC FREEPROCCACHE", 2),
            ("SELECT 1\nEXEC (\n'x')", 2),
            ("SELECT 1\nDROP TABLE #a,\n dbo.b", 2),
            ("SELECT [a\nb]\nBEGIN\nTRAN", 3),
            ("SELECT 1\nUPDATE TOP (\n5) dbo.T SET x = 1", 2),
            ("SELECT 1\nCREATE\nINDEX ix ON dbo.T (a)", 2),
            ("SELECT 1\nDELETE\nFROM\ndbo.T", 2),
            ("SELECT 1\nEXEC @p", 2),
            ("SELECT 1\nEXECUTE AS USER = 'x'", 2),
        ] {
            assert_eq!(blocked(sql).line, line, "{sql:?}");
        }
        let b = check(
            "SELECT 1\n/* x */\nSET NOEXEC OFF",
            Policy {
                writes: true,
                compile_only: true,
                reused_session: false,
            },
        )
        .expect_err("refused");
        assert_eq!(b.line, 3);
        // An EXEC is reported on its own line (R18): not on line 1 when nothing follows it,
        // and not on the line of its name when that is further down.
        assert_eq!(blocked("SELECT 1\nEXEC").line, 2);
        assert_eq!(blocked("SELECT 1\n\n-- c\nEXECUTE").line, 4);
        assert_eq!(blocked("SELECT 1\nEXEC\n  dbo.sp_who").line, 2);
        assert_eq!(blocked("SELECT 1\nEXEC\n\n  sp_spaceused N'a', 1").line, 2);
        assert_eq!(
            blocked("SELECT 1\n\nEXECUTE\n\nsp_who2 ; EXEC\nsp_spaceused N'a', 1").line,
            5
        );
        assert_eq!(blocked("SELECT 1\nEXEC\n(\n'x')").line, 2);
    }

    // The refusal for a TOP clause that is not closed yet when a later keyword refuses, and the
    // nested clauses among themselves, come out as when each clause's target was looked up at
    // its keyword: the earliest keyword's. (The expected values are what the guard of `3ef8f3c`,
    // which scanned ahead from every keyword, gave for these inputs.)
    #[test]
    fn open_top_clauses_refuse_in_the_order_of_their_keywords() {
        for (sql, line, reason) in [
            (
                "SELECT 1\nUPDATE TOP (\nDELETE FROM dbo.Z) dbo.T SET x = 1",
                2,
                "UPDATE dbo.T would write",
            ),
            (
                "SELECT 1\nUPDATE TOP (\nDELETE FROM dbo.Z) @t SET x = 1",
                3,
                "DELETE dbo.Z would write",
            ),
            (
                "UPDATE TOP (DELETE TOP (INSERT INTO dbo.A) dbo.B) dbo.C SET x = 1",
                1,
                "UPDATE dbo.C would write",
            ),
            (
                "UPDATE TOP (DELETE TOP (INSERT INTO dbo.A) dbo.B) @t SET x = 1",
                1,
                "DELETE dbo.B would write",
            ),
            (
                "UPDATE TOP (DELETE TOP (INSERT INTO #a) dbo.B) @t SET x = 1",
                1,
                "DELETE dbo.B would write",
            ),
            (
                "UPDATE TOP (DELETE TOP (INSERT INTO #a) @b) dbo.C SET x = 1",
                1,
                "UPDATE dbo.C would write",
            ),
            (
                "UPDATE TOP (1) @t SET x = 1;\nDELETE TOP (2) dbo.Q",
                2,
                "DELETE dbo.Q would write",
            ),
            (
                "UPDATE TOP (1) dbo.T SET x = 1;\nDELETE TOP (2) dbo.Q",
                1,
                "UPDATE dbo.T would write",
            ),
            (
                "INSERT TOP (1) INTO #t SELECT 1;\nINSERT TOP (SELECT 1) dbo.Q SELECT 1",
                2,
                "INSERT dbo.Q would write",
            ),
            (
                "DELETE TOP (1) FROM @t; DELETE TOP (1) FROM dbo.U",
                1,
                "DELETE dbo.U would write",
            ),
            (
                "DELETE TOP ((1) + (2)) FROM ##g",
                1,
                "DELETE ##g would write to a global temp table, which other sessions share",
            ),
            // Never closed, or closed once too often: no target.
            ("UPDATE TOP (1", 1, "UPDATE ? would write"),
            ("UPDATE TOP (UPDATE TOP (1) @t", 1, "UPDATE ? would write"),
            ("UPDATE TOP (1) ) @t", 1, "UPDATE ? would write"),
            (
                "UPDATE TOP (UPDATE TOP (1) @t) @t SET x = 1 )))) DELETE dbo.Q",
                1,
                "DELETE dbo.Q would write",
            ),
        ] {
            let b = blocked(sql);
            assert_eq!((b.line, b.reason.as_str()), (line, reason), "{sql:?}");
        }
        allowed("UPDATE TOP (UPDATE TOP (1) @t) @t SET x = 1");
        allowed("INSERT TOP (5) INTO #t SELECT 1");
        allowed("DELETE TOP (SELECT TOP (1) 1) FROM @t");
    }

    /// The work a write-classifier pass over `sql` takes, in the units of `Work`.
    fn work_of(sql: &str) -> u64 {
        let work = Work::new(sql, WORK_BASE, WORK_PER_BYTE);
        let _ = Guard {
            sql,
            work: &work,
            compile_only: false,
            reused_session: false,
        }
        .check_writes();
        work.used.get()
    }

    // R18: the shapes whose lookahead used to read the same suffix from every keyword now take
    // one look at each token, however deep they nest, and an input they can't be read in one
    // look from is refused.
    #[test]
    fn nested_top_clauses_are_read_in_one_pass() {
        // The reviewer's input, at the depths of its measurements.
        for n in [2_000, 4_000, 8_000, 16_000, MAX_OPEN_TOP] {
            let sql = format!("{}@t{}", "UPDATE TOP (".repeat(n), ") @t ".repeat(n));
            allowed(&sql);
            // About 10 reads a level; the old scan read n^2 of them.
            assert!(work_of(&sql) < 12 * n as u64, "{n}: {}", work_of(&sql));
            // A refusal at the end of it is found and named with its own line.
            let b = blocked(&format!("{sql}\nDELETE FROM dbo.T"));
            assert_eq!((b.line, b.reason.as_str()), (2, "DELETE dbo.T would write"));
        }
        // One level deeper than the classifier remembers is refused, whatever follows.
        let n = MAX_OPEN_TOP + 1;
        let sql = format!("{}@t{}", "UPDATE TOP (".repeat(n), ") @t ".repeat(n));
        let b = blocked(&sql);
        assert_eq!(b.line, 1);
        assert_eq!(
            b.reason,
            format!(
                "this input is too complex for the read-only check: TOP ( clauses are nested \
                 more than {MAX_OPEN_TOP} deep"
            )
        );
        assert!(b.advice.contains("split it into smaller batches"));
        assert!(b.advice.contains("--allow-write"));
        // The writes the clauses before it make are still checked first.
        let b = blocked(&format!("UPDATE TOP (1) dbo.T SET x = 1\n{sql}"));
        assert_eq!((b.line, b.reason.as_str()), (1, "UPDATE dbo.T would write"));
        // Deep parentheses inside one clause are not clauses.
        allowed(&format!(
            "UPDATE TOP ({}1{}) #t SET x = 1",
            "(".repeat(50_000),
            ")".repeat(50_000)
        ));
        // A clause that is never closed reads to the end once, however many follow.
        let b = blocked(&"DELETE TOP (".repeat(10_000));
        assert_eq!(b.reason, "DELETE ? would write");
        // --allow-write skips the classifier, so no limit of it applies.
        let writes = Policy {
            writes: true,
            compile_only: false,
            reused_session: false,
        };
        assert_eq!(check(&sql, writes), Ok(()));
    }

    #[test]
    fn sp_spaceused_arguments_are_scanned_a_bounded_number_of_times() {
        // Parentheses in the arguments of a call that takes none: a few are read, many are not.
        allowed("EXEC sp_spaceused (N'a')");
        let half = MAX_ARG_PARENS / 2;
        let (open, close) = ("(".repeat(half), ")".repeat(half));
        allowed(&format!("EXEC sp_spaceused {open}{close}; SELECT 1"));
        let parens = "(".repeat(MAX_ARG_PARENS);
        let b = blocked(&format!("SELECT 1\nEXEC sp_spaceused {parens}("));
        assert_eq!(b.line, 2);
        assert_eq!(
            b.reason,
            format!(
                "this input is too complex for the read-only check: the arguments of \
                 sp_spaceused have more than {MAX_ARG_PARENS} parentheses"
            )
        );
        assert!(b.advice.contains("--allow-write"));
        // What the arguments say before the limit still counts.
        assert!(
            blocked(&format!("EXEC sp_spaceused N'a', 1 {parens}("))
                .reason
                .contains("UPDATEUSAGE")
        );
        // A call after a stray `)` is the shape that kept every scan going: `) (` before each
        // call holds one parenthesis open at all of them. Each scan stops at its ninth.
        for unit in [
            "EXEC sp_spaceused ) (",
            "EXEC sp_spaceused (",
            "EXEC sp_spaceused ( ) (",
        ] {
            let sql = unit.repeat(50_000);
            let b = blocked(&sql);
            assert!(b.reason.contains("too complex"), "{unit}: {}", b.reason);
            assert!(work_of(&sql) < 1_000, "{unit}: {}", work_of(&sql));
        }
        // Calls that do nest a few parentheses are read about as often as they nest.
        for k in 1..=MAX_ARG_PARENS / 2 {
            let unit = format!("{}{}", "EXEC sp_spaceused (".repeat(k), ")".repeat(k));
            let sql = unit.repeat(5_000);
            allowed(&sql);
            assert!(
                work_of(&sql) < 3 * sql.len() as u64 / 2,
                "{k}: {}",
                work_of(&sql)
            );
        }
    }

    // R18: the work budget, the backstop under all of the above. The real one is too large to
    // reach with any input this size, so these use a small one.
    #[test]
    fn input_over_its_work_budget_is_refused() {
        let policy = READ_ONLY;
        let sql = "SELECT 1\n".repeat(1_000);
        // 1,000 lines of 2 tokens: a pass reads about 2,300 units' worth.
        assert_eq!(check_within(&sql, policy, 10_000, 0), Ok(()));
        let b = check_within(&sql, policy, 1_000, 0).expect_err("over the budget");
        assert!(
            b.reason.starts_with(
                "this input is too complex for the read-only check: it would take more than 1000 \
                 token reads"
            ),
            "{}",
            b.reason
        );
        assert!(b.advice.contains("split it into smaller batches with GO"));
        assert!(b.advice.contains("--allow-write"));
        // It names the line it ran out on: about half way.
        assert!((400..=600).contains(&b.line), "{}", b.line);
        // The budget grows with the text.
        assert_eq!(check_within(&sql, policy, 0, 1), Ok(()));
        assert!(check_within(&sql, policy, 0, 0).is_err());
        // A refusal found within the budget stands.
        let sql = format!("DELETE FROM dbo.T\n{}", "SELECT 1\n".repeat(100_000));
        let b = check_within(&sql, policy, 100, 0).expect_err("refused");
        assert_eq!((b.line, b.reason.as_str()), (1, "DELETE dbo.T would write"));
        // Spent before the refusal it would have found: the answer is the budget's, not a
        // verdict on the truncated text.
        let sql = format!("{}\nDELETE FROM dbo.T", "SELECT 1\n".repeat(100_000));
        let b = check_within(&sql, policy, 100, 0).expect_err("refused");
        assert!(b.reason.contains("too complex"), "{}", b.reason);
        // Nothing is read when no check applies.
        let open = Policy {
            writes: true,
            compile_only: false,
            reused_session: false,
        };
        assert_eq!(check_within(&sql, open, 0, 0), Ok(()));
        // --check's NOEXEC search has a budget of its own, and the write classifier another.
        let both = Policy {
            writes: false,
            compile_only: true,
            reused_session: false,
        };
        let sql = "SELECT 1\n".repeat(1_000);
        assert_eq!(check_within(&sql, both, 3_000, 0), Ok(()));
        let b = check_within(&sql, both, 1_000, 0).expect_err("over the budget");
        assert!(b.reason.contains("too complex"), "{}", b.reason);
    }

    #[test]
    fn a_repeated_lookahead_is_charged_each_time() {
        // Charged by what is read, lookahead included: the same text read twice costs twice.
        let work = Work::new("a b c d", 100, 0);
        let lex = Lexer::metered("a b c d", &work);
        assert_eq!(work.used.get(), 0);
        assert!(lex.ahead(2).is_some());
        assert_eq!(work.used.get(), 3);
        assert!(lex.ahead(2).is_some());
        assert_eq!(work.used.get(), 6);
        assert!(lex.after(4).ahead(0).is_none());
        assert_eq!(work.used.get(), 6 + 4 + 1);
        // Big tokens cost one more unit for each 32 bytes they, and what precedes them, take.
        let text = format!("x '{}' /* {} */ y", "s".repeat(100), "c".repeat(100));
        let work = Work::new(&text, 1_000, 0);
        let all: Vec<_> = Lexer::metered(&text, &work).collect();
        assert_eq!(all.len(), 3);
        assert!(
            work.used.get() >= 4 + 100 / 32 + 100 / 32,
            "{}",
            work.used.get()
        );
        assert!(!work.exceeded());
        // Once the limit is passed the lexer, and its copies, report the end of the input,
        // and the line it passed it on is kept.
        let text = "a\nb\nc\nd\ne\nf\n";
        let work = Work::new(text, 3, 0);
        let mut lex = Lexer::metered(text, &work);
        let copy = lex.clone();
        assert_eq!(lex.by_ref().count(), 4);
        assert!(work.exceeded());
        assert_eq!(work.line.get(), 4);
        assert!(copy.ahead(0).is_none());
        // An unmetered lexer is never cut off.
        assert_eq!(Lexer::new(text).count(), 6);
    }

    // A refusal never depends on how much comes after it, and a pass takes one look at each
    // token: inputs with hundreds of thousands of tokens in the shapes that used to be the
    // expensive ones (a name, a TOP clause, a DROP list).
    #[test]
    fn inputs_with_many_tokens_are_checked_in_one_pass() {
        let n = 200_000;
        allowed(&"SELECT 1\n".repeat(n));
        allowed(&"(".repeat(n));
        allowed(&format!("DROP TABLE {}#a", "#a, ".repeat(n)));
        allowed(&format!(
            "UPDATE TOP ({}1{}) #t SET x = 1",
            "(".repeat(n),
            ")".repeat(n)
        ));
        let b = blocked(&format!("{}\nDELETE FROM dbo.T", "(".repeat(n)));
        assert_eq!(b.line, 2);
        let b = blocked(&format!("{}\nDROP TABLE #a, dbo.T", ",".repeat(n)));
        assert_eq!(b.reason, "DROP TABLE dbo.T would write");
        assert_eq!(b.line, 2);
        let b = blocked(&format!("EXEC {}a", "a.".repeat(n)));
        assert!(b.reason.starts_with("EXEC a.a.a.a."), "{}", &b.reason[..20]);
        let unknown =
            " isn't on sqlh's list of read-only procedures, so sqlh can't tell what it does";
        assert!(b.reason.ends_with(&format!(".a.a{unknown}")));
        assert_eq!(b.reason.len(), "EXEC ".len() + 2 * n + 1 + unknown.len());
        // A TOP clause that is never closed reads to the end and finds no target.
        let b = blocked(&format!("DELETE TOP ({}", "(".repeat(n)));
        assert_eq!(b.reason, "DELETE ? would write");
        // sp_spaceused's arguments are read up to the end, or to the next statement.
        let args = ", NULL".repeat(n);
        assert!(
            blocked(&format!("EXEC sp_spaceused N'a'{args}"))
                .reason
                .contains("UPDATEUSAGE")
        );
        allowed(&format!("EXEC sp_spaceused N'a'; SELECT 1{args}"));
    }

    fn tokens(sql: &str) -> Vec<&str> {
        Lexer::new(sql).map(|t| t.text).collect()
    }

    fn spaces() -> Vec<char> {
        let beyond_ascii = [
            ('\u{85}', '\u{85}'),
            ('\u{A0}', '\u{A0}'),
            ('\u{1680}', '\u{1680}'),
            ('\u{2000}', '\u{200B}'),
            ('\u{2028}', '\u{2029}'),
            ('\u{202F}', '\u{202F}'),
            ('\u{205F}', '\u{205F}'),
            ('\u{3000}', '\u{3000}'),
        ];
        ('\0'..=' ')
            .chain(beyond_ascii.iter().flat_map(|&(from, to)| from..=to))
            .collect()
    }

    // The sets as `sys.dm_exec_describe_first_result_set` gave them for every code point on SQL
    // Server 2016 SP3 and 2025; the live test `the_servers_read_the_guards_characters_as_it_does`
    // checks the same lists against a real server.
    #[test]
    fn the_guards_spaces_and_currency_signs_are_exactly_the_servers() {
        let spaces = spaces();
        let currency = [
            ('\u{A2}', '\u{A5}'),
            ('\u{9F2}', '\u{9F3}'),
            ('\u{E3F}', '\u{E3F}'),
            ('\u{17DB}', '\u{17DB}'),
            ('\u{20A0}', '\u{20B1}'),
            ('\u{FDFC}', '\u{FDFC}'),
            ('\u{FE69}', '\u{FE69}'),
            ('\u{FF04}', '\u{FF04}'),
            ('\u{FFE0}', '\u{FFE1}'),
            ('\u{FFE5}', '\u{FFE6}'),
        ];
        for c in '\u{80}'..=char::MAX {
            assert_eq!(is_space(c), spaces.contains(&c), "U+{:04X}", c as u32);
            assert_eq!(
                is_currency(c),
                currency.iter().any(|&(from, to)| (from..=to).contains(&c)),
                "U+{:04X}",
                c as u32
            );
        }
    }

    #[test]
    fn tokens_end_where_the_servers_end_them() {
        let cases: &[(&str, &[&str])] = &[
            ("1DELETE t", &["1", "DELETE", "t"]),
            ("1.DELETE", &["1.", "DELETE"]),
            ("1.5DELETE", &["1.5", "DELETE"]),
            ("1.2.3DELETE", &["1.2", ".", "3", "DELETE"]),
            (".5DELETE", &[".", "5", "DELETE"]),
            ("1eDELETE", &["1e", "DELETE"]),
            ("1e5DELETE", &["1e5", "DELETE"]),
            ("1E+5DELETE", &["1E+5", "DELETE"]),
            ("1e-5DELETE", &["1e-5", "DELETE"]),
            ("1.5e5DELETE", &["1.5e5", "DELETE"]),
            ("1e+DELETE", &["1e", "+", "DELETE"]),
            ("1,000DELETE", &["1", ",", "000", "DELETE"]),
            ("0xINSERT", &["0x", "INSERT"]),
            ("0XINSERT", &["0X", "INSERT"]),
            ("0xDELETE", &["0xDE", "LETE"]),
            ("0x1FKILL", &["0x1F", "KILL"]),
            ("1N'a'", &["1", "N'a'"]),
            // A money literal takes no exponent: `$1eDELETE` is `$1` with an alias.
            ("$1DELETE", &["$1", "DELETE"]),
            ("$1.5DELETE", &["$1.5", "DELETE"]),
            ("$.5DELETE", &["$.5", "DELETE"]),
            ("$1,000DELETE", &["$1", ",", "000", "DELETE"]),
            ("$-1DELETE", &["$-1", "DELETE"]),
            ("$+.5EXEC", &["$+.5", "EXEC"]),
            ("$ 1EXEC", &["$ 1", "EXEC"]),
            ("$  -1.5EXEC", &["$  -1.5", "EXEC"]),
            ("$ .5EXEC", &["$ .5", "EXEC"]),
            ("$+ 1EXEC", &["$", "+", "1E", "XEC"]),
            ("$\t1EXEC", &["$", "1E", "XEC"]),
            ("$ a", &["$", "a"]),
            ("$ EXEC", &["$", "EXEC"]),
            ("$,1", &["$", ",", "1"]),
            ("1EXEC", &["1E", "XEC"]),
            ("1e5EXEC", &["1e5", "EXEC"]),
            ("$1eDELETE", &["$1", "eDELETE"]),
            ("£1DELETE", &["£1", "DELETE"]),
            ("£.5DELETE", &["£.5", "DELETE"]),
            ("£DELETE", &["£", "DELETE"]),
            ("£1EXEC", &["£1", "EXEC"]),
            ("£ -1EXEC", &["£ -1", "EXEC"]),
            ("£e5", &["£", "e5"]),
            ("€1 ¥2", &["€1", "¥2"]),
            ("a£DELETE", &["a", "£", "DELETE"]),
            ("\\1EXEC", &["\\1", "EXEC"]),
            ("\\ -1EXEC", &["\\ -1", "EXEC"]),
            ("\\DELETE", &["\\", "DELETE"]),
            // Not a number: the pseudocolumns, and a name that goes on.
            ("$action $DELETE", &["$action", "$DELETE"]),
            ("a$1DELETE a1DELETE", &["a$1DELETE", "a1DELETE"]),
            ("@a1DELETE #b1DELETE", &["@a1DELETE", "#b1DELETE"]),
            // Beyond ASCII: a name goes on, and a server's space ends it.
            ("é1è ñ", &["é1è", "ñ"]),
            ("a\u{200D}b", &["a\u{200D}b"]),
            ("a\u{FEFF}b", &["a\u{FEFF}b"]),
            ("a\u{A0}b", &["a", "b"]),
            ("\u{3000}a\u{200B}b\u{85}", &["a", "b"]),
            ("@a\u{A0}#b\u{2003}c", &["@a", "#b", "c"]),
            // A comment ends at LF or at a lone CR, and at nothing else.
            ("a --b\nc", &["a", "c"]),
            ("a --b\rc", &["a", "c"]),
            ("a --b\r\nc", &["a", "c"]),
            ("a --b\u{2028}c\u{85}d", &["a"]),
            ("a /* b\rc */ d", &["a", "d"]),
            ("'a--b\rc'd", &["'a--b\rc'", "d"]),
        ];
        for (sql, expected) in cases {
            assert_eq!(tokens(sql), *expected, "{sql:?}");
        }
    }

    // The servers don't start a new line at a lone CR (`SELECT 1/0` after two of them is their
    // line 1), so a refusal's line doesn't either.
    #[test]
    fn a_lone_cr_ends_a_comment_but_starts_no_line() {
        assert_eq!(blocked("SELECT 1 --c\rDELETE t").line, 1);
        assert_eq!(blocked("SELECT 1\r\rDELETE t").line, 1);
        assert_eq!(blocked("SELECT 1 --c\r\nDELETE t").line, 2);
        assert_eq!(blocked("SELECT 1 --c\nDELETE t").line, 2);
    }

    // What these spellings hid from every pass: each was run on the servers, and reached them.
    #[test]
    fn nothing_is_hidden_by_a_comment_a_number_or_a_space() {
        let statements = [
            ("COMMIT", "COMMIT is not allowed"),
            ("KILL 99999", "KILL is not allowed"),
            ("WAITFOR DELAY '00:00:01'", "WAITFOR is not allowed"),
            ("DELETE dbo.T", "DELETE dbo.T would"),
            ("INSERT t VALUES (1)", "INSERT t would"),
            ("UPDATE t SET a = 1", "UPDATE t would"),
            ("DROP TABLE t", "DROP TABLE t would"),
            ("EXEC sp_no_such_proc", "EXEC sp_no_such_proc isn't on"),
            ("DBCC NOSUCHCOMMAND", "DBCC NOSUCHCOMMAND isn't on"),
        ];
        let mut before: Vec<String> = [
            "SELECT 1 --c\r",
            "SELECT 1 --c\r\r",
            "SELECT 1e",
            "SELECT 1e5",
            "SELECT 1e+5",
            "SELECT $1",
            "SELECT $.5",
            "SELECT $ 1",
            "SELECT $-1",
            "SELECT $ +1.5",
            "SELECT £1",
            "SELECT £ 1",
            "SELECT £",
            "SELECT \\1",
            "SELECT \\ 1",
            "SELECT \\",
        ]
        .map(str::to_owned)
        .to_vec();
        for space in spaces() {
            before.push(format!("SELECT 1 {space}"));
            before.push(format!("SELECT 1{space}"));
            before.push(format!("SELECT 1 AS a{space}"));
            before.push(format!("DECLARE @x int SELECT @x{space}"));
        }
        for start in &before {
            for (statement, reason) in statements {
                let sql = format!("{start}{statement}");
                let b = blocked(&sql);
                assert!(b.reason.starts_with(reason), "{sql:?}: {}", b.reason);
            }
        }
        // A plain number takes the `E` of `EXEC` for its exponent (the servers read `1E` and an
        // alias `XEC`, and a name after that is their syntax error), and hex what is hex.
        for (start, keyword) in [
            ("SELECT 1", "DELETE t"),
            ("SELECT 1", "COMMIT"),
            ("SELECT 1.", "DELETE t"),
            ("SELECT .5", "DELETE t"),
            ("SELECT 1.5", "WAITFOR DELAY '00:00:01'"),
            ("SELECT 0x", "INSERT t VALUES (1)"),
            ("SELECT 0x", "KILL 1"),
            ("SELECT 0X1F", "KILL 1"),
        ] {
            blocked(&format!("{start}{keyword}"));
        }
        // Between the words of a statement as well.
        for space in spaces() {
            let b = blocked(&format!("DELETE{space}dbo.T"));
            assert_eq!(b.reason, "DELETE dbo.T would write", "{space:?}");
            let b = blocked(&format!("DROP{space}TABLE{space}dbo.T"));
            assert_eq!(b.reason, "DROP TABLE dbo.T would write", "{space:?}");
            allowed(&format!("DROP{space}TABLE{space}#t"));
        }
        // And at a batch's start, where a word is a procedure's name.
        allowed("\u{A0}SELECT 1");
        allowed("--c\rSELECT 1");
        allowed("\u{0}SELECT\u{B}1");
        assert!(
            blocked("\u{A0}sp_x")
                .reason
                .starts_with("sp_x starts the batch")
        );
    }

    #[test]
    fn noexec_under_check_is_seen_past_a_comment_a_number_or_a_space() {
        let compile_only = Policy {
            writes: true,
            compile_only: true,
            reused_session: false,
        };
        for sql in [
            "SELECT 1 --c\rSET NOEXEC OFF SELECT 2",
            "SET NOEXEC\u{A0}OFF SELECT 1",
            "SET\u{A0}NOEXEC OFF SELECT 1",
            "SET\u{B}NOEXEC\u{B}OFF SELECT 1",
            "SELECT 1NOEXEC",
        ] {
            let b = check(sql, compile_only).expect_err(sql);
            assert!(b.reason.starts_with("SET NOEXEC can't"), "{sql:?}");
        }
    }

    // A read that only looks like the shapes above still passes: a false refusal is a defect too.
    #[test]
    fn numbers_and_spaces_keep_what_was_legal() {
        for sql in [
            "SELECT 1e5, 1E+5, 1.5e-5, 0xFF, 0XAB, 0x, $1.50, $.5, £1, 1., .5",
            "SELECT 0xDELETE FROM t",
            "SELECT $1eDELETE",
            "SELECT 1EXEC",
            "SELECT $ a, £ b, \\ c, $ 1 AS d",
            "SELECT a$DELETE, b1DELETE, _DELETE FROM t",
            "SELECT 1 AS é, [a\u{A0}DELETE b], 'c\u{A0}DELETE d' FROM t",
            "SELECT $action, $partition.f(1)",
            "SELECT TOP 1PERCENT a FROM t WHERE a=1AND b=2OR c=3",
            "SELECT a FROM t\r\nWHERE b = 1 -- DELETE x\r\nAND c = 2",
            "SELECT\u{A0}1\u{3000}AS\u{2003}x",
            "CREATE TABLE #t(a int)\u{A0}INSERT\u{A0}#t\u{A0}VALUES(1)\u{85}DELETE\u{200B}#t",
        ] {
            allowed(sql);
        }
    }

    #[test]
    fn the_cost_and_session_rules_read_past_every_space() {
        const F: &str = "dm_db_index_physical_stats";
        for space in spaces() {
            for sql in [
                format!("SELECT * FROM sys{space}.{F}(NULL,NULL,NULL,NULL,'DETAILED')"),
                format!("SELECT * FROM sys.{space}{F}(NULL,NULL,NULL,NULL,'DETAILED')"),
                format!("SELECT * FROM sys.{F}{space}(NULL,NULL,NULL,NULL,'DETAILED')"),
                format!("SELECT * FROM sys.{F}({space}NULL,NULL,NULL,NULL,'DETAILED')"),
                format!("SELECT * FROM sys.{F}(NULL{space},NULL,NULL,NULL,'DETAILED')"),
                format!("SELECT * FROM sys.{F}(NULL,NULL,NULL,NULL,{space}'DETAILED')"),
                format!("SELECT * FROM sys.{F}(NULL,NULL,NULL,NULL,'DETAILED'{space})"),
                format!("SELECT{space}*{space}FROM sys.{F}(NULL,NULL,NULL,NULL,'DETAILED')"),
            ] {
                let b = blocked(&sql);
                assert!(b.reason.ends_with("in every database"), "{sql:?}");
            }
            for (sql, what) in [
                (format!("SET{space}AUTOPILOT ON"), "SET AUTOPILOT"),
                (format!("SET AUTOPILOT{space}ON"), "SET AUTOPILOT"),
                (
                    format!("SET{space}IDENTITY_INSERT #t ON"),
                    "SET IDENTITY_INSERT",
                ),
                (format!("OPEN{space}SYMMETRIC KEY k"), "OPEN SYMMETRIC KEY"),
                (format!("OPEN SYMMETRIC{space}KEY k"), "OPEN SYMMETRIC KEY"),
                (format!("OPEN{space}MASTER{space}KEY"), "OPEN MASTER KEY"),
            ] {
                let b = check(&sql, MCP).expect_err(&sql);
                assert_eq!(b.reason, state_reason(what), "{sql:?}");
            }
        }
        for sql in [
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL, --x\rNULL,NULL,NULL,'DETAILED')",
            "SELECT * --x\rFROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED')",
        ] {
            let b = blocked(sql);
            assert!(b.reason.ends_with("in every database"), "{sql:?}");
        }
        for sql in [
            "SELECT 1 --c\rSET AUTOPILOT ON",
            "SELECT 1SET AUTOPILOT ON",
            "SELECT 1\u{A0}SET AUTOPILOT ON",
        ] {
            let b = check(sql, MCP).expect_err(sql);
            assert_eq!(b.reason, state_reason("SET AUTOPILOT"), "{sql:?}");
        }
    }

    // The servers compare a name, this mode and every word they don't reserve by a collation, which
    // ignores or folds these. All of them were run: each took the name, the mode or the statement.
    #[test]
    fn what_a_collation_ignores_is_seen_where_a_rule_goes_by_a_name() {
        for sql in [
            "SELECT * FROM sys.dm_db_index_physical_stats\u{200D}(NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM sys.[dm_db_index_physical_stats\u{200D}](NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM [sys\u{200D}].dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM sys\u{200D}.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM \u{FF53}ys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,N'DE\u{200D}TAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,N'DETAILED\u{FEFF}')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED\u{0}')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,N'DETAILED\u{0}zz')",
        ] {
            let b = blocked(sql);
            assert!(b.reason.contains("'DETAILED'"), "{sql:?}: {}", b.reason);
        }
        let b = blocked(
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,N'SAMP\u{200D}LED')",
        );
        assert!(b.reason.contains("'SAMPLED'"), "{}", b.reason);

        for (sql, what) in [
            ("SET AUTOPILOT\u{200D} ON", "SET AUTOPILOT"),
            ("SET AUTO\u{200D}PILOT ON", "SET AUTOPILOT"),
            ("OPEN SYMMETRIC\u{200D} KEY k", "OPEN SYMMETRIC KEY"),
            ("OPEN SYMMETRIC KEY\u{FEFF} k", "OPEN SYMMETRIC KEY"),
            ("OPEN MASTER\u{200D} KEY", "OPEN MASTER KEY"),
        ] {
            let b = check(sql, MCP).expect_err(sql);
            assert_eq!(b.reason, state_reason(what), "{sql:?}");
            assert_eq!(check(sql, READ_ONLY), Ok(()), "{sql:?}");
        }

        let compile_only = Policy {
            writes: true,
            compile_only: true,
            reused_session: false,
        };
        for sql in [
            "SET NOEXEC\u{200D} OFF SELECT 1",
            "SET NO\u{200D}EXEC OFF SELECT 1",
            "SET \u{FF2E}\u{FF2F}\u{FF25}\u{FF38}\u{FF25}\u{FF23} OFF SELECT 1",
        ] {
            let b = check(sql, compile_only).expect_err(sql);
            assert!(b.reason.starts_with("SET NOEXEC can't"), "{sql:?}");
        }

        // Words the servers don't reserve, each of which they took as the statement.
        for (sql, reason) in [
            ("SELECT 1 SEND\u{200D} ON CONVERSATION @x", "SEND is not"),
            (
                "SELECT NEXT\u{200D} VALUE FOR s",
                "NEXT VALUE FOR s advances",
            ),
            (
                "SELECT NEXT VALUE\u{200D} FOR s",
                "NEXT VALUE FOR s advances",
            ),
            ("SELECT 1 BEGIN DIALOG\u{200D} @x", "BEGIN DIALOG would"),
        ] {
            let b = blocked(sql);
            assert!(b.reason.starts_with(reason), "{sql:?}: {}", b.reason);
        }
    }

    // A false refusal is a defect, and folding must take none away either: only what a match
    // refuses goes by the fold. A batch that starts with a word the fold reads as a statement's is
    // still a procedure call to the server, so it stays refused as one.
    #[test]
    fn the_fold_leaves_real_names_alone_and_weakens_nothing() {
        for sql in [
            "SELECT UPDATE\u{65F6}\u{95F4}, DELETE\u{00E9}, \u{00E9}COMMIT FROM t",
            "SELECT * FROM sys\u{00E9}.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAILED')",
            "SELECT * FROM sys.dm_db_index_physical_stats(NULL,NULL,NULL,NULL,'DETAIL\u{00E9}D')",
        ] {
            allowed(sql);
        }
        for (first, shown) in [("SELECT", "SELECT\u{200D}"), ("PRINT", "PRINT\u{200D}")] {
            let sql = format!("{first}\u{200D} 1");
            let b = blocked(&sql);
            assert!(
                b.reason.starts_with(&format!("{shown} starts the batch")),
                "{sql:?}: {}",
                b.reason
            );
        }
        // The list goes by exact spelling, so a name the fold would read as listed isn't listed.
        assert!(
            blocked("EXEC sp_who\u{200D}")
                .reason
                .contains("isn't on sqlh's list")
        );
        // Nor does a word the fold reads as a statement's end the argument check's scan.
        blocked("EXEC sp_configure SELECT\u{200D}, 1");
        blocked("EXEC sp_spaceused N'a', SELECT\u{200D}");
        // Nor does a scratch target count where the word before it only folds to the keyword.
        blocked("ALTER TABLE\u{200D} #t ADD a int");
        blocked("DROP TABLE\u{200D} #t");
    }

    #[test]
    fn spelling_goes_by_the_fold() {
        for (name, keyword, expected) in [
            ("NOEXEC", "NOEXEC", true),
            ("noexec", "NOEXEC", true),
            ("NOEXEC  ", "NOEXEC", true),
            ("NOEXEC\u{200D}", "NOEXEC", true),
            ("\u{FEFF}NO\u{200D}EXEC\u{200C} ", "NOEXEC", true),
            ("\u{FF2E}\u{FF4F}EXEC", "NOEXEC", true),
            ("NOEXE", "NOEXEC", false),
            ("NOEXECS", "NOEXEC", false),
            ("NOEXEC\u{00E9}", "NOEXEC", false),
            ("\u{65F6}NOEXEC", "NOEXEC", false),
            ("", "NOEXEC", false),
            ("\u{200D}", "NOEXEC", false),
        ] {
            assert_eq!(spells(name, keyword), expected, "{name:?}");
        }
        let mut buf = [0; KEYWORD_MAX];
        assert_eq!(keyword("delete", &mut buf), "DELETE");
        assert_eq!(keyword("del\u{200D}ete", &mut buf), "DELETE");
        assert_eq!(keyword("\u{FF44}elete", &mut buf), "DELETE");
        assert_eq!(keyword("DELETE\u{65F6}", &mut buf), "");
        assert_eq!(keyword("OPENDATASOURCEXYZ", &mut buf), "");
        assert_eq!(keyword(&"a\u{200D}".repeat(17), &mut buf), "");
    }

    // sp_spaceused's and sp_configure's parameters are found by collation too, and there sqlh does
    // refuse: `@updateusage` with a joiner in it got the parameter's own error 15143 from the
    // server. Neither procedure has a parameter whose name needs a character beyond ASCII.
    #[test]
    fn an_argument_check_reads_through_what_a_collation_ignores() {
        for (sql, advice) in [
            (
                "EXEC sp_spaceused @objname = 'a', @updateusage\u{200D} = 'true'",
                "Leave @updateusage out",
            ),
            (
                "EXEC sp_spaceused @\u{FF55}pdateusage = 'true'",
                "Leave @updateusage out",
            ),
            (
                "EXEC sp_spaceused @updateusage\u{FEFF}='true'",
                "Leave @updateusage out",
            ),
            (
                "EXEC sp_configure 'a', @configvalue\u{200D} = 1",
                "Read sys.configurations",
            ),
            (
                "sp_configure @configname = 'a', @\u{FF43}onfigvalue = 1",
                "Read sys.configurations",
            ),
        ] {
            let b = blocked(sql);
            assert!(b.advice.starts_with(advice), "{sql:?}: {}", b.advice);
        }
        allowed("EXEC sp_spaceused @objname = N'é'");
        allowed("EXEC sp_configure @configname = N'é'");
    }

    #[test]
    fn under_check_the_advice_does_not_promise_a_run() {
        let compile_only = Policy {
            writes: false,
            compile_only: true,
            reused_session: false,
        };
        for (sql, advice) in [
            (
                "EXEC sp_WhoIsActive",
                "Rerun with --allow-write to compile it as written",
            ),
            (
                "DBCC FREEPROCCACHE",
                "Rerun with --allow-write to compile it as written",
            ),
            (
                "sp_WhoIsActive",
                "Fix it if it's a typo, or rerun with --allow-write to compile it as written",
            ),
            (
                "EXEC dbo.sp_who",
                "Call it as sys.sp_who, or rerun with --allow-write to compile it as written",
            ),
            // These never said it would run.
            (
                "DBCC LOGINFO",
                "Read sys.dm_db_log_info instead, or rerun with --allow-write",
            ),
            ("DELETE dbo.T", WRITE_ADVICE),
        ] {
            let b = check(sql, compile_only).expect_err(sql);
            assert_eq!(b.advice, advice, "{sql}");
            // Where the flag doesn't work, none of this shows.
            let off = b.advice_for(Some(&WritesOff::Mcp));
            assert!(!off.contains("--allow-write"), "{sql}: {off}");
        }
        assert_eq!(
            blocked("EXEC sp_WhoIsActive").advice,
            "Rerun with --allow-write to run it as written"
        );
    }

    #[test]
    fn a_from_inside_an_expression_opens_no_table_list() {
        let none = UncommittedReads::default();
        let nolock = UncommittedReads {
            nolock: true,
            ..none
        };
        for (sql, expected) in [
            ("SELECT TRIM(CHAR(32) FROM UPPER(nolock)) FROM #t", none),
            ("SELECT trim(N' ' FROM LTRIM(readuncommitted)) FROM t", none),
            (
                "SELECT TRIM(TRIM(a FROM b) FROM UPPER(nolock)) FROM t",
                none,
            ),
            (
                "SELECT TRIM(a FROM TRIM(b FROM UPPER(nolock))) FROM t",
                none,
            ),
            ("SELECT TRIM(LEADING a FROM UPPER(nolock)) FROM t", none),
            ("SELECT 1 FROM t WHERE a IS DISTINCT FROM ABS(nolock)", none),
            (
                "SELECT 1 FROM t WHERE a IS NOT DISTINCT FROM ABS(nolock)",
                none,
            ),
            // The real ones next to them still count.
            ("SELECT TRIM(a FROM b) FROM t (NOLOCK)", nolock),
            ("SELECT TRIM(a FROM b) FROM t WITH (NOLOCK)", nolock),
            (
                "SELECT TRIM((SELECT TOP 1 c FROM u (NOLOCK)) FROM b) FROM t",
                nolock,
            ),
            (
                "SELECT TRIM(a FROM (SELECT TOP 1 c FROM u (NOLOCK))) FROM t",
                nolock,
            ),
            ("SELECT DISTINCT a FROM t (NOLOCK)", nolock),
            // A `(` at the depth of an earlier `TRIM(` is another parenthesis.
            (
                "SELECT TRIM(a FROM b), (SELECT 1 FROM u (NOLOCK)) FROM t",
                nolock,
            ),
        ] {
            assert_eq!(uncommitted_reads(sql), expected, "{sql}");
        }
    }

    // The spellings of decision 30(a)'s hints that DESIGN.md §4.3 didn't list as limits. The
    // servers ran each, and `#t ([NOLOCKX])` is their error 207, so these are taken as hints.
    #[test]
    fn hints_are_found_in_every_spelling_the_lexer_can_see() {
        let none = UncommittedReads::default();
        let nolock = UncommittedReads {
            nolock: true,
            ..none
        };
        let hint = UncommittedReads {
            readuncommitted: true,
            ..none
        };
        let level = UncommittedReads {
            isolation: true,
            ..none
        };
        for (sql, expected) in [
            ("SELECT * FROM sys.objects ([NOLOCK ]) WHERE 1 = 0", nolock),
            ("SELECT * FROM #t ([NOLOCK  ])", nolock),
            ("SELECT * FROM #t (\"READUNCOMMITTED \")", hint),
            ("SELECT * FROM tempdb..#t (NOLOCK)", nolock),
            ("SELECT * FROM tempdb.dbo.#t x (NOLOCK)", nolock),
            ("SELECT * FROM dbo.#t (READUNCOMMITTED)", hint),
            ("SELECT 1 FROM #t (NOLOCK\u{A0})", nolock),
            ("SELECT 1 FROM #t\u{B}WITH(\u{B}NOLOCK\u{B})", nolock),
            ("SELECT 1 FROM #t (\u{B}NOLOCK)", nolock),
            ("SELECT 1 --x\rFROM #t (NOLOCK)", nolock),
            ("SELECT 1FROM #t (NOLOCK)", nolock),
            (
                "SET\u{A0}TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
                level,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL READ\u{3000}UNCOMMITTED",
                level,
            ),
            ("SELECT * FROM #t (NOLOCK\u{200D})", nolock),
            ("SELECT * FROM #t ([NOLOCK\u{200D}])", nolock),
            ("SELECT * FROM #t WITH (NOLOCK\u{200D})", nolock),
            (
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED\u{200D}",
                level,
            ),
            // Not a hint: a name of its own (error 207).
            ("SELECT * FROM #t ([NOLOCKX])", none),
        ] {
            assert_eq!(uncommitted_reads(sql), expected, "{sql:?}");
        }
    }
}
