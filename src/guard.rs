//! Read-only guard, layer 1 (DESIGN.md §7): a keyword classifier over a T-SQL token
//! stream. It fails closed: any write keyword outside strings, comments and quoted names
//! blocks the batch unless its target is a temp table (#t) or table variable (@t).
//! Layer 2, the rolled-back wrapper transaction, lives in main.rs.

#[derive(Debug, PartialEq, Eq)]
pub struct Blocked {
    /// 1-based line within the batch.
    pub line: u32,
    pub reason: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Bare identifier or keyword; `upper` holds the uppercased text.
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

#[derive(Debug)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    upper: String,
    line: u32,
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

pub fn check(sql: &str) -> Result<(), Blocked> {
    let tokens = tokenize(sql);
    Guard { tokens: &tokens }.check()
}

struct Guard<'t, 'a> {
    tokens: &'t [Token<'a>],
}

impl Guard<'_, '_> {
    fn check(&self) -> Result<(), Blocked> {
        if let Some(first) = self.tokens.first() {
            let is_label = matches!(self.kind(1), Some(Kind::Punct(':')));
            let implicit_call = match first.kind {
                Kind::Word => !STATEMENT_START.contains(&first.upper.as_str()) && !is_label,
                Kind::Quoted => true,
                _ => false,
            };
            if implicit_call {
                self.check_proc(0, "a procedure call")?;
            }
        }
        for (i, token) in self.tokens.iter().enumerate() {
            if token.kind != Kind::Word {
                continue;
            }
            match token.upper.as_str() {
                "INSERT" => {
                    let mut j = self.skip_top(i + 1);
                    if self.word(j) == Some("INTO") {
                        j += 1;
                    }
                    self.require_scratch(i, j, "INSERT")?;
                }
                "INTO" => self.require_scratch(i, i + 1, "INTO")?,
                "UPDATE" => {
                    if self.word(i + 1) == Some("STATISTICS") {
                        return Err(self.blocked(i, "UPDATE STATISTICS"));
                    }
                    self.require_scratch(i, self.skip_top(i + 1), "UPDATE")?;
                }
                "DELETE" => {
                    let mut j = self.skip_top(i + 1);
                    if self.word(j) == Some("FROM") {
                        j += 1;
                    }
                    self.require_scratch(i, j, "DELETE")?;
                }
                "TRUNCATE" => self.require_table(i, "TRUNCATE TABLE")?,
                "ALTER" => self.require_table(i, "ALTER")?,
                "CREATE" => {
                    let mut j = i + 1;
                    while matches!(self.word(j), Some("UNIQUE" | "CLUSTERED" | "NONCLUSTERED")) {
                        j += 1;
                    }
                    match self.word(j) {
                        Some("TABLE") => self.require_scratch(i, j + 1, "CREATE TABLE")?,
                        // CREATE INDEX name ON #t
                        Some("INDEX") if self.word(j + 2) == Some("ON") => {
                            self.require_scratch(i, j + 3, "CREATE INDEX")?
                        }
                        _ => return Err(self.blocked(i, "CREATE")),
                    }
                }
                "DROP" => {
                    if self.word(i + 1) != Some("TABLE") {
                        return Err(self.blocked(i, "DROP"));
                    }
                    let mut j = i + 2;
                    if self.word(j) == Some("IF") && self.word(j + 1) == Some("EXISTS") {
                        j += 2;
                    }
                    loop {
                        self.require_scratch(i, j, "DROP TABLE")?;
                        if !matches!(self.kind(j + 1), Some(Kind::Punct(','))) {
                            break;
                        }
                        j += 2;
                    }
                }
                "EXEC" | "EXECUTE" => self.check_exec(i)?,
                "BEGIN" => {
                    if let Some(w @ ("TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG")) =
                        self.word(i + 1)
                    {
                        return Err(self.blocked(i, &format!("BEGIN {w}")));
                    }
                }
                "DBCC" => {
                    let command = self.word(i + 1).unwrap_or("");
                    if !DBCC_READ.contains(&command) {
                        return Err(self.blocked(i, &format!("DBCC {command}")));
                    }
                }
                word if BLOCKED.contains(&word) => {
                    return Err(Blocked {
                        line: token.line,
                        reason: format!(
                            "{word} is not allowed in read-only mode (if it is a column name, write it as [{}])",
                            token.text
                        ),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn word(&self, i: usize) -> Option<&str> {
        self.tokens
            .get(i)
            .filter(|t| t.kind == Kind::Word)
            .map(|t| t.upper.as_str())
    }

    fn kind(&self, i: usize) -> Option<&Kind> {
        self.tokens.get(i).map(|t| &t.kind)
    }

    /// Skips `TOP (n)` / `TOP n` starting at `i`.
    fn skip_top(&self, i: usize) -> usize {
        if self.word(i) != Some("TOP") {
            return i;
        }
        if !matches!(self.kind(i + 1), Some(Kind::Punct('('))) {
            return i + 2;
        }
        let mut depth = 0;
        for (j, t) in self.tokens.iter().enumerate().skip(i + 1) {
            match t.kind {
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        return j + 1;
                    }
                }
                _ => {}
            }
        }
        self.tokens.len()
    }

    /// The token at `target` must be a temp table or table variable.
    fn require_scratch(&self, at: usize, target: usize, what: &str) -> Result<(), Blocked> {
        match self.kind(target) {
            Some(Kind::Temp | Kind::Var) => Ok(()),
            _ => Err(self.blocked(at, &format!("{what} {}", self.name_at(target)))),
        }
    }

    /// `KEYWORD TABLE #t` is allowed; anything else after KEYWORD blocks.
    fn require_table(&self, at: usize, what: &str) -> Result<(), Blocked> {
        if self.word(at + 1) == Some("TABLE") {
            self.require_scratch(at, at + 2, what)
        } else {
            Err(self.blocked(at, what))
        }
    }

    fn check_exec(&self, at: usize) -> Result<(), Blocked> {
        let mut j = at + 1;
        match self.kind(j) {
            Some(Kind::Punct('(')) => return Err(self.blocked(at, "EXEC (dynamic SQL)")),
            Some(Kind::Var) if matches!(self.kind(j + 1), Some(Kind::Punct('='))) => j += 2,
            Some(Kind::Var) => {
                return Err(self.blocked(at, "EXEC of a procedure named by a variable"));
            }
            _ => {}
        }
        if self.word(j) == Some("AS") {
            return Err(self.blocked(at, "EXECUTE AS"));
        }
        self.check_proc(j, "EXEC")
    }

    /// Checks the (possibly multi-part) procedure name starting at `start` against the allowlist.
    fn check_proc(&self, start: usize, what: &str) -> Result<(), Blocked> {
        let last = self
            .dotted(start)
            .into_iter()
            .last()
            .filter(|t| matches!(t.kind, Kind::Word | Kind::Quoted));
        let name = last.map_or("", |t| t.upper.as_str());
        if !PROCS.contains(&name) && start == 0 && what != "EXEC" {
            // A batch that starts with an unknown word is run as a procedure call by SQL
            // Server, so a typo like `SELEC 1` lands here too.
            return Err(Blocked {
                line: self.tokens.first().map_or(1, |t| t.line),
                reason: format!(
                    "{} starts the batch, so SQL Server would run it as a stored procedure (a typo?); \
                     only read-only system procedures may run without --allow-write",
                    self.name_at(start)
                ),
            });
        }
        if !PROCS.contains(&name) {
            return Err(self.blocked(start, &format!("{what} {}", self.name_at(start))));
        }
        if name == "SP_SPACEUSED" && self.tokens.iter().any(|t| t.upper == "@UPDATEUSAGE") {
            return Err(self.blocked(start, "sp_spaceused @updateusage"));
        }
        Ok(())
    }

    /// The tokens of the dotted name starting at `start` (`db..obj` allowed): name parts and dots.
    fn dotted(&self, start: usize) -> Vec<&Token<'_>> {
        let mut parts = Vec::new();
        let mut expect_part = true;
        for t in self.tokens.iter().skip(start) {
            match t.kind {
                Kind::Punct('.') => expect_part = true,
                Kind::Word | Kind::Quoted | Kind::Temp | Kind::Var if expect_part => {
                    expect_part = false
                }
                _ => break,
            }
            parts.push(t);
        }
        parts
    }

    /// The dotted name starting at `i`, as written, for messages.
    fn name_at(&self, i: usize) -> String {
        let name: String = self.dotted(i).iter().map(|t| t.text).collect();
        if name.is_empty() {
            "?".to_owned()
        } else {
            name
        }
    }

    fn blocked(&self, at: usize, what: &str) -> Blocked {
        Blocked {
            line: self.tokens.get(at).map_or(1, |t| t.line),
            reason: format!("{what} would write"),
        }
    }
}

fn tokenize(sql: &str) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    let word_char = |c: u8| {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$' || c >= 0x80
    };
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        let start = i;
        let start_line = line;
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
                i = skip_quoted(bytes, i + 1, b'\'', &mut line);
                Kind::Str
            }
            b'N' | b'n' if next == Some(b'\'') => {
                i = skip_quoted(bytes, i + 2, b'\'', &mut line);
                Kind::Str
            }
            b'[' => {
                i = skip_quoted(bytes, i + 1, b']', &mut line);
                Kind::Quoted
            }
            b'"' => {
                i = skip_quoted(bytes, i + 1, b'"', &mut line);
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
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
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
        let upper = match kind {
            Kind::Word | Kind::Var | Kind::Temp => text.to_ascii_uppercase(),
            Kind::Quoted => text[1..text.len().saturating_sub(1).max(1)].to_ascii_uppercase(),
            _ => String::new(),
        };
        tokens.push(Token {
            kind,
            text,
            upper,
            line: start_line,
        });
    }
    tokens
}

/// Returns the index just past the closing quote (doubled quotes are escapes).
fn skip_quoted(bytes: &[u8], mut i: usize, close: u8, line: &mut u32) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b if b == close => {
                if bytes.get(i + 1) == Some(&close) {
                    i += 2;
                    continue;
                }
                return i + 1;
            }
            b'\n' => *line += 1,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(sql: &str) {
        assert_eq!(check(sql), Ok(()), "should be allowed: {sql}");
    }

    fn blocked(sql: &str) -> Blocked {
        check(sql).expect_err(&format!("should be blocked: {sql}"))
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
                reason: "UPDATE dbo.Orders would write".into()
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
}
