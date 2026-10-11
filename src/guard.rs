//! Read-only guard, layer 1 (DESIGN.md §7): a keyword classifier over a T-SQL token
//! stream. It fails closed: any write keyword outside strings, comments and quoted names
//! blocks the batch unless its target is a temp table (#t) or table variable (@t).
//! Layer 2, the rolled-back wrapper transaction, lives in main.rs.
//!
//! The tokens are read as a stream and none is kept (PLAN.md 2.14), so the guard's memory
//! doesn't depend on what the input looks like: see `Guard` and `Lexer`.
//!
//! The guard reduces accidents. It is not a security boundary: database permissions are.

#[derive(Debug, PartialEq, Eq)]
pub struct Blocked {
    /// 1-based line within the batch.
    pub line: u32,
    pub reason: String,
    /// How to get past the refusal; the output puts it after "nothing was run.".
    pub advice: String,
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

/// `name` in ASCII upper case, for a `match` on keywords. A word too long to be one gives "",
/// which no arm matches, so a long word costs nothing.
fn keyword<'b>(name: &str, buf: &'b mut [u8; KEYWORD_MAX]) -> &'b str {
    let Some(room) = buf.get_mut(..name.len()) else {
        return "";
    };
    room.copy_from_slice(name.as_bytes());
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

/// Read-only system procedures that may be executed.
const PROCS: &[&str] = &[
    "SP_COLUMNS",
    "SP_DATABASES",
    "SP_DATATYPE_INFO",
    "SP_DEPENDS",
    "SP_DESCRIBE_FIRST_RESULT_SET",
    "SP_DESCRIBE_UNDECLARED_PARAMETERS",
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
    "SP_SERVER_INFO",
    "SP_SPACEUSED",
    "SP_SPECIAL_COLUMNS",
    "SP_SPROC_COLUMNS",
    "SP_STORED_PROCEDURES",
    "SP_TABLES",
    "SP_WHO",
    "SP_WHO2",
];

/// DBCC commands that only read.
const DBCC_READ: &[&str] = &[
    "INPUTBUFFER",
    "OPENTRAN",
    "SHOW_STATISTICS",
    "TRACESTATUS",
    "USEROPTIONS",
];

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

pub fn check(sql: &str, policy: Policy) -> Result<(), Blocked> {
    if policy.is_open() {
        return Ok(());
    }
    let guard = Guard { sql };
    if policy.compile_only {
        guard.check_compile_only()?;
    }
    if !policy.writes {
        guard.check_writes()?;
    }
    Ok(())
}

/// Whether `sql` has the bare word `XACT_ABORT` among its tokens, which is how `SET XACT_ABORT`
/// is written (a comment, a string or a `[bracketed]` name is not one). Not a guard check: a
/// run whose input says it counts rows exactly (DESIGN.md §4.4), because with XACT_ABORT ON an
/// attention rolls the transaction back, and a counting that stopped early would end with the
/// wrapper transaction gone. Dynamic SQL in a string is not seen; that run is cut like any
/// other.
pub fn mentions_xact_abort(sql: &str) -> bool {
    Lexer::new(sql).any(|t| t.is_word("XACT_ABORT"))
}

/// The classifier reads the batch as a stream of tokens and keeps none of them: where it
/// needs to look ahead it reads on from a copy of its position (a `Lexer`). What it holds
/// therefore doesn't depend on how many tokens the input has (PLAN.md 2.14); only a
/// refusal's message can be as long as the name it quotes.
struct Guard<'a> {
    sql: &'a str,
}

impl<'a> Guard<'a> {
    fn lexer(&self) -> Lexer<'a> {
        Lexer::new(self.sql)
    }

    /// Under `--check`, input must not touch NOEXEC: `SET NOEXEC OFF` would let the rest of
    /// the batch, and every later batch, run for real.
    fn check_compile_only(&self) -> Result<(), Blocked> {
        match self.lexer().find(|t| t.is_word("NOEXEC")) {
            Some(token) => Err(Blocked {
                line: token.line,
                reason: "SET NOEXEC can't be used with --check: switching it off would run the \
                         SQL instead of only compiling it"
                    .into(),
                advice: "Remove SET NOEXEC (--check sets it already); write a column named \
                         NOEXEC as [NOEXEC]. --allow-write doesn't lift this"
                    .into(),
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
                self.check_proc(&start, "a procedure call", true)?;
            }
        }
        // `lex` is always just past `token`, which is where every lookahead starts.
        let mut lex = start;
        while let Some(token) = lex.next() {
            if token.kind != Kind::Word {
                continue;
            }
            let mut buf = [0; KEYWORD_MAX];
            let line = token.line;
            match keyword(token.name, &mut buf) {
                "INSERT" => {
                    let mut target = self.skip_top(&lex);
                    if target.word_is(0, "INTO") {
                        target = target.after(1);
                    }
                    self.require_scratch(line, &target, "INSERT")?;
                }
                "INTO" => self.require_scratch(line, &lex, "INTO")?,
                "UPDATE" => {
                    if lex.word_is(0, "STATISTICS") {
                        return Err(self.blocked(line, "UPDATE STATISTICS"));
                    }
                    self.require_scratch(line, &self.skip_top(&lex), "UPDATE")?;
                }
                "DELETE" => {
                    let mut target = self.skip_top(&lex);
                    if target.word_is(0, "FROM") {
                        target = target.after(1);
                    }
                    self.require_scratch(line, &target, "DELETE")?;
                }
                "TRUNCATE" => self.require_table(line, &lex, "TRUNCATE TABLE")?,
                "ALTER" => self.require_table(line, &lex, "ALTER")?,
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
                    } else {
                        return Err(self.blocked(line, "CREATE"));
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
                        if j.kind(1) != Some(Kind::Punct(',')) {
                            break;
                        }
                        j = j.after(2);
                    }
                }
                "EXEC" | "EXECUTE" => self.check_exec(line, &lex)?,
                "BEGIN" => {
                    let kinds = ["TRAN", "TRANSACTION", "DISTRIBUTED", "DIALOG"];
                    if let Some(w) = lex.word(0).and_then(|w| find_entry(&kinds, w)) {
                        return Err(self.blocked(line, &format!("BEGIN {w}")));
                    }
                }
                "DBCC" => {
                    let command = lex.word(0).unwrap_or("");
                    if !in_list(DBCC_READ, command) {
                        let command = command.to_ascii_uppercase();
                        return Err(self.blocked(line, &format!("DBCC {command}")));
                    }
                }
                // Sequence allocation survives a rollback, so the wrapper can't undo it.
                "NEXT" if lex.word_is(0, "VALUE") && lex.word_is(1, "FOR") => {
                    return Err(Blocked {
                        line,
                        reason: format!(
                            "NEXT VALUE FOR {} advances a sequence, which a rollback can't undo",
                            self.name_at(&lex.after(2))
                        ),
                        advice: WRITE_ADVICE.into(),
                    });
                }
                word if BLOCKED.contains(&word) => {
                    return Err(Blocked {
                        line,
                        reason: format!(
                            "{word} is not allowed in read-only mode (if it is a column name, write it as [{}])",
                            token.text
                        ),
                        advice: WRITE_ADVICE.into(),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The position after `TOP (n)` / `TOP n` if `at` is at one, else `at`.
    fn skip_top(&self, at: &Lexer<'a>) -> Lexer<'a> {
        if !at.word_is(0, "TOP") {
            return at.clone();
        }
        if at.kind(1) != Some(Kind::Punct('(')) {
            return at.after(2);
        }
        let mut scan = at.after(1);
        let mut depth = 0usize;
        while let Some(t) = scan.next() {
            match t.kind {
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return scan;
                    }
                }
                _ => {}
            }
        }
        scan
    }

    /// The token at `target` must be a session temp table (`#t`) or a table variable (`@t`).
    /// Global temp tables (`##t`) are shared with other sessions, so writing to them blocks.
    /// `line` is the line of the keyword that makes the write.
    fn require_scratch(&self, line: u32, target: &Lexer<'a>, what: &str) -> Result<(), Blocked> {
        match target.ahead(0) {
            Some(t) if t.kind == Kind::Temp && t.text.starts_with("##") => Err(Blocked {
                line,
                reason: format!(
                    "{what} {} would write to a global temp table, which other sessions share",
                    t.text
                ),
                advice: "Use a #temp table, or rerun with --allow-write".into(),
            }),
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

    /// `after` is the position just past the EXEC keyword, which is on `line`.
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
        self.check_proc(&name, "EXEC", false)
    }

    /// Checks the (possibly multi-part) procedure name starting at `start` against the
    /// allowlist. `batch_start` is true for the implicit call of a batch that begins with a
    /// word that is no statement keyword.
    fn check_proc(&self, start: &Lexer<'a>, what: &str, batch_start: bool) -> Result<(), Blocked> {
        let mut parts = Dotted::new(start);
        let mut last = None;
        for part in parts.by_ref() {
            last = Some(part);
        }
        let end = parts.rest();
        let last = last.filter(|t| matches!(t.kind, Kind::Word | Kind::Quoted));
        let name = last.map_or("", |t| t.name);
        let line = start.ahead(0).map_or(1, |t| t.line);
        if !in_list(PROCS, name) && batch_start {
            // A batch that starts with an unknown word is run as a procedure call by SQL
            // Server, so a typo like `SELEC 1` lands here too.
            return Err(Blocked {
                line,
                reason: format!(
                    "{} starts the batch, so SQL Server would run it as a stored procedure (a typo?); \
                     only read-only system procedures may run without --allow-write",
                    self.name_at(start)
                ),
                advice: "Fix it if it's a typo, or rerun with --allow-write to call the procedure"
                    .into(),
            });
        }
        if !in_list(PROCS, name) {
            return Err(self.blocked(line, &format!("{what} {}", self.name_at(start))));
        }
        if !self.local_system_qualifier(start) {
            // The policy, not a claim about what the name resolves to: it may well be the
            // system procedure, but another schema, database or linked server can name a
            // different one.
            return Err(Blocked {
                line,
                reason: format!(
                    "{what} {}: only a bare, sys. or master. name is taken as the system \
                     procedure (another schema, database or linked server can name a different one)",
                    self.name_at(start)
                ),
                advice: format!(
                    "Call it as sys.{}, or rerun with --allow-write to run it as written",
                    last.map_or("", |t| t.text)
                ),
            });
        }
        if name.eq_ignore_ascii_case("SP_SPACEUSED") && self.spaceused_updates_usage(&end) {
            return Err(Blocked {
                line,
                reason: "sp_spaceused with @updateusage (named, or as the second argument) runs \
                         DBCC UPDATEUSAGE, which writes"
                    .into(),
                advice: "Leave @updateusage out to only read sizes, or rerun with --allow-write"
                    .into(),
            });
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

    /// `sp_spaceused`'s second positional parameter is `@updateusage`. `args_start` is the
    /// position just past the procedure's name.
    fn spaceused_updates_usage(&self, args_start: &Lexer<'a>) -> bool {
        let mut positional = 0;
        let mut depth = 0usize;
        let mut arg_start = true;
        let mut scan = args_start.clone();
        while let Some(t) = scan.next() {
            match t.kind {
                Kind::Punct(';') if depth == 0 => break,
                Kind::Word if depth == 0 && in_list(STATEMENT_START, t.name) => break,
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
                if named && t.name.eq_ignore_ascii_case("@UPDATEUSAGE") {
                    return true;
                }
                if !named {
                    positional += 1;
                }
            }
        }
        positional >= 2
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
        Blocked {
            line,
            reason: format!("{what} would write"),
            advice: WRITE_ADVICE.into(),
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
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str) -> Self {
        Lexer {
            sql,
            pos: 0,
            line: 1,
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
}

fn word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$' || c >= 0x80
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
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
                c if c.is_ascii_whitespace() => {
                    i += 1;
                    continue;
                }
                b'-' if next == Some(b'-') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
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
                    i += 1;
                    while i < bytes.len() && word_char(bytes[i]) {
                        i += 1;
                    }
                    if c == b'@' { Kind::Var } else { Kind::Temp }
                }
                c if c.is_ascii_digit() => {
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.')
                    {
                        i += 1;
                    }
                    Kind::Num
                }
                c if word_char(c) => {
                    while i < bytes.len() && word_char(bytes[i]) {
                        i += 1;
                    }
                    Kind::Word
                }
                _ => {
                    // Multi-byte characters only appear inside words, which start with a letter or
                    // a byte >= 0x80, so `c` is ASCII here.
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
            "EXEC dbo.PurgeOrders would write"
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
        ] {
            assert!(blocked(sql).reason.contains("UPDATEUSAGE"), "{sql}");
        }
    }

    // PLAN.md 2.15: input that says XACT_ABORT gets exact counting.
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
        };
        let check_read_only = Policy {
            writes: false,
            compile_only: true,
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
                },
            );
        }
    }

    // PLAN.md 2.14: the guard reads the tokens as a stream and keeps none of them. The tests
    // below pin the stream itself, the case-insensitive matching that replaced the uppercased
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
        assert_eq!(keyword("é_x", &mut buf), "é_X");
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
            "DBCC FREEPROCCACHE would write"
        );
        assert_eq!(blocked("dbcc").reason, "DBCC  would write");
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
                    compile_only: true
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
        assert_eq!(b.reason, format!("EXEC {long} would write"));
        assert_eq!(
            blocked(&format!("{long} 1")).reason,
            format!(
                "{long} starts the batch, so SQL Server would run it as a stored procedure (a typo?); \
                 only read-only system procedures may run without --allow-write"
            )
        );
    }

    // The refusals' line numbers are the contract of PLAN.md 2.14's restructuring: the line of
    // the keyword that makes the write, through comments and multi-line strings and names.
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
            },
        )
        .expect_err("refused");
        assert_eq!(b.line, 3);
        // Kept as it was: an EXEC with nothing after it is reported on line 1, because the
        // refusal looks the line up on the token that should follow the keyword.
        assert_eq!(blocked("SELECT 1\nEXEC").line, 1);
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
        assert!(b.reason.ends_with(".a.a would write"));
        assert_eq!(
            b.reason.len(),
            "EXEC ".len() + 2 * n + 1 + " would write".len()
        );
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
}
