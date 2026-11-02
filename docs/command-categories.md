# Command categories (Phase 6 design)

Status: a design for owner decisions, 2026-10-23; built on HEAD `f905385` (exe 2,808,320 bytes), the commit that holds the interim `EXEC` allowlist step and its review fixes; its decisions D1–D15 were taken on 2026-10-23 and are recorded in PLAN.md §6 as decisions 22–36; each section says where a decision changed or confirmed its recommendation. It covers PLAN.md Phase 6, steps 1–4, and the interim step's open wording. It replaces an earlier draft written against `eb5f568` and an uncommitted build: every guard verdict here was re-run on `f905385` (Appendix A), and every issue two critiques raised against that draft is answered in Appendix B.

---

## 1. For the owner: summary

**Build order (owner decision 51, 2026-11-01; PLAN.md §6):** Part 1 (§6) in six thin slices, each released and used before the next: (1) refusal wording (§6.9) and the small items (§6.10); (2) `activity`, `errorlog` and `info` (§6.7); (3) read list round 2 and the exact-shape reads (§6.1, §6.2); (4) contention (§6.4); (5) the pause and showplan hygiene (§6.6, §6.8); (6) remote reach (§6.5), last, since its MSDTC check is blocked. Part 2 (§7) waits until real use shows which changes agents need. The decisions below (§9) stand.

**The model**
- **Permissions are the boundary; sqlh's checks are an accident guard** for a login granted more than the agent needs (VISION.md, Safety). Every rule is lexical. Synonyms, renamed objects, CLR functions and mapped drives get past the classifier in every mode. Dynamic SQL and `EXEC @var`, which read-only runs and MCP refuse, get past the deny tier under `--allow-write` or `--allow unvetted` (§2).
- **Read-only runs and every `sqlh mcp` read tool keep today's shape:** the classifier, then the rolled-back wrapper transaction. Part 1 makes them fit DBA work better: a longer read list, a few exact-shape reads that run alone outside the wrapper, lock-holding SQL refused, agent-chosen remote paths refused, a client-side pause for deltas, and sqlh's own `activity` and `errorlog` reads.
- **Changes come in six kinds a person can grant,** named in DBA words: `data`, `maintenance`, `tuning`, `kill`, `remote`, `unvetted`. Everything else that changes the server (settings, capture, resets, security, Agent jobs, mail) stays behind `--allow-write`. `read`, `scratch` and the deny tier are never switch names.
- **A kind is granted per profile and used per run.** The owner writes the profile's `allow` list, which holds even under `write_policy = "deny"`; a run uses a listed kind with `--allow KIND`. Each kind says whether its runs keep the wrapper: `unvetted` and `remote` keep it, the others drop it, a run keeps it only if every kind it grants keeps it, and a run without the wrapper refuses the shapes that relied on it.
- **A small deny tier is refused in every mode, `--allow-write` included:** `RESTORE DATABASE`/`LOG`, `BACKUP` to a non-local path, `SHUTDOWN`, `xp_cmdshell`, `sp_OA*`, repairs and a few more (§7.1). It stops an agent that reaches for one by name. It does not stop one that hides it (§2).
- **Every refusal names its kind and the switch that lifts it, or says that none does.** Finer reason labels (contention, settings, cost, ...) appear only in refusal text and the docs.

**Two parts**
- **Part 1 (§6), read-only runs and `sqlh mcp`.** Classifier v2's first increment. It needs D2–D9 and none of D1, and can be built before Part 2. It adds no grant switch.
- **Part 2 (§7), changes.** It adds the deny tier, the kinds, `--allow`, the profiles list and the `execute` tool; those (D10–D12, D15) need D1(b) first. Three of its read-side features use no kind and can be built with Part 1, apart from their `--allow` mentions: one literal of dynamic SQL classified under the caller's policy (D13(b)), sqlh-owned per-database execution (D13(d)), and trusted procedure entries in CLI read-only runs and MCP (D14(b)). **Decided (decision 36): no `execute` tool in Phase 6;** Part 2 is CLI only, and column E below describes a later phase.

**Decisions** (§9 has the options and trade-offs). Each is recorded in PLAN.md §6, from decision 22 on (D1 is decision 22).
1. **D1 Shape and names:** grant six kinds per profile and run, and keep `--allow-write` as the blanket switch. **Decided: (b), decision 22.**
2. **D2 Refusal wording:** the unknown-procedure and unknown-DBCC texts and named statements now; the kind and its switch in every refusal with v2. **Decided: (a), decision 23.**
3. **D3 Contention:** refuse lock-holding hints, REPEATABLE READ and SERIALIZABLE, deadlock priority above LOW, and a lock timeout above the run's (10 s cap when the run has none); SNAPSHOT as a run setting after a live test. **Decided: (a), and SNAPSHOT (i), decision 24.**
4. **D4 Remote:** (a) allow four-part reads through a DBA's linked server with a note, after the MSDTC test; (b) refuse agent-chosen UNC, URL and variable paths, and fail closed for unlisted path-taking functions. **Decided: D4a(i) and D4b(i), decision 25.**
5. **D5 Read list round 2:** the argument-checked DBCC commands and procedures, the msdb rule, `#temp` statistics, the exact-shape reads alone outside the wrapper, and `EXECUTE AS USER` reads in the CLI. Recommend all of (a)–(e), each entry gated on its live check; the one-shot `sp_server_diagnostics`, about 5 s on the server, in MCP too once measured. **Decided: all of (a)–(e), with (a1), decision 26.**
6. **D6 Waits:** a `:pause N` line between batches, in the CLI and in MCP's `sql`, capped and client-side; conditional on D3. **Decided: (a), 60 s cap, decision 27.**
7. **D7 sqlh-owned reads:** `activity` (sessions, blocking, head blockers) and `errorlog` (newest first), as CLI commands and MCP tools, and `info` reporting the login's view permissions. **Decided: (a), decision 28.**
8. **D8 Showplans in Phase 6:** hygiene only. **Decided: (a), decision 29.**
9. **D9 Small items:** the NOLOCK note, host-disclosure reads stay allowed, the application name per run kind, a refusal of the all-wildcard SAMPLED/DETAILED call, and MCP's session-state holes closed until tested. **Decided: yes to each, decision 30.**
10. **D10 Deny tier:** the fixed list in every mode, worded as an accident guard, plus `DROP DATABASE` and enabling `xp_cmdshell`, OLE Automation or CLR (any part of the option's name). Recommend (b), and (e), the deny tier reading one literal of dynamic SQL, once D13(b) exists. **Decided: (b) and (e), decision 31.**
11. **D11 Grants:** the profile's `allow` list (defaulting by `write_policy`), `--allow K` limited to it, and the wrapper per kind (kept only when every granted kind keeps it). Recommend (a), with the local `COPY_ONLY` backup kept in `maintenance` (i). **Decided: (a) with (i), decision 32.**
12. **D12 Kill:** a `kill` kind (`KILL <session id>`, `sp_stop_job`), no wrapper; an identity-checked kill later. **Decided: (a), decision 33.**
13. **D13 Dynamic SQL:** literal text classified under the caller's whole policy, plus sqlh-owned per-database execution; drop `sp_MSforeachdb`. **Decided: (b) and (d), decision 34.**
14. **D14 Trusted procedures:** owner-written entries, allowed in the CLI and in MCP under caps (in MCP on a connection closed after the call until a reset test passes), checked in the same batch for signatures and `EXECUTE AS`, with an optional hash pin. **Decided: (b), decision 35.**
15. **D15 The `execute` tool:** a first increment for listed `tuning`, `kill` and `maintenance` only, without `maintenance`'s backup. **Decided: (a), none in Phase 6, decision 36.**

---

## 2. Purpose and scope

**The framing (VISION.md, Safety).** Database permissions should be configured appropriately and remain the enforcement boundary. sqlh also needs its own checks, because a DBA or developer may grant an agent more access than it needs. The checks reduce accidental damage in that case while keeping legitimate DBA investigation practical. They are not a security boundary against a malicious principal with the same access, nor against an agent steered by text it read (prompt injection in data). Categories are an accident guard.

**What no lexical rule can stop.** The classifier reads tokens. It does not resolve names, read module bodies or follow strings into execution. So, whatever this design decides:
- **Dynamic SQL.** `EXEC (@sql)` and `sp_executesql @stmt` with a variable run text the classifier never saw; read-only runs and MCP refuse them (G1, G2 [probe]). Under `--allow-write` the classifier is off, and `EXEC ('xp_cmdshell ''dir''')` and `EXEC sp_executesql N'SHUTDOWN'` pass (W1, W6 [probe]). `--allow unvetted` admits dynamic SQL with the classifier on, so the same two pass there too, unless D13(b) classifies the one literal. A deny tier keyed on names reads inside a string only if it applies D13(b)'s one-literal rule (D10(e)); a variable or a concatenation it can't read at all.
- **`EXEC @var`.** `DECLARE @s nvarchar(100) = N'xp_cmdshell'; EXEC @s 'dir'` names the procedure at run time (G5 refused read-only; W5 passes under `--allow-write` [probe]; `--allow unvetted` admits it too).
- **Synonyms.** `SELECT * FROM dbo.syn_RemoteOrders` passes (G14 [probe]); the synonym may point at a linked server, a function or another database. `CREATE SYNONYM dbo.x FOR master.sys.xp_cmdshell` turns a denied name into an innocent one.
- **Renamed or shadowing objects.** A user procedure created as `master.dbo.sp_who2` may be what `EXEC master.dbo.sp_who2` runs [inf], and the qualifier rule accepts that spelling (DESIGN.md §7 calls the rule "a conservative policy, not a claim about what a given name resolves to"). A wrapper procedure around `xp_cmdshell` has any name its author chose.
- **CLR and other user functions.** `SELECT dbo.fn_x(1)` passes (G13 [probe]). A function backed by a CLR assembly with `EXTERNAL_ACCESS` or `UNSAFE` can write files, open sockets and read the registry from inside a `SELECT` [doc], so a "read" can send what it reads anywhere. A T-SQL function can call one.
- **Paths that look local.** A drive letter mapped or `SUBST`ed to a share (`N'Z:\'`, I2 passes [probe]), a junction, or a relative path whose base the DBA put on a share. The service account's file and network rights are the boundary there.
- **Code that runs with more rights than the caller:** a module `WITH EXECUTE AS OWNER`, or one signed with a certificate whose user holds more permissions [doc]. "Permissions are the boundary" holds for the caller's login, not for what such a module does once sqlh lets the call through.

**What the categories decide.** For each kind of change and each mode (CLI read-only, CLI with writes, a `deny` profile, `sqlh mcp`'s read tools, a future `execute` tool): allow, refuse with a switch, or refuse with none. Also the refusal wording and advice, which are part of the output contract (DESIGN.md §5.1 and §7).

**What they are not.** Not a permission system and not a substitute for least privilege. Not a claim that an allowed statement is harmless: a read can disclose data or load the server. Not a parser.

**Scope.** PLAN.md Phase 6, steps 1–4: workflows (§3), risks (§4), the model and its policy map (§5), the read-only side (§6, Part 1) and the change side (§7, Part 2), showplans (§8), and the owner decisions (§9). Also the interim step's open wording (D2), and the live tests still needed (§10).

**Evidence tags.** Every factual claim carries one; anything unverified says so.
- **[doc]** Microsoft Learn, as the surveys and critiques cited it; not re-read for this revision.
- **[live]** measured by a live test on the test server (SQL Server 2025 17.0.1000.7, Standard Developer): DESIGN.md §7's permission table (each server permission alone, with a `Reader` login; securityadmin only from the procedures' source), mcp-mode.md §5 and §9. **[live: sys.messages]** a message's text read from that server's `sys.messages`. **[live, by hand]** a one-off check recorded in mcp-mode.md or DESIGN.md §7, with no test.
- **[src]** source read: sqlh's own (`src/...`), a system procedure's (`OBJECT_DEFINITION`), or a community procedure's published source.
- **[probe]** the guard at `f905385`, offline; the ID (A1, B5, ...) is the case in Appendix A.
- **[mem]** a survey's or critique's memory, unchecked.
- **[inf]** inferred.
- **‡** a list entry no survey covered; check it before coding it.

---

## 3. Workflows

### 3.1 Table

Families: F1 blocking, sessions and locks; F2 waits, counters, I/O and resources; F3 Query Store, plan cache and showplans; F4 indexes, statistics, storage and integrity; F5 configuration, error logs, Agent, backups and security; F6 community procedures and cross-database helpers. ✓ passes the guard at `f905385`; ✗ is refused with exit 5 (Appendix A has the message). *(new)* marks a workflow the critiques added.

| Fam | Workflow | Freq | Key commands (guard at `f905385`) |
|---|---|---|---|
| F1 | Session and request overview | core | `EXEC sp_who2 'active'` ✓ B1; implicit `sp_who2 'active'` ✓ B2; `sys.dm_exec_requests` with `sys.dm_exec_sql_text` ✓ B3; `EXEC sp_WhoIsActive` ✗ B5 |
| F1 | Blocking chains and head blockers | core | `sys.dm_exec_sessions` with `sys.dm_exec_input_buffer` ✓ B4; `INSERT INTO #w EXEC sp_who2` ✓ I5; `sp_WhoIsActive @find_block_leaders = 1` ✗ B6; `INSERT INTO #t EXEC dbo.sp_WhoIsActive` ✗ B7 |
| F1 | What a session is running | core | `DBCC INPUTBUFFER(53)` ✓ B9; `sys.dm_exec_text_query_plan` ✓ J1; `INSERT INTO #ib EXEC ('DBCC INPUTBUFFER(53)')` ✗ B10 |
| F1 | Locks | common | `EXEC sp_lock` ✓ B11; `sys.dm_tran_locks` ✓ J2; `sys.dm_db_page_info` ✓ E18; `DBCC PAGE` ✗ E16 |
| F1 | Open and long transactions | core | `DBCC OPENTRAN` ✓ B12; `DBCC SQLPERF(LOGSPACE)` ✗ B13 |
| F1 | Deadlock history | common | `sys.fn_xe_file_target_read_file(N'system_health*.xel', ...)` ✓ B34; the same with the path read into a variable ✓ B35; ring buffer ✓ I28; error-log search ✓ A1, A3; `DBCC TRACEON(1222, -1)` ✗ B36 |
| F1 | Blocked process report | occasional | `sp_configure` listing ✓ A8, A9; with a value ✗ A10–A13; `CREATE EVENT SESSION` ✗ F22 |
| F1 | Ending a blocker | occasional | `KILL 53` ✗ B16; `KILL 53 WITH STATUSONLY` ✗ B17; `KILL '<uow>'` ✗ I10; `KILL STATS JOB 12` ✗ J23; `msdb.dbo.sp_stop_job` ✗ B18; a query that builds `KILL` text ✓ B37 |
| F1 | Own-session settings | occasional | `SET DEADLOCK_PRIORITY HIGH` ✓ B20, `@p` ✓ B21; `SET LOCK_TIMEOUT -1` ✓ B22, `@t` ✓ B23, `30000` ✓ B24; `WITH (TABLOCKX, HOLDLOCK)` ✓ B25; `(UPDLOCK)` ✓ B26; `(NOLOCK, XLOCK)` ✓ B27; `(SERIALIZABLE)` ✓ B28; `(HOLDLOCK)` ✓ I20; `(REPEATABLEREAD)` ✓ I21; isolation `SERIALIZABLE` ✓ B30, `REPEATABLE READ` ✓ B31; `sp_getapplock` ✗ B32; `sp_set_session_context` ✗ B33 |
| F1 | *(new)* Per-session deltas (CPU, reads) | core | two snapshots of `sys.dm_exec_requests`/`sys.dm_exec_sessions` around a wait: `WAITFOR` ✗ C1; a spin loop ✓ C2; `sp_WhoIsActive @delta_interval` ✗ (B5) |
| F2 | Cumulative waits, latches, spinlocks | core | `SELECT ... INTO #w1 FROM sys.dm_os_wait_stats` ✓ C2 |
| F2 | Interval sampling (deltas) | core | `WAITFOR DELAY` between snapshots ✗ C1, C3; CPU spin loop ✓ C2; a `:pause 10` line between `GO` batches ✓ I1 (passes the guard today; a syntax error on the server); `sp_BlitzFirst @Seconds = 5` ✗ C15 |
| F2 | *(new)* Per-query deltas (what burned CPU in the last 30 s) | core | `sys.dm_exec_query_stats` with `sys.dm_exec_sql_text` ✓ I26; the delta needs a pause, as above |
| F2 | A call's own waits | common | `sys.dm_exec_session_wait_stats WHERE session_id = @@SPID` ✓ I7 |
| F2 | Statistics resets | occasional | `DBCC SQLPERF('sys.dm_os_wait_stats', CLEAR)` ✗ C5; `ALTER RESOURCE GOVERNOR RESET STATISTICS` ✗ C6 (as a bare `ALTER`) |
| F2 | Counters; file I/O latency | core | `sys.dm_os_performance_counters` ✓ J3; `sys.dm_io_virtual_file_stats` with `sys.master_files` ✓ J4 |
| F2 | Disk and volume space | common | `sys.dm_os_volume_stats` ✓ C16; `xp_fixeddrives` ✓ A6; `sys.dm_os_enumerate_filesystem(N'\\host\share', N'*')` ✓ C12; `N'Z:\'` ✓ I2 |
| F2 | Log space and VLFs | core | `sys.dm_db_log_info` ✓ H15; `DBCC SQLPERF(LOGSPACE)` ✗ B13; `DBCC LOGINFO` ✗ E30 |
| F2 | CPU and memory | common | `sys.dm_os_buffer_descriptors` ✓ C8 (heavy); `DBCC MEMORYSTATUS` ✗ C7 |
| F2 | Server and host inventory | core | `sys.dm_server_registry` ✓ I13; `sys.dm_server_services` ✓ I14; `xp_msver` ✓ A7; `xp_instance_regread` ✓ A5; `HAS_PERMS_BY_NAME` ✓ I15 |
| F2 | Health history | occasional | `sp_server_diagnostics` ✗ B14; with `@repeat_interval = 0` ✗ B15 |
| F2 | Cache flush and benchmarking | rare | `DBCC DROPCLEANBUFFERS` ✗ C9; `DBCC FREESYSTEMCACHE('ALL')` ✗ C10; `CHECKPOINT` ✗ C11 |
| F3 | Query Store configuration | common | `sys.database_query_store_options` ✓ J6; `ALTER DATABASE CURRENT SET QUERY_STORE = ON` ✗ Q16; `... CLEAR` ✗ Q15 (both as a bare `ALTER`) |
| F3 | Query Store top consumers and regressions | core | catalog joins ✓ J5; `sp_query_store_force_plan` ✗ Q17; `unforce_plan` ✗ J10; `set_hints` ✗ Q18; `remove_plan` ✗ J22; `flush_db` ✗ Q19 |
| F3 | Plan cache | core | `sys.dm_exec_query_stats` ✓ I26; `.nodes()` over `sys.dm_exec_query_plan` ✓ Q25; `DBCC PROCCACHE` ✗ Q26 |
| F3 | Estimated plan | core | `SET SHOWPLAN_XML ON` / `GO` / `SELECT` ✓ Q2; `SHOWPLAN_ALL` ✓ J20; the same with `UPDATE` ✗ Q3, `EXEC dbo.Proc` ✗ Q4, `sp_executesql` ✗ Q5; `SET SHOWPLAN_XML ON; SELECT 1;` in one batch ✓ Q6 (the server refuses it) |
| F3 | Actual plan and runtime statistics | common | `SET STATISTICS XML ON; SELECT` ✓ Q7; with `EXEC dbo.Proc` ✗ Q8; `STATISTICS IO, TIME` ✓ Q13; `STATISTICS PROFILE` ✓ Q14; `OPTION (QUERYTRACEON 9481)` ✓ Q12 |
| F3 | Plan-cache remediation | occasional | `DBCC FREEPROCCACHE (0x06...)` ✗ Q20; bare ✗ Q21; `ALTER DATABASE SCOPED CONFIGURATION CLEAR PROCEDURE_CACHE 0x06...` ✗ Q22 (as a bare `ALTER`); `sp_recompile` ✗ Q23 |
| F3 | Plan guides; result-set metadata | rare / occasional | `sys.plan_guides` ✓ J36; `sp_create_plan_guide` ✗ J9; `sp_describe_first_result_set` ✓ Q24 |
| F4 | Index usage and missing indexes | core | `sp_helpindex` ✓ I30; `sys.dm_db_missing_index_details` ✓ J8 |
| F4 | Fragmentation | common | `sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')` ✓ E1 (every index in every database); scoped `'SAMPLED'` ✓ E2; mode in a variable ✓ E3; `DBCC SHOWCONTIG` ✗ E31; `sp_estimate_data_compression_savings` ✗ E20 |
| F4 | Statistics inspection | core | `DBCC SHOW_STATISTICS` ✓ E4; `sys.dm_db_stats_histogram` ✓ I27; `sp_autostats N'dbo.Orders'` ✗ E5 |
| F4 | Statistics and index maintenance | common | `UPDATE STATISTICS dbo.Orders` ✗ E6; `sp_updatestats` ✗ J11; `ALTER INDEX ... REORGANIZE` ✗ J12; `... WAIT_AT_LOW_PRIORITY (... ABORT_AFTER_WAIT = BLOCKERS)` ✗ E25; `DBCC SHRINKFILE` ✗ E26; `SHRINKDATABASE` ✗ J13; `UPDATEUSAGE` ✗ J14; `UPDATE STATISTICS #work` ✗ E7; `CREATE STATISTICS ... ON #work` ✗ E8; `ALTER INDEX ALL ON #work REBUILD` ✗ E9 |
| F4 | What-if indexes | occasional | `CREATE INDEX ... ON #work` ✓ E10; `... ON dbo.Orders` ✗ E11 |
| F4 | Space, rows, identity headroom | core | `sp_spaceused N'dbo.Orders'` ✓ I29; with `@updateusage = N'FALSE'` ✗ A16; `DBCC CHECKIDENT (..., NORESEED)` ✗ E12 |
| F4 | Integrity checks | occasional | `msdb.dbo.suspect_pages` ✓ J7; `DBCC CHECKDB WITH ESTIMATEONLY` ✗ E13; `CHECKTABLE` ✗ I6; `CHECKCONSTRAINTS (N'dbo.Orders')` ✗ E14; unscoped ✗ J21; `REPAIR_ALLOW_DATA_LOSS` ✗ E15 |
| F4 | Page internals and log records | rare | `sys.dm_db_database_page_allocations` ✓ E19; `DBCC IND` ✗ J38; `DBCC DBINFO` ✗ J37; `DBCC WRITEPAGE` ✗ E17; `sys.fn_dblog` ✓ E27; `sys.fn_dump_dblog(..., N'\\host\share\x.trn', ...)` ✓ E28 |
| F4 | Cross-server comparison | rare | four-part reads ✓ E21–E23; a four-part column reference ✓ E24 |
| F5 | Error log | core | `xp_readerrorlog` ✓ A1; `sp_readerrorlog` ✓ A2, implicit ✓ A3; `sp_enumerrorlogs` ✓ A4; the 7-argument `xp_` form with a time window and `N'desc'` ✓ A23; `INSERT INTO #log EXEC sys.sp_readerrorlog 0, 1` then `ORDER BY LogDate DESC` ✓ A22; `sp_cycle_errorlog` ✗ F21 |
| F5 | *(new)* The error log's oldest-first trap | core | `EXEC sys.sp_readerrorlog 0, 1` ✓ A2, but a least-privilege login has only this 4-argument form, oldest first (§3.2) |
| F5 | Registry, host, services | common | `xp_instance_regread` ✓ A5; `xp_regread` ✗ A18; `xp_instance_regwrite` ✗ A17 |
| F5 | Configuration | common | `sp_configure` listing ✓ A8, A9; with a value ✗ A10, A11, `;1` ✗ A12, a bare `GET` ✗ A13 |
| F5 | Agent jobs | common | `msdb.dbo.sysjobhistory` with `msdb.dbo.agent_datetime` ✓ F3; `msdb.dbo.sp_help_job` ✗ F1; `sp_help_jobactivity` ✗ F2; `sp_help_jobhistory` ✗ J18; `sp_start_job` ✗ F4 |
| F5 | Backups | common | `msdb.dbo.backupset` with `backupmediafamily` ✓ F9; `BACKUP ... WITH COPY_ONLY` local ✗ J30; to a share ✗ F8; to a URL ✗ J31 |
| F5 | *(new)* Restore-file checks | common | `RESTORE HEADERONLY` ✗ F5; `FILELISTONLY` ✗ F6; `LABELONLY` ✗ I12; `VERIFYONLY` ✗ I11 |
| F5 | Always On health | occasional | `ALTER AVAILABILITY GROUP ag FAILOVER` ✗ F10; cross-replica four-part reads ✓ E21 |
| F5 | Security, keys, audit | common | `sp_helprotect` ✓ A25; `sp_helplogins` ✗ F11; `sp_helpsrvrolemember` ✗ J16; `sp_change_users_login 'Report'` ✗ J15; `xp_logininfo` ✗ F12; `sp_help_revlogin` ✗ A24; `OPEN SYMMETRIC KEY` ✓ F15, with `DECRYPTBYKEY` ✓ I23; `OPEN MASTER KEY` ✓ F16; `GRANT` ✗ F31; `fn_get_audit_file(N'\\host\share\*.sqlaudit', ...)` ✓ F19; with the path in a variable ✓ F20 |
| F5 | *(new)* "Can user X read this?" | common | `EXECUTE AS USER = N'app_reader'; SELECT * FROM sys.fn_my_permissions(...); REVERT;` ✗ F13 |
| F5 | Linked servers and mail | occasional | `sys.servers` ✓ F26; `sp_linkedservers` ✗ F27; `sp_helplinkedsrvlogin` ✗ J17; `sp_testlinkedserver` ✗ F17; `OPENQUERY` ✗ F29; `EXEC (...) AT` ✗ F28; `EXEC Lnk.master.sys.sp_who` ✗ F30; `sp_send_dbmail` ✗ F18 |
| F6 | Live activity, health check, sampling | core | `sp_WhoIsActive` ✗ B5, B6, B8; `sp_Blitz @CheckServerInfo = 1` ✗ G9; `sp_BlitzFirst` ✗ C15 |
| F6 | Plan cache, index, maintenance | core / common | `sp_BlitzCache` ✗ G8; `dbo.IndexOptimize @Execute = 'N'` ✗ G10; `DBA.dbo.CommandLog` ✓ G11 |
| F6 | *(new)* Cross-database loops | common | a cursor over `sys.databases` with `EXEC (@sql)` ✗ G4 (line 7); `sp_MSforeachdb` ✗ G3; `sp_MSforeachtable` ✗ J19; `sp_ineachdb` ✗ J35 |
| F6 | Dynamic SQL | common | `sp_executesql N'SELECT 1'` ✗ G1; with parameters ✗ J32; `EXEC ('SELECT 1')` ✗ G2; text holding a refused statement ✗ G6, G7, I4 |
| F6 | Vetting what is installed | occasional | `HASHBYTES('SHA2_256', OBJECT_DEFINITION(...))` ✓ G12; `sys.crypt_properties` ✓ G15 |

### 3.2 What matters for a decision, by family

**F1 Blocking, sessions, locks**
- **The investigator appears in its own evidence, but less than the draft said.** A CLI read-only run holds its wrapper transaction from its first batch to its last, so between batches its session looks like "sleeping with an open transaction", the orphaned head blocker's signature. An idle `sqlh mcp` pooled session holds none: decision 17's live test saw it "idle with no open transaction" (mcp-mode.md §9 [live]); a call holds one only while it runs. Every sqlh session has `program_name` `sqlh` today, read-only or not (`src/db/mod.rs`, `application_name = "sqlh"` [src]). Excluding `program_name = N'sqlh'` from a report would hide a sqlh `--allow-write` run that is the real head blocker (agent trial 2's 927,269-row `DELETE` would have been one under `flag`; only the profile's `deny` stopped it). → D7 excludes only the report's own session and idle pooled ones; D9(c) names the run kind in the application name.
- **The wrapper makes lock-holding reads worse.** Locks taken under `UPDLOCK`, `XLOCK`, `TABLOCKX`, `HOLDLOCK`, `REPEATABLE READ` or `SERIALIZABLE` last to the run's closing `ROLLBACK`, not to the end of the statement [doc]. Decision 7's `LOCK_TIMEOUT 10000` bounds sqlh's own waits, not the waits it causes. The guard checks no `SET` statement [src], so in-SQL `SET DEADLOCK_PRIORITY HIGH` and `SET LOCK_TIMEOUT -1` override decision 7 (B20–B24 [probe]): in a deadlock an application session is then the victim. → D3.
- **`KILL` can't run in a user transaction:** Msg 6115, "KILL command cannot be used inside user transactions." [live: sys.messages]. Learn's KILL page doesn't say so (the draft's [doc] was wrong). `KILL n WITH STATUSONLY` "doesn't end or roll back" anything and only reports a rollback's progress [doc]; whether 6115 applies to it too is [inf]. → D5 (exact-shape reads), D12.
- **Session ids are reused.** Between the read that names a head blocker and a `KILL`, that session can end and its id go to another connection [mem]. Raw `KILL` takes only a constant [mem] and checks no identity. → D12.
- **`sp_server_diagnostics` with `@repeat_interval` of 5 or more never ends** until it is cancelled [doc]. The one-shot form waits too: the procedure "has to run at least 5 seconds to return complete data" [doc], so a call takes about 5 s on the server [inf]. → D5's argument check.
- **Optimized locking** (SQL Server 2025, per database) changes the evidence: XACT resources and `LCK_M_S_XACT_*` waits appear, and joins to `sys.partitions` miss XACT rows [doc].
- **Permissions decide what a read sees, silently.** On 2022 and later most F1 views need `VIEW SERVER PERFORMANCE STATE` [doc]; without it they show only the caller's own session, with no error. Whether `sp_who`, `sp_who2` and `DBCC INPUTBUFFER` show other sessions with that permission alone is unverified. → D7's `info` permissions.

**F2 Waits and resources**
- **Cumulative counters need a window, and MCP calls are stateless** (decision 15). Refusing `WAITFOR` (C1, C3) pushes agents toward CPU spin loops, which pass (C2 [probe]) and distort the CPU and `SOS_SCHEDULER_YIELD` they measure. The deltas DBAs sample most in an incident are per query (`sys.dm_exec_query_stats`) and per session (`sys.dm_exec_requests`, `sys.dm_exec_sessions`), not only waits. → D6.
- **`sys.dm_exec_session_wait_stats` resets with a pooled session's reset** [doc], so a call's own waits (I7) are a clean measurement.
- **Silent partial results.** Joining `sys.master_files` without `VIEW ANY DEFINITION`, `CREATE DATABASE` or `ALTER ANY DATABASE` returns zero rows and no error [doc]; an agent reads that as "no I/O". → D7 (`info`).
- **Resets are not transactional and destroy baselines** that other DBAs and monitoring tools rely on: `DBCC SQLPERF(... CLEAR)`, `ALTER RESOURCE GOVERNOR RESET STATISTICS`, which can't run in a transaction anyway [doc]. → "other changes", `--allow-write` only (§7.3).
- **`DBCC MEMORYSTATUS` returns dozens of small result sets** [mem]; `--max-results 20` (MCP: at most 20 per call) hides most. The memory views (`sys.dm_os_memory_clerks`, `sys.dm_os_process_memory`, `sys.dm_os_sys_memory`) answer in a few compact results. → D5.

**F3 Query Store and plans**
- **Plans are large:** tens to hundreds of KB of XML. `--max-cell 500` leaves a useless prefix; `max_cell 0` lets one call return up to `--max-output` (1,000,000 bytes). → §8.
- **`SET SHOWPLAN_XML` must be alone in its batch** [doc]. `SET SHOWPLAN_XML ON; SELECT 1;` passes the guard (Q6 [probe]) and fails on the server (error 1067 [mem]). → D8.
- **Under `SHOWPLAN_*`, `FMTONLY` or `PARSEONLY`, sqlh's closing `ROLLBACK` is only compiled or described, not run.** The CLI is safe: closing the session rolls the transaction back. `sqlh mcp` drops the connection, and the next call logs in again, at 148–185 ms (mcp-mode.md §5 [live, by hand]). → D8.
- **Tuning changes persist and affect every session** running the query: forcing a plan, setting Query Store hints, plan guides. Whether the `sp_query_store_*` procedures run inside a user transaction, and whether a rollback undoes them, is undocumented. → `tuning`, no wrapper (§5.5).
- **`sp_query_store_flush_db` isn't needed to read recent data:** for the current interval `sys.query_store_runtime_stats` has rows already flushed and rows still in memory; aggregate by `plan_id`, `execution_type` and `runtime_stats_interval_id` [doc]. It needs `ALTER` on the database [doc]. → its refusal carries that advice.
- **Qualification retargets a database:** `master.sys.sp_query_store_flush_db` acts on master.
- **SQL Server 2025's `ABORT_QUERY_EXECUTION` Query Store hint** makes every run of an application's query fail with error 8778 [doc; the docs disagree on GA or preview]. → deny tier.

**F4 Indexes, statistics, integrity**
- **The wrapper undoes little here:** `UPDATE STATISTICS` "commits independently of the enclosing transaction" [doc, a SQL Server 2008 R2 page; recheck on 2025]; `ALTER INDEX ... REORGANIZE` is not rolled back [doc]; `DBCC SHRINKFILE` keeps the work it completed [doc]; a rolled-back DDL statement still invalidated plans and wrote log.
- **DDL inside the wrapper holds Sch-M to the run's end,** which blocks even NOLOCK readers [doc], and is then rolled back. → `maintenance` runs without the wrapper.
- **`--allow-write` is all or nothing:** one `DBCC CHECKTABLE` drops the classifier and the wrapper for the whole input. → D11.
- **`sys.dm_db_index_physical_stats` treats NULL as a wildcard,** including an `OBJECT_ID()` that returns NULL [doc]. E1's all-NULL `'DETAILED'` call reads every page of every index in every database. The mode can come from a variable (E3 [probe]), so no lexical check is complete. → D9(d).
- **`DBCC CHECKIDENT(t)` with one argument reseeds** [doc]; only `NORESEED` reads.
- **Not cheap reads:** `DBCC CHECKDB` runs on an internal database snapshot [doc] (whether `ESTIMATEONLY` creates one is unverified); `sp_estimate_data_compression_savings` copies a sample of the object into tempdb [doc]. → both stay `maintenance`.
- **`DBCC CHECKCONSTRAINTS` with a named table or constraint** "constructs and executes a query" and returns the violating rows; the query data goes to a temp table [doc]. Without an argument it checks every table [doc]. → D5.
- **Histograms disclose up to 200 real values per column** [doc]. Permissions decide.
- **The agent's own queries move the evidence:** `user_scans` and `last_user_*` change as it investigates.

**F5 Configuration, logs, Agent, backups, security**
- **The qualifier rule makes every msdb read procedure unreachable:** `msdb.dbo.sp_help_job` (F1), `sp_help_jobactivity` (F2) and `sp_help_jobhistory` (J18) are refused; reading the msdb tables passes (F3, F9). → D5's msdb rule.
- **Some reads can't run in a transaction,** so they can never ride the wrapper: `RESTORE HEADERONLY`, `FILELISTONLY` and `LABELONLY` (Msg 3021, "Cannot perform a backup or restore operation within a transaction." [live: sys.messages]; that the `...ONLY` forms raise it is [inf]), and `KILL ... WITH STATUSONLY` (above). → D5's exact-shape class.
- **Settings:** `RECONFIGURE` can't run in a user transaction (Msg 574, "%ls statement cannot be used inside a user transaction." [live: sys.messages]; that `RECONFIGURE` raises it [doc]); nor can `ALTER DATABASE` (Msg 226, "%ls statement not allowed within multi-statement transaction." [live: sys.messages]; that `ALTER DATABASE` raises it [doc]).
- **The error log's oldest-first trap.** `xp_readerrorlog` takes a time window and a sort order (A23), but needs sysadmin or an explicit `GRANT EXECUTE` [live]. A least-privilege login (`VIEW SERVER STATE` or `VIEW ANY ERROR LOG`) gets only `sp_readerrorlog`, whose four parameters are log number, log type and two search strings (DESIGN.md §7 [live]): no window, no order, and the log comes oldest first [mem]. With the default `--max-rows 100` the agent sees startup messages and the incident hides behind a count. `INSERT INTO #log EXEC sys.sp_readerrorlog 0, 1` then `ORDER BY LogDate DESC` passes (A22 [probe]). That it works inside the wrapper is [inf]: the live test ran only `INSERT INTO #log EXEC xp_readerrorlog 0, 1`, as the test profile [live], and `sp_readerrorlog`'s source only checks a permission and calls its `xp_` [src]; §10 item 10 checks it with a least-privilege login. → D7.
- **Restore-file checks.** msdb history (F9) covers only backups this server took whose history hasn't been purged. Checking a file from elsewhere before a restore needs `RESTORE HEADERONLY`/`FILELISTONLY`, refused today (F5, F6) and needing `CREATE DATABASE` [doc]. → D5.
- **"Can user X read this?"** has no read-only path: `EXECUTE AS` is refused (F13), and `fn_my_permissions` and `HAS_PERMS_BY_NAME` evaluate the caller. → D5(e).
- **Agent jobs started by `sp_start_job` run in the Agent process,** outside the transaction [doc]. `sp_cycle_errorlog` looks like an error-log reader; exact names are required.
- **Host disclosure:** `xp_instance_regread` runs with no server permission and read `LoginMode` [live]; a login with no server permission also read the service's account with it [live, by hand: DESIGN.md §7's one-off probe]. `sys.dm_server_services` (I14) shows the account too to a login with `VIEW SERVER STATE` [doc]. → D9(b).

**F6 Community procedures and cross-database helpers**
- **The guard can't see inside user code,** and trusting a name trusts whatever is installed under it. → D14.
- **Effects inside them escape the classifier and the wrapper:** waits (`sp_BlitzFirst` defaults to 5 s; `sp_WhoIsActive @delta_interval`, `sp_PressureDetector @sample_seconds`, `sp_HumanEvents` [src]); `xp_cmdshell` in `sp_Blitz` when it is enabled [src]; event sessions (`sp_HumanEvents`); linked-server writes (`@OutputServerName`, `@PushBackupHistoryToListener`); mail (`@EmailRecipients`).
- **Some end the wrapper mid-call:** `XACT_ABORT ON` in `sp_PressureDetector`, `sp_HumanEvents` and `sp_LogHunter`; `sp_BlitzIndex`'s CATCH blocks run `WHILE @@trancount > 0 ... ROLLBACK` [src]. sqlh notices at the next batch boundary (DESIGN.md §7's invariant); the procedure's later statements autocommit.
- **Ola Hallengren's `IndexOptimize` and `DatabaseIntegrityCheck` refuse `@@TRANCOUNT <> 0`** [src], so they can never run read-only, not even as `@Execute = 'N'` dry runs.
- **Cross-database loops are refused in every form** (G3, G4, J19, J35). `sp_MSforeachdb` is undocumented and known to skip databases [mem]. → D13.
- **MCP is the agents' main path** (agent trial 3: offered both, the model used only the MCP tools), and it has nothing in place of `sp_WhoIsActive`. → D7, D14.

---

## 4. Risks

| Risk | Definition | Does the rollback wrapper cover it? |
|---|---|---|
| `none` | No effect beyond rows and messages. | n/a |
| `data_write` | Changes rows in user or system tables. | **Yes** for DML inside the transaction. **No** for sequence values (`NEXT VALUE FOR`, refused for that reason, H1 [probe]) and identity values (DESIGN.md §7). **Unknown** for `DBCC CHECKIDENT` reseeds and `DBCC UPDATEUSAGE`. |
| `schema_change` | Creates, alters or drops objects, indexes or statistics. | **Mostly** the metadata. Not what happened meanwhile: Sch-M blocking, plan invalidation, log volume. `UPDATE STATISTICS` and `REORGANIZE` are not undone [doc]. |
| `server_config_change` | Server or database settings, scoped configuration, trace flags, XE sessions, forced plans and hints, plan guides. | **No.** Several can't run in a user transaction: `RECONFIGURE` (Msg 574), `ALTER DATABASE` (Msg 226) [live: sys.messages; which statements raise them, doc], `ALTER RESOURCE GOVERNOR` and resumable index DDL [doc]. The rest are untested or undocumented. |
| `server_state_reset` | Clears caches, statistics or diagnostic history: plan cache, buffer pool, wait statistics, Query Store statistics, error-log rollover. | **No** [doc]. |
| `session_state` | State that outlives a statement: `SET` options, `CONTEXT_INFO`, session context, cursors, open keys, session trace flags, impersonation. | **No.** The CLI's session ends with the run. In MCP the reset clears a verified list (mcp-mode.md §5) and the setup re-sets the rest; other state reaches the next call on that connection (§6.3). |
| `intervention` | Ends or stops other sessions' work: `KILL`, `sp_stop_job`. | **No,** and `KILL` can't run inside it (Msg 6115 [live: sys.messages]). A killed session's rollback can take a long time, longer for a long transaction [doc]; about as long as the work did, 5 to 15 s for 237 MB of log (DESIGN.md §4.3 [live]). |
| `lock_or_wait` | Holds or waits on locks, waits on purpose, or makes others wait or lose. | **Worse inside it:** locks under hints or REPEATABLE READ/SERIALIZABLE are held to the closing `ROLLBACK`. Waits caused and others' deadlock rollbacks are not undone. |
| `external_side_effect` | Effects outside the instance's transactions: files, registry, OS commands, mail, linked servers, Agent jobs, hosts reached through paths, CLR code with `EXTERNAL_ACCESS` or `UNSAFE` (even inside a `SELECT`, §2). | **No** [doc: extended procedures "aren't within the scope of a transaction"]. A CLR function's file, socket or registry effects are outside it too [inf]. A UNC path makes the service account open a connection to the named host and authenticate to it [inf]. `BACKUP` to a share or URL copies a whole database off the server. Mail queued through Service Broker is probably discarded by the rollback [inf]. |
| `disclosure` | Reading sensitive data beyond table rows: other sessions' SQL and plans, registry values, service accounts, histograms, audit records, credential material. | n/a. Permissions are the boundary (VISION.md). A CLR function with `EXTERNAL_ACCESS` or `UNSAFE` can send what a `SELECT` reads elsewhere (§2). |
| `resource_cost` | Heavy CPU, I/O or memory: whole-instance `DETAILED` scans, plan-cache XML shredding, `sys.dm_os_buffer_descriptors`, CHECKDB. | n/a. Bounded only by `-t` (60 s by default; none under `-t 0`), cancellation, and the 250 ms count budget from the first hidden row (decision 6). `-t` stops sqlh's wait; the server's I/O already done stays done. |
| `security_change` | Permissions, principals, impersonation, audits, keys. | GRANT and principal DDL inside a transaction are probably rolled back [inf]. `EXECUTE AS ... WITH NO REVERT` changes the session's identity for good; in MCP that session is pooled. |
| `elevation_through_code` | Code that runs with more rights than the caller: modules `WITH EXECUTE AS OWNER` or a user, and modules signed with a certificate [doc]. | n/a. It bends "permissions are the boundary" for whatever sqlh lets call such a module (D14). |

**What follows.** Only `data_write` and most of `schema_change` are covered by the wrapper. Everything else is stopped by the classifier, granted on purpose, or accepted openly.

**A wrapper can be lost mid-batch.** A procedure that runs `COMMIT` or `ROLLBACK`, or one under `XACT_ABORT ON`, can end the wrapper inside a batch. sqlh notices only at the next batch boundary or at the end (DESIGN.md §7's invariant), and the procedure's later statements autocommit.

---

## 5. The model: two layers

### 5.1 Layer 1: kinds of change a person can grant

Six kinds, named in DBA words. Categories that share a policy are one kind. Each is a switch: `--allow KIND` for a run, the profile's `allow` list for what a profile permits (§7.2). §7.3 lists each kind's exact membership.

| Kind | What it admits | Wrapper when granted (§5.5) |
|---|---|---|
| `data` | Data and schema changes: DML and DDL on anything but a `#temp` table or a table variable, transaction control, global temp tables, sequences, Service Broker, and the lock-holding forms (contention, §6.4), which are write-intent locking | off |
| `maintenance` | Physical upkeep and checks: statistics updates, index rebuild and reorganize, `DBCC CHECK*` (no repair), shrink (no `BLOCKERS`), `UPDATEUSAGE`, `CHECKPOINT`, compression estimates, a `COPY_ONLY` backup to a local path (D11's sub-choice; never through `execute`'s first increment) | off |
| `tuning` | One query's or one plan's fix: force or unforce a plan, set or clear Query Store hints, evict one plan by handle, recompile one object | off |
| `kill` | `KILL <session id>`; `msdb.dbo.sp_stop_job` | off |
| `remote` | Reach beyond the instance: linked servers (`OPENQUERY`, `OPENDATASOURCE`, `OPENROWSET`, `EXEC ... AT`, four-part names unless D4a makes them reads), an external model's endpoint (`AI_GENERATE_EMBEDDINGS`, decision 55: refused until this kind can grant it), non-local paths in path-taking functions, Active Directory and file-system lookups | on, unless another granted kind drops it |
| `unvetted` | Code sqlh can't classify: procedures not on the read list, unlisted DBCC commands, dynamic SQL that isn't one literal | on; refused together with a kind that drops it |

**Not a kind:** everything else that changes the server. Settings, capture, resets, security, Agent jobs, mail, registry writes, local backups that aren't `COPY_ONLY`. These are "other changes": only `--allow-write` lifts them, and the `execute` tool never offers them. A seventh kind for settings is possible later; nothing in an investigation needs one now (system_health already captures deadlocks [doc]).

**Never switch names:** `read` (what runs without one), `scratch` (session-private state, a part of read) and the deny tier (nothing lifts it).

### 5.2 Layer 2: reason labels

Used only in refusal text and the docs, never as a switch. A refusal names its kind or label, then the switch (§6.9).

| Label | Where it applies | What lifts it |
|---|---|---|
| data read, metadata read, server-state read, host disclosure | read; documentation only | nothing needed |
| scratch | `#temp` and table-variable writes, cursors, `USE`, session `SET` options | nothing needed; in MCP only what the reset is shown to clear (§6.3) |
| contention | lock-holding hints and isolation, deadlock priority, lock timeout, `WAITFOR`, app locks (§6.4) | `data`, where the lock is the intent |
| exact-shape read | the few reads that must run alone outside the wrapper (§6.2) | nothing needed when alone |
| settings, capture, resets, security, jobs, mail, registry | "other changes" (§7.3) | `--allow-write` only |
| cost | the all-wildcard `SAMPLED`/`DETAILED` call (D9(d)) | a rewrite: name the database and object, or use `LIMITED` |
| MCP session state | session state the reset isn't shown to clear (§6.3) | nothing in MCP; the CLI runs it, and the refusal says so |
| deny tier | §7.1 | nothing |

### 5.3 Policy map

Modes: **A** CLI read-only (`flag` or no profile, without `--allow-write`). **B** CLI with `--allow-write` (`flag` with the flag, or `write_policy = "allow"`). **C** a profile with `write_policy = "deny"` (`--allow-write` is a usage error there, as today). **D** `sqlh mcp`'s read tools (decision 16). **E** the future `execute` tool (D15). **Column E is not built in Phase 6 (decision 36):** it records the design for a later phase.

`--allow K` lifts K in A and C when K is in the profile's `allow` list. That list defaults to every kind under `flag` and `allow`, to none under `deny`, and an explicit list replaces the default (§7.2). "If listed" below means that.

| Kind or label | A CLI read-only | B `--allow-write` | C `deny` profile | D MCP read tools | E `execute` (D15 rec.) |
|---|---|---|---|---|---|
| read, with scratch | allow | allow | allow | allow; scratch only where the reset is shown to clear it | allow |
| exact-shape reads (§6.2) | allow, alone, no wrapper | allow | allow, alone | allow, alone in a call | n/a |
| trusted procedure entries (D14) | allow, in the wrapper | allow | allow | allow, in the wrapper, under caps; the connection closed after the call until the reset test passes (§7.7) | allow |
| `tuning` | refuse; `--allow tuning` if listed | allow | refuse; `--allow tuning` if listed | refuse | if listed |
| `kill` | refuse; `--allow kill` if listed | allow | the same as A | refuse | if listed |
| `maintenance` | refuse; `--allow maintenance` if listed | allow | the same as A | refuse | if listed |
| `remote` | refuse (D4a may make linked-server reads plain reads); `--allow remote` if listed | allow | the same as A | refuse (D4a as in A) | not in Phase 6 |
| `unvetted` | refuse; `--allow unvetted` if listed, in the wrapper | allow | the same as A | refuse | not in Phase 6 |
| `data` | refuse; `--allow data` if listed | allow | the same as A | refuse | not in Phase 6 |
| other changes | refuse; `--allow-write` | allow | refuse; nothing lifts it | refuse | never |
| cost (D9(d)) | refuse; a rewrite | allow (classifier off) | refuse; a rewrite | refuse; a rewrite | refuse; a rewrite |
| deny tier (D10) | refuse; nothing lifts it | **refuse** (new; today it runs) | refuse | refuse | refuse |

### 5.4 Why each row has its policy: a risk ranking

Ranked by the worst likely accident when a careless or misled agent holds an over-granted login. Rank orders the switches; it also fixes the draft's column E, which allowed DML and unvetted code while refusing `UPDATE STATISTICS`.

| Rank | Kind | Worst likely accident | Undone by a rollback? | Reach | Consequence |
|---|---|---|---|---|---|
| 1 | read | discloses what the login may read; loads the server | n/a | the login's permissions | everywhere; permissions decide |
| 2 | `tuning` | a bad forced plan or hint for one query, for every session | no; the inverse call undoes it | one query | grantable; in E's first increment |
| 3 | `kill` | ends the wrong session (ids are reused); its rollback runs as long as its work | no | one session | grantable; in E's first increment |
| 4 | `maintenance` | a rebuild or CHECKDB loads a busy server for minutes; Sch-M blocks readers; shrink fragments | no | one object or database, and server load | grantable; in E's first increment |
| 5 | `remote` | the service account authenticates to a host the agent named; another server's data read under a mapping that exceeds the profile's rights | no (the other side) | other servers and hosts | grantable, wrapper on; not in E in Phase 6 |
| 6 | `unvetted` | whatever the code does outside the transaction: waits, `xp_cmdshell` where enabled, XE sessions, linked or mail writes, ending the wrapper mid-call | transactional effects only | unknown | grantable, wrapper on; not in E in Phase 6 |
| 7 | `data` | committed data loss (agent trial 2's model ran a 927,269-row `DELETE` with `--allow-write`; only the profile's `deny` stopped it) | no: the wrapper is off | the data | grantable; not in E in Phase 6 |
| 8 | other changes | instance-wide settings, permissions, jobs, mail, baselines reset | no | the instance | `--allow-write` only; never in E |
| 9 | deny tier | a replaced or dropped database, a database copied off the server, OS commands, a stopped service | no | the instance and host | no switch |

**Column by column.**
- **A and C** refuse every kind until the run names it with `--allow`, and C also needs the owner to list it; C has no blanket switch. A `deny` profile can now be given `maintenance`, `tuning` or `kill` without opening `data` (decision 12 kept).
- **B** is today's `--allow-write`, minus the deny tier.
- **D** stays read-only (decision 16). The exact-shape reads and trusted entries are reads.
- **E** offers only what the profile explicitly lists, and in Phase 6 only ranks 2–4, without `maintenance`'s backup (§7.8). The harness's approval prompt is the last check for E, and users often set it to always allow (decision 16's reason). The CLI's "a person sees the command line" is the same kind of prompt (an allowlisted `sqlh` command), so E is not treated as weaker than the CLI for what it offers; it offers less because it is new.

### 5.5 The wrapper per kind

| Kind | Wrapper | Why |
|---|---|---|
| `unvetted` | **kept** | "Run this procedure and roll back whatever it did" is the narrow switch for `sp_WhoIsActive`, `sp_BlitzCache` and the like. The rollback catches its transactional writes. |
| `remote` | **kept**, unless another granted kind drops it | A read. Whether a four-part read inside the wrapper runs, promotes to MSDTC or fails is the first live test (§10). The rollback never reaches the other side, so a remote construct loses no protection when another granted kind drops the wrapper. |
| `data` | dropped | The change is the intent (today's `--allow-write`). |
| `maintenance` | dropped | `BACKUP` can't run inside it (Msg 3021 [live: sys.messages]); a rebuild inside it holds Sch-M to the end and is then rolled back; `UPDATE STATISTICS` commits regardless [doc]. |
| `tuning` | dropped | The change is the intent; whether `sp_query_store_*` run inside a transaction is unknown; `FREEPROCCACHE` isn't transactional. |
| `kill` | dropped | `KILL` can't run inside it (Msg 6115 [live: sys.messages]). |

**The general rule:** a run keeps the wrapper only if every kind it grants keeps it. A run that grants `remote` with `data`, `maintenance`, `tuning` or `kill` (a four-part `INSERT` is `data` and `remote`, §5.6; `RESTORE VERIFYONLY` from a share is `maintenance` and `remote`) runs without it. §6.2's exact shapes run alone and without the wrapper wherever they are allowed; with a non-local path they are `remote` as well, and `--allow remote` runs them the same way, alone and without the wrapper, since `RESTORE` fails inside it (Msg 3021).

**Rules for a run without the wrapper** (any granted kind that drops it):
- **`unvetted` can't share it:** `--allow unvetted` with a kind that drops the wrapper is a usage error (exit 3): "unvetted code runs only in the rolled-back transaction; run it apart from maintenance". It is the one kind that needs the wrapper; `remote` only keeps it when nothing else drops it.
- **Wrapper-dependent shapes are refused.** Today an alias spelled like a temp table passes because the wrapper covers it (E29 [probe]; DESIGN.md §7). With the wrapper off and `data` not granted, a write whose target is spelled `#x` is refused unless the same input creates `#x` (`CREATE TABLE #x` or `INTO #x`). No `#x` the input doesn't create can exist in the run except as an alias. With `data` granted the write is intended, and `--allow-write` runs it today (W7 [probe]).
- **Exact counts.** As under `--allow-write` (decision 6): a stopped count skips later statements, which could include a `COMMIT`, so these runs count every hidden row.
- **No READ UNCOMMITTED,** as under `--allow-write` (decision 7): `--read-uncommitted` with such a kind is a usage error.
- **The classifier still runs** and refuses every kind the run wasn't granted. Its read verdicts are trusted without a backstop; that is the price of the narrower switch, and it is still narrower than `--allow-write`, which runs everything.

### 5.6 How classification works (classifier v2)

- **Constructs.** A statement keyword sequence (`ALTER INDEX`, `SET LOCK_TIMEOUT`), a procedure call (explicit `EXEC`, or the implicit call at a batch's start), a DBCC subcommand with its arguments, a word that counts wherever it appears (`WAITFOR`, `UPDLOCK`), a dotted name, a function call's path argument.
- **A construct can match more than one kind:** an `INSERT` into a four-part name is `data` and `remote`; `sys.fn_dump_dblog` with a UNC literal is `unvetted` and `remote`. A run needs every kind any of its constructs matches.
- **The refusal names the first construct by line,** and for it the first ungranted kind in this order: deny tier, other changes, then the kinds from rank 7 down (`data`, `unvetted`, `remote`, `maintenance`, `kill`, `tuning`), then the labels.
- **Lexical only.** The existing tokenizer (comments, nested ones too, `'..'`, `N'..'`, `[..]`, `".."`). Rules use word tokens, dotted names (empty parts count, as in `master..x`), literal kinds and parenthesis depth. No name resolution, no schema knowledge.
- **Fail closed.** A procedure, DBCC subcommand or path-taking function on no list is `unvetted`. An argument check counts only literals: a variable, an expression or a concatenation in a checked position puts the construct in the stricter kind.
- **Bounded work.** Every rule looks ahead a bounded number of tokens or scans its arguments under the existing caps (`MAX_ARG_PARENS` = 8, the work budget; DESIGN.md §7). Literal recursion (D13) charges the literal's bytes to the same budget.
- **Exempt.** sqlh's own SQL (`QueryOrigin` sqlh: the schema commands, `info`, and §6.7's `activity` and `errorlog`) is always read-only and not classified. `--check`'s NOEXEC rule is unchanged.

---

## 6. Part 1: read-only runs and `sqlh mcp`

Classifier v2's first increment. It changes what read-only runs and `sqlh mcp` allow and refuse; it adds no grant switch (the kinds and `--allow` are Part 2), so it doesn't wait for D1. Where an advice below names `--allow-write`, Part 2 later names the narrow kind instead.

### 6.1 Read list, round 2 (D5)

Each argument check uses literals only, with today's argument scanner and its caps (the `;number` and unreserved-word fixes of `f905385` included). All run inside the wrapper; each needs its live check (§10) before it ships.

| Construct | Allowed when | Otherwise | Note |
|---|---|---|---|
| `sp_server_diagnostics` | no arguments, or `@repeat_interval` (named, or the first positional argument) is the integer literal `0` | contention (it waits) | with 5 or more it never ends [doc]; the one-shot form takes about 5 s on the server ("has to run at least 5 seconds" [doc]; the time [inf], measured in §10 item 4), a bounded internal wait inside `-t` or the call's timeout |
| `DBCC SQLPERF` | exactly one argument, the bare word `LOGSPACE` or the string `'LOGSPACE'`/`N'LOGSPACE'` (any case); optionally `WITH NO_INFOMSGS` | other changes (resets) | |
| `DBCC CHECKIDENT` | exactly two arguments, the second the bare word `NORESEED`; optionally `WITH NO_INFOMSGS` | `data` | one argument reseeds [doc] |
| `DBCC MEMORYSTATUS` | always | — | dozens of result sets [mem]; docs and the skill lead with `sys.dm_os_memory_clerks`, `sys.dm_os_process_memory`, `sys.dm_os_sys_memory` |
| `DBCC CHECKCONSTRAINTS` | the first argument names a table or constraint (a literal) | `maintenance` (unscoped, every table) | runs a query; temp table [doc] |
| `sp_autostats` | exactly one argument (`@tblname`, positional or named), optionally a named `@indname`; no second positional argument, no `@flagc` | `maintenance` | display form [doc] |
| `sp_spaceused` | extend today's check: `@updateusage` is allowed when it is the literal `'false'`/`N'false'` (any case); `NULL` and anything else are refused | `maintenance` | A16 is refused today |
| `sp_helplogins`, `sp_helpsrvrole`, `sp_helpsrvrolemember`, ‡`sp_helpntgroup`, `sp_srvrolepermission`, `sp_dbfixedrolepermission` | always | — | read-only [doc/mem]; `sp_helplogins` needs securityadmin [doc] |
| `sp_change_users_login` | the first argument is the literal `'Report'` | other changes (security) | |
| `sp_linkedservers`, `sp_helplinkedsrvlogin`, `sp_helpserver` | always | — | local metadata |
| **msdb rule:** `sp_help_job`, `sp_help_jobhistory`, `sp_help_jobstep`, `sp_help_jobactivity`, `sp_help_operator`, `sp_help_alert`, `sysmail_help_status_sp`, `sysmail_help_queue_sp` | qualified exactly `msdb.dbo.` (they exist only in msdb) | `unvetted` | `SQLAgentReaderRole` [doc] |

**Not read, on purpose** (the safety critique): `DBCC CHECKDB`/`CHECKTABLE ... WITH ESTIMATEONLY` and `sp_estimate_data_compression_savings` stay `maintenance`. Neither is a cheap read (§3.2, F4), and `-t` is the only bound in a read-only run or an MCP call (none under `-t 0`).

**Stays out:** `xp_regread` (any key, not mapped to the instance) and `xp_enumerrorlogs` (`sp_enumerrorlogs` covers it), as built in the interim step (PLAN.md Phase 6, Built); both fall to `unvetted`.

**What these additions keep:** none of them takes an app lock or sets session context, and none waits as `WAITFOR` does, once `sp_server_diagnostics`'s repeat form is refused. So the letter of plan-history.md 5.4's MCP rule holds (§6.11). One addition is not instant: the one-shot `sp_server_diagnostics` runs about 5 s on the server, on a preemptive thread at high priority [doc], a bounded internal wait the caller doesn't choose. D5 accepts it in MCP explicitly, or keeps it to the CLI.

**Doc fix that rides along (done 2026-11-02):** DESIGN.md §7 said "`sp_helptrigger` and other `sp_help*`", but the code lists exact names: `sp_helprotect` passes (A25) and `sp_help_revlogin` (A24), `sp_helplogins` (F11) and `sp_helpsrvrolemember` (J16) are refused. §7 now lists the names.

### 6.2 Exact-shape reads that run alone outside the wrapper (D5)

Some pure reads fail inside a user transaction, so the wrapper can only break them. When one is the whole input (one batch, one statement; comments and a trailing `;` allowed), sqlh runs it without the wrapper: the session setup without `BEGIN TRANSACTION` (`run::session_setup` already takes `wrap: bool` [src]). Nothing else runs in that run or call, so nothing else needs the backstop. The classifier still runs. Permissions are the boundary.

| Shape (exactly) | Why outside | Permission |
|---|---|---|
| `RESTORE HEADERONLY` \| `FILELISTONLY` \| `LABELONLY` `FROM DISK = N'<local literal>'` [`, DISK = N'<local literal>'` ...] [`WITH FILE = <int>`] | Msg 3021 inside a transaction [live: sys.messages; inf for these forms] | `CREATE DATABASE` [doc] |
| `KILL <integer literal> WITH STATUSONLY` | Msg 6115 for `KILL` [live: sys.messages; inf for this form] | `ALTER ANY CONNECTION` [doc]; it ends nothing [doc] |

- **Local literal:** after trimming leading whitespace, its first two characters are not both separators (each `\` or `/`: Windows takes either, so `\\host`, `//host`, `/\host` and `\/host` all name a share [mem], and `\\?\` and `\\.\` are covered too), it doesn't start with `\??\`, and it doesn't contain `://`. A share or URL falls to `remote` (D4b); `--allow remote` then runs the shape alone and without the wrapper, as here (§5.5's general rule).
- **Left out:** `RESTORE VERIFYONLY` reads the whole backup (cost; `maintenance`). Any other `RESTORE` option, a variable path, or a second statement makes the input an ordinary run, where `RESTORE` is refused as today.
- **In `sqlh mcp`:** allowed when the call's `sql` is exactly one shape. The call leaves no session state [inf]: no `SET`, no temp table, no transaction.
- **Until the live checks** (§10) confirm 3021 and 6115 inside the wrapper and that the shapes work outside it, the class isn't built.

### 6.3 Scratch additions and `sqlh mcp`'s session-state rule (D5, D9(e))

**Scratch additions** (they write only session-private objects, which the wrapper's rollback drops):
- `UPDATE STATISTICS #t` (E7), `CREATE STATISTICS ... ON #t` (E8), `ALTER INDEX ... ON #t` (E9), ‡`MERGE INTO #t` (H12; `MERGE` is a blocked word today).

**MCP admits session state only in three cases** (decision 15: calls are stateless only as far as the reset and the setup clear a session; state outside mcp-mode.md §5's verified list could reach the next call on that connection):
- **(a) The reset is shown to clear it:** `ROWCOUNT`, `TEXTSIZE`, `LANGUAGE`, `DATEFORMAT`, `DATEFIRST`, `CONTEXT_INFO`, a global cursor, `NOCOUNT`, `IMPLICIT_TRANSACTIONS`, `ARITHIGNORE`, the database [live, 5.4]; `STATISTICS IO`/`TIME`/`XML`, `FORCEPLAN`, `CURSOR_CLOSE_ON_COMMIT`, `ANSI_NULL_DFLT_OFF`, `QUERY_GOVERNOR_COST_LIMIT` [live, by hand]; the setup's own options and the isolation level, which the setup sets on every call.
- **(b) Its leftover could only add output:** `STATISTICS PROFILE` [inf].
- **(c) sqlh drops or cleans the connection after it:** `SHOWPLAN_XML`, `FMTONLY`, `PARSEONLY` [live, by hand]; `SHOWPLAN_ALL`/`TEXT` [inf]. §6.8's hygiene turns these into case (a)-like cleanups that keep the connection.

**`#temp` tables** are dropped by the call's own wrapper rollback [live, 5.4]. The reset's own drop of one is checked only at the driver level (`db::spike::spike_reset`), so a run without the wrapper (§6.2's exact shapes; Part 2's kinds that drop it) relies on that unexercised path.

**The holes D9(e) closes** (built 2026-11-01, DESIGN.md §7; until then the guard checked no `SET` and no `OPEN`, so all of these passed in MCP): `SET AUTOPILOT ON` (H6; its leftover would compile the next call's statements instead of running them [mem]; by hand, the next call on `--pool 1` returned its rows on a new login, mcp-mode.md §5, which isn't the reset test), `OPEN SYMMETRIC KEY` (F15, I23), `OPEN MASTER KEY` (F16), ‡`SET IDENTITY_INSERT` (H7; the `#temp` it names is gone at the call's end, so probably harmless [inf]). D9(e) refuses them in `sqlh mcp` until a reset test covers them (§10), with `... leaves session state sqlh mcp's reset isn't known to clear; nothing was run. Run it with the sqlh CLI, whose session ends with the run`. The CLI keeps them. The cost: until the test passes, MCP loses the `OPEN SYMMETRIC KEY`/`OPEN MASTER KEY` reads with `DECRYPTBYKEY` (F15, I23 pass today) and `SET IDENTITY_INSERT #t`.

### 6.4 Contention (D3)

**Refused in read-only runs and `sqlh mcp`; lifted by `data`.**
- **Words anywhere** outside strings, comments and quoted names: `UPDLOCK`, `XLOCK`, `TABLOCKX`, `HOLDLOCK`, `SERIALIZABLE`, `REPEATABLEREAD`; and `REPEATABLE READ` after `ISOLATION LEVEL`. `SERIALIZABLE` and `REPEATABLEREAD` cover both the hint and the isolation level. An unbracketed column named like one (I22) gets today's `[Name]` advice.
- **`SET DEADLOCK_PRIORITY`** with anything but `LOW` or an integer literal from −10 to −5: `NORMAL`, `HIGH`, −4 to 10, a variable (B20, B21).
- **`SET LOCK_TIMEOUT`, folded in whole, with an absolute cap.** Allowed only as an integer literal from 0 to the run's own lock timeout in milliseconds (decision 7's 10,000 by default, or `--lock-timeout`/the profile's `lock_timeout`). When the run has none (`--lock-timeout 0`), the cap is 10,000. Never −1, never a variable (B22–B24). A value in range can only make sqlh give up sooner.
- **`WAITFOR`** in every form (refused today), `sp_getapplock` and `sp_releaseapplock` (refused today as unknown procedures), and `sp_server_diagnostics` with a repeat interval (§6.1).

**Not contention:** `TABLOCK` alone (under READ COMMITTED a table lock is released when the statement finishes [doc]); `NOLOCK` and `READUNCOMMITTED` (D9(a)'s note); `READPAST`, `ROWLOCK`, `PAGLOCK`, `NOWAIT`, `READCOMMITTEDLOCK`; the `SNAPSHOT` table hint (J27; memory-optimized tables).

**Why refuse rather than note.** `lock_or_wait` is the one risk the wrapper makes worse (§4), blocking already caused can't be undone, and the in-SQL `SET`s override decision 7 from inside the input. The rare legitimate SERIALIZABLE read gives way.

**Advice.** It names what the caller can change:
- CLI: `-- blocked: line 1: WITH (UPDLOCK) holds its locks until the run ends, so other sessions wait on it; nothing was run. Remove the hint: a read under READ COMMITTED releases its locks as it goes`.
- Lock timeout, CLI: `... SET LOCK_TIMEOUT 30000 would wait longer than the run's lock timeout (10 s); nothing was run. Rerun with --lock-timeout 30`.
- Lock timeout, MCP: `...; nothing was run. The profile's lock_timeout sets how long a call waits for locks`. MCP calls can't set it (mcp-mode.md §4's arguments), and a call argument capped at the profile's value could only lower it, which the in-SQL literal already does.

**SNAPSHOT (open, D3 sub-question).** The non-blocking consistent read a DBA picks on a hot OLTP database. A transaction can't switch to SNAPSHOT after it started under another level; it starts at its first data access [doc]. Whether an in-SQL `SET TRANSACTION ISOLATION LEVEL SNAPSHOT` as the run's first statement works inside the wrapper is untested: the test database doesn't allow snapshot isolation. Today it passes (H10) and at worst aborts the wrapper, which stops the run (DESIGN.md §7's invariant). Proposal: a run setting like decision 7's READ UNCOMMITTED, `--snapshot` and a profile's `snapshot = true`, set in the session setup before `BEGIN TRANSACTION`. It needs `ALLOW_SNAPSHOT_ISOLATION ON` on the database [doc] and is exclusive with `read_uncommitted`. Once it exists, an in-SQL SNAPSHOT is refused with advice naming it. Pending the live test (§10), which needs the owner's approval.

### 6.5 Remote reach (D4)

**D4a: four-part reads through a linked server a DBA configured.** `SELECT ... FROM Lnk.db.sys.x`, `Lnk..sys.databases` (E21–E23 pass today). The linked server's login mapping is a DBA's grant, so under "permissions are the boundary" the natural default is to allow them, with a note: `-- note: line 3 reads through linked server Lnk; the rollback doesn't reach it, and its login mapping decides what it may read`. Cross-replica AG health checks are routine incident reads. **This depends on a fact nobody has measured:** does a four-part read inside the wrapper run, promote the transaction to MSDTC, or fail without it [mem]? If it fails, allowing it changes nothing in practice; if it promotes, a DTC transaction per run is the cost to weigh. The test comes first (§10).
- Unchanged either way: `OPENQUERY`, `OPENROWSET`, `OPENDATASOURCE` and `EXEC ... AT` stay refused (F28, F29): their text runs on the other server as written, unvetted. Four-part procedure names stay refused by the qualifier rule (F30). Four-part writes are `data` and `remote`.
- Deprecated four-part column references (`SELECT Sales.dbo.Orders.OrderId`, E24) are four dotted parts not followed by `(`. If D4a refuses four-part names, they are refused with the advice `use a table alias`. XML method calls (`qp.query_plan.nodes(...)`, Q25) are followed by `(` and don't count.

**D4b: paths an agent chose.** A UNC path makes the server's service account open a connection to that host and authenticate to it [inf]; an agent misled by text it read can name any host.
- **The functions and their path arguments:** `fn_xe_file_target_read_file` (1 `path`, 2 `mdpath`, 3 `initial_file_name`, which Learn's example gives as a full URL [doc]), `fn_get_audit_file` (1 `file_pattern`, 2 `initial_file_name`, "the path and name of a specific file" [doc]), `fn_trace_gettable` (1), `dm_os_enumerate_filesystem` (1), `dm_os_file_exists` (1). A path argument passes when it is `NULL`, `DEFAULT`, or a local literal (§6.2's test). A UNC path, a URL, a device path, a variable, an expression or a concatenation is `remote` (C12–C14, F19, F20, I3, J26, B35, J46, J47 pass today). And every string literal in any argument of these functions gets the local test too, whatever its position, so a path argument the list misses still fails closed.
- **Every spelling of the name:** each rule matches the function by its name's last part, however it is written: bare (`fn_trace_gettable`), with the deprecated `::` prefix, `sys.`, `master.sys.`, `master..`, and bracketed (`[sys].[fn_get_audit_file]`). All of these pass today (J39–J43 [probe]); Learn's own `fn_trace_gettable` example is unqualified [mem].
- **Fail closed for the rest:** any other function whose name's last part starts with `fn_` or `dm_`, in any of those spellings, with a string-literal argument that fails the local test is `remote`. `fn_dump_dblog` (E28, and bare, J43) is undocumented and on no list, so it is `unvetted` in every spelling, and its UNC literal makes it `remote` too. The prefix rule also catches a user function so named (G13's `dbo.fn_x`) given such a literal: rare, and it fails closed. A variable path in a function sqlh doesn't know can't be seen; documented.
- **Teach the local forms:** deadlock history reads `sys.fn_xe_file_target_read_file(N'system_health*.xel', NULL, NULL, NULL)`, which a relative name resolves to the default log folder [doc] (B34; on 2022+ it needs `VIEW SERVER PERFORMANCE STATE` [doc]).
- **Two-step advice for a variable path:** `-- blocked: line 3: sys.fn_get_audit_file(@p, ...) takes a path sqlh can't check; nothing was run. Read the path first, then pass it as a literal`. An audit or XE file on a share the server is configured to write stays refused: a later sub-option (D4b(ii)) is a sqlh-owned read that takes the path from `sys.server_file_audits` or `sys.dm_xe_session_targets` itself.
- **What the test can't see:** mapped and `SUBST` drives, junctions, and a relative base on a share (§2). What it does catch beyond `\\host`: leading whitespace is trimmed first, and any two leading separators in either direction (`//`, `/\`, `\/`; J44, J45 pass today), `\\?\` and `\??\` count as non-local.
- **Tests to name:** the guard's table-driven tests cover each listed function in each spelling (J39–J43), a path in each listed path argument (J46, J47), and both mixed-separator forms (J44, J45), each refused as `remote`; the same functions with `NULL`, `DEFAULT` and a local literal still pass (B34, J25).
- **Restore-file checks from a share** (P9's main case: another server's backup, usually on a share): `RESTORE HEADERONLY|FILELISTONLY|LABELONLY` with a non-local path is `remote`, so read-only runs and MCP refuse it; `--allow remote` runs it alone and without the wrapper (§5.5's general rule, §6.2).

**Also `remote`:** `xp_logininfo` (F12), `sp_validatelogins`, `sp_testlinkedserver` (F17), `xp_dirtree`, `xp_subdirs`, `xp_fileexist`. Reads of `sys.servers` (F26) and `sys.linked_logins` stay reads; so does `xp_instance_regread`, which reads the local host (the owner asked for it, PLAN.md Phase 6, the interim step).

### 6.6 Waits: a client-side pause (D6)

**The mechanism.** A line `:pause N` between batches, in the CLI's input and in an MCP call's `sql`. N is whole seconds from 1 to the cap. Like `GO`, the line ends the batch before it. sqlh sends nothing for N seconds, then runs the next batch on the same connection.
- **Why it fits:** the batches already share the session and its `#temp` tables within a run or a call, so the agent snapshots into `#w1`, pauses, and computes the delta in SQL, for any DMV: waits, latches, spinlocks, `sys.dm_exec_query_stats` (per query), `sys.dm_exec_requests`/`sys.dm_exec_sessions` (per session), `sys.dm_db_index_operational_stats`, tempdb usage. No fixed menu to grow.
- **No server worker, no server-side wait:** a `WAITFOR` holds a worker for its duration [mem]; a client-side pause holds nothing on the server but the idle session. `WAITFOR` stays refused in every read-only run and every `sqlh mcp` call; only `--allow data` or `--allow-write` runs it (W11 [probe] for `--allow-write`). So MCP's rule is untouched (§6.11). Cancellation during a pause is immediate: no request is outstanding.
- **Bounds:** each pause and the sum of a run's (or call's) pauses at most the cap (proposed 60 s, the owner's number), and at most `-t` when `-t` isn't 0; in MCP, at most the call's timeout. A pause past a bound is a usage error before anything runs (exit 3). `--check` skips pauses.
- **What the open wrapper holds meanwhile:** with D3, no lock-holding hint or isolation can be in the input, and under READ COMMITTED row and page locks are released as the scan moves on and table locks at the end of the statement [doc]. So the session holds its `#temp` objects and an idle transaction. That transaction can hold back tempdb log truncation for the pause, and perhaps version-store cleanup where row versioning is on [inf]. The cap bounds both. **D6 is conditional on D3:** without it a pause would extend held locks.
- **Output:** one line `-- paused 10 s` where the pause was, so the transcript shows the interval. The agent should still take `ms_ticks` from `sys.dm_os_sys_info` in each snapshot for an exact interval.
- **Today** `:pause 10` passes the guard and fails on the server as a syntax error (I1), so no working input changes meaning. sqlcmd has no `:pause`; sqlh's other sqlcmd `:` commands remain non-goals (DESIGN.md §1).
- **Later, on top:** presets (`sqlh sample waits|io|counters --seconds N`, and an MCP `sample` tool) written as sqlh's own SQL around the same pause, which also do the arithmetic agents get wrong (agent trial 2).
- **Alternative:** a `pause_seconds` MCP argument that splits a call into before and after parts. Rejected: the directive serves the CLI and MCP with one rule.

### 6.7 sqlh-owned reads: `activity`, `errorlog`, `info` (D7)

All are sqlh's own SQL (exempt from the classifier, always read-only, inside the wrapper), with CLI commands and MCP tools, like the schema commands. MCP then has a substitute for `sp_WhoIsActive` whatever D14 decides.

**`activity`** (`sqlh activity [--plans]`; MCP `activity`, `plans?`):
- One row per user session that is running, blocked, blocking, or sleeping with an open transaction: session id, blocking session, head-blocker flag and chain depth, status, wait type, time and resource, CPU, reads, elapsed, open transaction count and start, login, host, `program_name`, database, command, statement text (cut by `max_cell`), and the input buffer for head blockers and sleeping sessions with an open transaction (`sys.dm_exec_input_buffer`). `--plans` adds the plan, cut like any value.
- A head blocker blocks others and is not blocked, or is blocked only by a session with no request.
- **Excludes only the report's own `@@SPID` and, in `sqlh mcp`, that server's pooled connections that have no row in `sys.dm_exec_requests`** (idle; an idle pooled session holds no transaction, §3.2, F1). A busy pooled session, such as a parallel call's long read whose Sch-S lock blocks an index rebuild, shows, marked as this server's own (it notes each pooled connection's session id when it opens it), with `program_name` `sqlh mcp` (D9(c)). Other sqlh sessions show, with their `program_name` (D9(c) makes a write run stand out).
- **Says what the login can't see:** it checks `HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER PERFORMANCE STATE')` (2022+; `VIEW SERVER STATE` before) and, without it, notes `-- note: this login sees only its own session (it lacks VIEW SERVER PERFORMANCE STATE)`.

**`errorlog`** (`sqlh errorlog [--since TIME] [--search TEXT] [--agent] [--archive N]`; MCP `errorlog`, `since?`, `search?`, `log?` (`server`/`agent`), `archive?`):
- Newest first, so `max_rows` shows the end of the log that matters.
- When the login can run `xp_readerrorlog` (sysadmin or an explicit grant [live]), sqlh calls it with the window and `N'desc'` (server-side filter). Otherwise `INSERT INTO #log EXEC sys.sp_readerrorlog @archive, @type, @search` then `SELECT ... WHERE LogDate >= @since ORDER BY LogDate DESC`. That this works inside the wrapper is [inf]: the live test ran the `xp_` form inside `INSERT ... EXEC` as the test profile [live], not the `sp_` form or a least-privilege login (§3.2, F5; §10 item 10). The second form reads the whole archive into tempdb, bounded by `-t`.
- Whether `HAS_PERMS_BY_NAME` answers for `master.sys.xp_readerrorlog` is unverified (§10).
- The `INSERT ... EXEC` recipe also goes into DESIGN.md §7 and the skill (Phase 8) whatever D7 decides.

**`info` reports the login's view permissions:** `VIEW SERVER PERFORMANCE STATE`, `VIEW SERVER STATE`, `VIEW ANY ERROR LOG`, `VIEW ANY DEFINITION`, `VIEW SERVER SECURITY STATE`, and sysadmin, one line. Agents then know up front which reads will fail or return nothing (§3.2, F1 and F2). For the candidate SQL Server 2016 floor, `VIEW SERVER STATE` stands in for the 2022 permissions.

**Cost:** new code and output shapes (golden tests, DESIGN.md §5 and §9, README's "Built for agents"); MCP tools count against decision 20's 350 KiB gate, and the exe against its 3 MB budget (337,408 bytes left at `f905385`).

### 6.8 Showplan hygiene (D8)

- **OFF before the rollback.** When the input mentions `SHOWPLAN_XML`, `SHOWPLAN_ALL`, `SHOWPLAN_TEXT`, `FMTONLY`, `PARSEONLY` or (outside `--check`) `NOEXEC`, sqlh sends the matching `SET ... OFF` before its closing `ROLLBACK`, each `SHOWPLAN_*` as a request of its own (it must be alone in its batch [doc]). Then the `ROLLBACK` runs, and `sqlh mcp` keeps the connection instead of dropping it (§3.2, F3). Sending `OFF` for an option that is already off is harmless. That `SET PARSEONLY OFF` takes effect under `PARSEONLY ON` is [inf]; the live test checks the same SPID survives (§10).
- **Refuse SHOWPLAN not alone in its batch:** a batch that holds `SET SHOWPLAN_*` and any other statement (Q6) is refused locally: `-- blocked: line 1: SET SHOWPLAN_XML must be alone in its batch; nothing was run. Put GO after SET SHOWPLAN_XML ON`. The server would refuse it anyway [doc].
- The plan modes and a summary are §8, after classifier v2.

### 6.9 Refusal wording (D2)

**Shape, unchanged:** `-- blocked: line L: <construct> <reason>; nothing was run. <advice>`. **Rule:** every refusal names its kind or label in the reason, and the advice names the switch that lifts it (the narrowest one), or says that none does. An alternative comes first where there is one (DESIGN.md §7's rule). In a `deny` profile the advice says what the profile doesn't allow; in `sqlh mcp`, `sqlh mcp is read-only` (until the `execute` tool exists for that profile and kind).

**Now** (with or after the interim step; independent of D1):
- **Unknown procedure,** the case PLAN.md asks for:
  - `-- blocked: line 1: EXEC sp_WhoIsActive isn't on sqlh's list of read-only procedures, so sqlh can't tell what it does; nothing was run. Rerun with --allow-write to run it as written`
  - implicit: `-- blocked: line 1: sp_WhoIsActive starts the batch, so SQL Server would run it as a stored procedure (a typo?); it isn't on sqlh's list of read-only procedures; nothing was run. Fix it if it's a typo, or rerun with --allow-write to run it as written`
- **Unknown DBCC,** with the documented replacement: `-- blocked: line 1: DBCC PAGE isn't on sqlh's list of read-only DBCC commands; nothing was run. Read sys.dm_db_page_info instead, or rerun with --allow-write`. Replacements: `LOGINFO` → `sys.dm_db_log_info`; `DBINFO` → `DATABASEPROPERTYEX(name, 'LastGoodCheckDbTime')`; `SHOWCONTIG` → `sys.dm_db_index_physical_stats`; `PAGE` → `sys.dm_db_page_info`; `IND` → `sys.dm_db_database_page_allocations`; `PROCCACHE` → `sys.dm_exec_cached_plans`; `SHOWFILESTATS` → `sys.dm_db_file_space_usage` (its `total_page_count / 8` and `allocated_extent_page_count / 8` matched the command's `TotalExtents` and `UsedExtents` on 2016 SP3 and 2025, 2026-11-01). **`sys.dm_db_page_info` is SQL Server 2019's:** it is absent on 2016 SP3, which decision 49 supports, and the guard runs before connecting, so the refusal says so (`Read sys.dm_db_page_info (SQL Server 2019 and later) instead, ...`). The others are present on both 2016 SP3 and 2025 (checked live, 2026-11-01); `sys.dm_db_database_page_allocations` is undocumented, unlike the rest, and the refusal says that too (`Read sys.dm_db_database_page_allocations (undocumented) instead, ...`).
- **Named statements:** DDL refusals name the statement, not a bare `ALTER` or `CREATE`: `ALTER EVENT SESSION`, `ALTER RESOURCE GOVERNOR`, `ALTER DATABASE SCOPED CONFIGURATION`, `ALTER DATABASE`, `ALTER INDEX`, `ALTER AVAILABILITY GROUP`, `CREATE STATISTICS`, `CREATE EVENT SESSION` (fourteen probes say only `ALTER would write` or `CREATE would write`: C6, Q15, Q16, Q22, Q27, E8, E9, E25, F10, F22, F23, I9, J12, J34).
- **`sp_query_store_flush_db`** gets the aggregation advice (§3.2, F3) in place of a flag, and a reason that says what it does, since sqlh knows: `EXEC sys.sp_query_store_flush_db isn't on sqlh's list of read-only procedures: it writes Query Store's in-memory data to disk; nothing was run. Read sys.query_store_runtime_stats instead: it has the rows not yet flushed too; aggregate by plan_id, execution_type and runtime_stats_interval_id`.

**With v2:** the kind and its switch. Examples (CLI under `flag`; MCP; a `deny` profile `P` without the kind listed):

| Kind or label | Reason after the construct | CLI advice | MCP / `deny` |
|---|---|---|---|
| `unvetted` (procedure) | `isn't on sqlh's list of read-only procedures (unvetted code)` | `Rerun with --allow unvetted to run it in the rolled-back transaction` | `sqlh mcp is read-only` / `Profile P doesn't allow unvetted code` |
| `unvetted` (dynamic SQL) | `runs text sqlh can't classify (unvetted code)` | `Pass the text as one literal (EXEC sp_executesql N'...'), or rerun with --allow unvetted` | the alternative; `sqlh mcp is read-only` |
| `data` | `changes data` / `changes the schema` | `Use a #temp table, or rerun with --allow data if the change is intended` | `Use a #temp table; sqlh mcp is read-only` / `...; profile P doesn't allow data changes` |
| contention | `holds its locks until the run ends` / `would make other sessions the deadlock victim` / `would wait longer than the run's lock timeout` / `waits on the server` | the alternative (§6.4, §6.6); `--allow data` only where the lock is the intent | the alternative naming the profile's `lock_timeout` |
| `maintenance` | `is maintenance, which a rollback doesn't undo` | `Rerun with --allow maintenance` | `sqlh mcp is read-only` / `Profile P doesn't allow maintenance` |
| `tuning` | `changes how SQL Server runs this query for every session (tuning)` | `Rerun with --allow tuning` | likewise |
| `kill` | `ends another session's work, which can't be undone` | `Rerun with --allow kill`; for progress, `KILL n WITH STATUSONLY` alone | likewise |
| `remote` | `reaches linked server Lnk, outside what the rollback covers` / `makes the server open a path on another host` | the local form first (§6.5), then `--allow remote` | likewise |
| other changes | `changes a server setting` / `changes permissions` / `starts an Agent job` / ... | `Rerun with --allow-write if the change is intended` | `sqlh mcp is read-only` / `Profile P doesn't allow changes (write_policy = "deny")` |
| cost | `would read every page of every index in every database` | `Name the database and object, or use 'LIMITED'` | the same |
| MCP session state | `leaves session state sqlh mcp's reset isn't known to clear` | n/a (the CLI runs it) | `Run it with the sqlh CLI, whose session ends with the run` |
| deny tier | `is on sqlh's deny list (<why>)` | `No switch runs it; ask a person to run it outside sqlh if it is intended` | the same |

**Contract impact:** DESIGN.md §5.1 and §7, the golden tests, `--help` where it quotes a refusal, README's "Built for agents". JSON carries the same words in `error`; a separate `kind` member would be a contract 2.2 addition (optional, part of D2).

### 6.10 Small items (D9)

- **(a) The NOLOCK note.** When the guard sees `NOLOCK` or `READUNCOMMITTED` as a hint, or `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` (H8, H9 pass today), the run gets decision 7's honesty note, worded for the source: `-- note: some reads were uncommitted (NOLOCK in the SQL): rows may include uncommitted, duplicated or missing data`. Today these bypass decision 7's opt-in and its note. Refusing them would over-block a common DBA habit.
- **(b) Host-disclosure reads stay allowed** and are documented in DESIGN.md §7's notes: registry (`xp_instance_regread`), services (`sys.dm_server_services`), drives, `xp_msver`. `xp_instance_regread` runs with no server permission (`LoginMode` [live]; the service account [live, by hand]); a DBA stops it by revoking `EXECUTE` from public. A profile setting to refuse this label is possible later; no reviewer asked.
- **(c) The application name per run kind:** `sqlh` (CLI read-only), `sqlh mcp` (MCP read tools), `sqlh (writes)` (`--allow-write` or `write_policy = "allow"`), `sqlh (allow: maintenance,kill)` (kind grants, Part 2). A DBA's `sp_who2` or `sp_WhoIsActive` then tells a harmless wrapper from a write run. The name is client-asserted, a courtesy label, not evidence. It is fixed at login, so a pooled MCP connection keeps `sqlh mcp`, and the `execute` tool needs connections of its own (D15).
- **(d) Refuse the all-wildcard SAMPLED/DETAILED call.** `sys.dm_db_index_physical_stats` whose first argument is the literal `NULL`, `DEFAULT`, `0`, `'0'`, `''` or `N''`, or another the servers take the same way, and whose fifth is the literal `'SAMPLED'` or `'DETAILED'`, as a string or in binary (E1; amended by the owner, 2026-11-02, PLAN.md §6, decision 30; DESIGN.md §7 has each) is refused: `-- blocked: line 1: sys.dm_db_index_physical_stats(NULL, ..., 'DETAILED') would read every page of every index in every database; nothing was run. Name the database and object, or use 'LIMITED'`. `'SAMPLED'` doesn't read every page, so its reason says `would read at least 1% of the pages of every index in every database` instead. No switch lifts it in a read-only run; the rewrite does. A run with writes on skips the classifier, so it isn't refused there. A variable mode (E3) can't be seen and is documented, as DESIGN.md §7 does with the rest the text doesn't show. `-t` stops sqlh's wait, not the I/O and buffer-pool churn already caused on a struggling server. Unscoped `DBCC CHECKCONSTRAINTS` is `maintenance` (§6.1), not read.
- **(e) MCP's session-state holes** (§6.3): refuse `SET AUTOPILOT`, `OPEN SYMMETRIC KEY`, `OPEN MASTER KEY` and `SET IDENTITY_INSERT` in `sqlh mcp` until a reset test covers them, with advice to run them with the CLI. The cost: decrypting reads (F15, I23) and `SET IDENTITY_INSERT #t`, which worked in MCP until this, move to the CLI until then. Built 2026-11-01, in both tools that take SQL; `CLOSE` of a key isn't refused, and the spellings the rule sees are DESIGN.md §7's.

### 6.11 What Part 1 keeps

- **plan-history.md 5.4's MCP rule:** `WAITFOR`, app locks and session context stay out of reach of every MCP call. Round 2 adds none of the three; it does add one bounded internal wait, the one-shot `sp_server_diagnostics`'s about 5 s, which D5 accepts for MCP or keeps to the CLI (§6.1). The pause is client-side (§6.6), and D13's literal recursion inherits the rule (§7.6).
- The qualifier rule, the work budget and its "too complex" refusals, `--check`'s NOEXEC rule, `##` refusals, `NEXT VALUE FOR`, the transaction invariant, and the exemption of sqlh's own SQL.

---

## 7. Part 2: changes

The deny tier, the kinds and the grants (D10–D12) and the `execute` tool (D15) need D1(b) first. Three read-side features here use no kind and don't wait for D1, apart from their `--allow data`/`--allow unvetted` mentions, so they can be built with Part 1: D13(b)'s one literal classified under the caller's policy, D13(d)'s per-database execution (§7.6), and D14(b)'s trusted entries in CLI read-only runs and MCP (§7.7).

### 7.1 The deny tier (D10)

A lexical pass that runs in **every** mode, `--allow-write` and kind grants included, before anything connects. Its rules look ahead a bounded number of tokens, so it stays linear and needs no "too complex" refusal, and large write scripts still run. In read-only runs the classifier refuses these already; the deny tier changes their wording to say that no switch lifts them.

**Members:**
- `RESTORE DATABASE` and `RESTORE LOG`, every form (`WITH REPLACE`, `WITH RECOVERY` included). A restore replaces a database; no investigation needs one. W2 passes under `--allow-write` today [probe]. (`RESTORE HEADERONLY`/`FILELISTONLY`/`LABELONLY`/`VERIFYONLY` are not `RESTORE DATABASE`; §6.2.)
- `BACKUP` (any kind: `DATABASE`, `LOG`, `CERTIFICATE`, `MASTER KEY`, `SERVICE MASTER KEY`, `SYMMETRIC KEY`) **to anything but local literals:** a target `TO DISK|TAPE|FILE = <literal>` that fails §6.2's local test, `TO URL`, a variable, or a logical backup device (it could name a share). A backup copies a whole database, or a key, off the server: an exfiltration path (W3 passes under `--allow-write` today [probe]).
- `SHUTDOWN` (W4 [probe]).
- `xp_cmdshell`, and `sp_OACreate`, `sp_OAMethod`, `sp_OAGetProperty`, `sp_OASetProperty`, `sp_OADestroy`, `sp_OAStop`, `sp_OAGetErrorInfo`, under any qualification.
- `DBCC WRITEPAGE`; `DBCC CHECKDB|CHECKTABLE|CHECKALLOC|CHECKFILEGROUP` with `REPAIR_ALLOW_DATA_LOSS`, `REPAIR_REBUILD` or `REPAIR_FAST`.
- `ABORT_AFTER_WAIT = BLOCKERS` anywhere (index DDL, `ALTER TABLE`, `DBCC SHRINKFILE`/`SHRINKDATABASE`): it kills the sessions in its way [doc].
- `ALTER EVENT SESSION` and `DROP EVENT SESSION` of `system_health`, bracketed or not: the server's own deadlock and health history.
- `ALTER DATABASE ... SET QUERY_STORE CLEAR`.
- `sp_query_store_set_hints` whose `@query_hints` (named or second positional) contains `ABORT_QUERY_EXECUTION`, or isn't a literal.
- `EXECUTE AS ... WITH NO REVERT`.

**Recommended additions** (D10(b)): `DROP DATABASE` (J33 is refused read-only today), and `sp_configure` with a value (named, or a second argument, read past a procedure number as today's check reads it) when its option argument is a literal (a string or a bare word, in any case) that is part of `xp_cmdshell`, `Ole Automation Procedures` or `clr enabled`, or holds `%`, `_` or `[`, or is not a literal at all. That is the step that makes the denied commands, or unvetted CLR, runnable for every login. A part of the name is enough because SQL Server "recognizes any unique string that is part of the configuration name" [doc]: `EXEC sp_configure 'cmdshell', 1`, `'Ole Auto', 1` and `'clr en', 1` pass under `--allow-write` today (W8–W10 [probe]). The wildcard characters are refused because the procedure may match the name with `LIKE` [inf], and if `xp_cmdshell` is the only option name with `_` [inf], refusing `_` costs nothing else; §10 item 4 checks both (its source by `OBJECT_DEFINITION`, and `SELECT name FROM sys.configurations WHERE name LIKE N'%[_]%'`). The deny tier's tests name a partial name (`'cmdshell'`), a bare word and a variable. **Candidates the owner may add:** `ALTER DATABASE ... SET OFFLINE|EMERGENCY|SINGLE_USER` (J34), `DBCC SHRINKDATABASE`, the instance-wide cache clears, faked statistics (`UPDATE STATISTICS ... WITH ROWCOUNT =|PAGECOUNT =|STATS_STREAM =`).

**Honest wording.** The deny tier is an accident guard. It stops an agent that reaches for one of these by name. Under `--allow-write` the classifier is off, so dynamic SQL (`EXEC ('xp_cmdshell ''dir''')`, W1; `EXEC sp_executesql N'SHUTDOWN'`, W6), `EXEC @var` (W5), a synonym, or a procedure that wraps one gets past it [probe]. Under `--allow unvetted` the classifier is on but admits dynamic SQL and `EXEC @var`, so the same forms get past it there too (a single literal only until D13(b) classifies it). DESIGN.md §7 is to say so; the login's permissions are the boundary. The refusal claims only that no switch lifts it. Example: `-- blocked: line 1: RESTORE DATABASE is on sqlh's deny list (it replaces a database); nothing was run. No switch runs it; ask a person to run it outside sqlh if it is intended`.

**Reading one literal in every mode** (D10(e)): the deny tier applies D13(b)'s rule itself. One literal of dynamic SQL (`EXEC (N'...')` with no concatenation, `AT` or `AS`; `sp_executesql`'s statement as one literal) is unescaped and read by the deny tier's own rules, in every mode, `--allow-write` included, at linear cost charged to the literal's bytes. It catches W1 and W6. A variable, a concatenation, `EXEC @var`, a synonym and a wrapper still get past it.

**Stronger option, not recommended:** under `--allow-write` and kind grants, also refuse dynamic-SQL forms (`EXEC (...)`, `EXEC @var`, non-literal `sp_executesql`) unless `unvetted` is granted. It would refuse legitimate write scripts that build SQL, and a synonym or wrapper still gets past it.

### 7.2 Grants per kind (D11)

- **The profile's `allow` list** (owner-written; a TOML array of kind names, for example `allow = ["maintenance", "kill"]`). It is honored under every `write_policy`, `deny` included: `deny` keeps meaning "no data or schema writes, no blanket switch".
- **Defaults when the key is absent:** under `flag` and `allow`, every kind (those profiles can already run everything with `--allow-write`, so refusing a narrower switch would push agents to the blanket one); under `deny`, none. An explicit list replaces the default in every policy. A run without a profile is `flag`.
- **Per run:** `--allow KIND[,KIND]` lifts exactly the named kinds for that run, and only kinds in the list; an unlisted kind is a usage error (exit 3): `--allow kill: profile P doesn't allow kill (its allow list is maintenance)`. Each run that kills or rebuilds says so on its command line. `--allow` with `--allow-write` is a usage error. Under `write_policy = "allow"`, a run without `--allow` keeps today's behavior (all writes); a run with `--allow K` gets only K.
- **In `sqlh mcp`:** the read tools ignore the list (decision 16); the `execute` tool offers listed kinds (D15).
- **The wrapper per kind and the rules for a run without it:** §5.5.
- **R19 applies:** `sqlh profiles add` would gain a way to write the list, and an agent that can run `profiles add` unattended can change it, as it can change `write_policy` (DESIGN.md §7). The list is an accident guard like `deny`, not a control over who may change the server. Human-only profile management would be a separate design.
- **Application name:** `sqlh (allow: K,...)` (D9(c)).
- **`--help`:** held to 62 lines by a test; `--allow` needs a line, and the kinds are named by every refusal that one lifts.

### 7.3 What each kind admits

Targets "not `#`" mean anything but a `#temp` table or table variable.

- **`data`:** `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `TRUNCATE TABLE`, `SELECT ... INTO`, `OUTPUT ... INTO` on a target not `#`; `UPDATETEXT`, `WRITETEXT`, `BULK INSERT` (a non-local path also needs `remote`); `CREATE`/`ALTER`/`DROP` of objects not claimed below (tables, views, procedures, functions, triggers, indexes and statistics on non-`#` targets, schemas, synonyms, types, sequences, partition functions and schemes, `CREATE DATABASE`), `ALTER INDEX ... DISABLE`, `ENABLE`/`DISABLE TRIGGER`; `DBCC CHECKIDENT` in any form but `NORESEED`; `DBCC CLONEDATABASE`; `NEXT VALUE FOR`; `##` targets; `BEGIN TRAN`/`TRANSACTION`/`DISTRIBUTED`, `COMMIT`, `ROLLBACK`, `SAVE TRAN`; Service Broker (`SEND`, `RECEIVE`, `BEGIN DIALOG`, `END CONVERSATION`, ‡`MOVE CONVERSATION`, `GET CONVERSATION GROUP`); and contention (§6.4).
- **`maintenance`:** `UPDATE STATISTICS` (not `#`; not the faked-statistics options), `sp_updatestats`, `sp_createstats`, `sp_autostats` with a flag; `ALTER INDEX ... REBUILD|REORGANIZE|PAUSE|RESUME|ABORT` and `ALTER TABLE ... REBUILD` (not `#`; not `BLOCKERS`); `DBCC CHECKDB|CHECKTABLE|CHECKALLOC|CHECKCATALOG|CHECKFILEGROUP` (no repair; `ESTIMATEONLY` included), unscoped `DBCC CHECKCONSTRAINTS`, `DBCC SHRINKFILE|SHRINKDATABASE` (not `BLOCKERS`), `DBCC UPDATEUSAGE`, ‡`DBCC CLEANTABLE`, ‡`INDEXDEFRAG`, ‡`DBREINDEX`; `sp_spaceused` with `@updateusage` true; `sp_estimate_data_compression_savings`; `CHECKPOINT`; `RESTORE VERIFYONLY` from a local path; `BACKUP DATABASE|LOG ... TO DISK = N'<local literal>' ... WITH COPY_ONLY` (D11's sub-choice (i); under (ii) it is "other changes"). A copy-only backup doesn't change the backup sequence [doc]; any other backup breaks the differential base or splits the log chain the DBA's restores rely on, so it is "other changes". Even copy-only, one run writes a full copy of a database to any path that passes the lexical test (a mapped drive, a junction, a relative base on a share; §2), and can fill a volume; no investigation needs it, so `execute`'s first increment leaves it out (§7.8).
- **`tuning`:** §7.4.
- **`kill`:** §7.5.
- **`remote`:** §6.5.
- **`unvetted`:** every procedure or implicit call on no list (user, community and unlisted system procedures, including read-list names under a refused qualifier, and `xp_regread`, `xp_enumerrorlogs`); every DBCC subcommand on no list; `EXEC (...)` without `AT` and `sp_executesql` whose text isn't one literal (every form of both without D13(b)), `EXEC @proc`, `sp_MSforeachdb`, `sp_MSforeachtable`, `sp_ineachdb`; `fn_dump_dblog` in every spelling (§6.5).
- **Other changes** (`--allow-write` only):
  - *security:* `GRANT`, `REVOKE`, `DENY`, `SETUSER`, `EXECUTE AS` as a statement (but D5(e)'s read form); `CREATE`/`ALTER`/`DROP` of logins, users, roles, application roles, certificates, keys, credentials, audits and audit specifications, ‡security policies, ‡database encryption keys; ‡`sp_addlogin`, ‡`sp_droplogin`, ‡`sp_addsrvrolemember`, ‡`sp_dropsrvrolemember`, ‡`sp_addrolemember`, ‡`sp_droprolemember`, ‡`sp_password`; `sp_change_users_login` but `'Report'`; `sp_help_revlogin` (user code that returns password hashes [mem]).
  - *settings:* `sp_configure` with a value, `RECONFIGURE`, `ALTER DATABASE` (but §7.1's), `ALTER DATABASE SCOPED CONFIGURATION SET` and `CLEAR` (but §7.4's one-handle form), ‡`ALTER SERVER CONFIGURATION`, `ALTER RESOURCE GOVERNOR`, ‡resource pools and workload groups, `ALTER AVAILABILITY GROUP`, plan guides (`sp_create_plan_guide`, `_from_handle`, `sp_control_plan_guide`).
  - *capture:* `DBCC TRACEON`/`TRACEOFF` (session-scoped ones too, until a reset test shows the reset clears them), `CREATE`/`ALTER`/`DROP EVENT SESSION` (but `system_health`'s, denied).
  - *resets:* `DBCC FREEPROCCACHE` other than one handle, `FREESYSTEMCACHE`, ‡`FREESESSIONCACHE`, `FLUSHPROCINDB`, `DROPCLEANBUFFERS`, `SQLPERF` but `LOGSPACE`; `ALTER RESOURCE GOVERNOR RESET STATISTICS`; `sp_query_store_remove_plan`, `_remove_query`, `_reset_exec_stats`, `_flush_db`, `_consistency_check`; `sp_cycle_errorlog`, `sp_cycle_agent_errorlog`.
  - *jobs, mail, registry, other:* `msdb.dbo.sp_start_job` and Agent job changes; `sp_send_dbmail`; `xp_instance_regwrite`, `xp_regwrite`, ‡`xp_regdeletevalue`, ‡`xp_regdeletekey`; `KILL` forms other than a session id; local backups that aren't `COPY_ONLY`; faked statistics.

### 7.4 Tuning, apart from settings and resets

The fixes a DBA applies in a plan-regression incident shouldn't share a switch with `RECONFIGURE` or `DROPCLEANBUFFERS`. `tuning` admits, with literal arguments:
- `sys.sp_query_store_force_plan` and `sys.sp_query_store_unforce_plan` (`@query_id`, `@plan_id`; Q17, J10 are refused today).
- `sys.sp_query_store_set_hints` with a literal `@query_hints` that doesn't contain `ABORT_QUERY_EXECUTION` (deny tier), and `sys.sp_query_store_clear_hints`.
- `DBCC FREEPROCCACHE (<one binary literal>)`: one plan or SQL handle (Q20). Bare, or with a resource pool's name, it clears more: resets.
- `ALTER DATABASE SCOPED CONFIGURATION CLEAR PROCEDURE_CACHE <one binary literal>`: one plan, database-scoped permission (Q22). Without a handle it clears the database's cache: resets.
- `sys.sp_recompile N'<one object>'` (Q23).
- Qualification: bare, `sys.`, or `<db>.sys.`, which acts on that database (§3.2, F3) and is explicit.

**Not tuning:** plan guides (persistent, statement-pattern matching: settings); the Query Store removals and `sp_query_store_flush_db` (resets; flush's refusal carries the aggregation advice).

### 7.5 Kill (D12)

- `kill` admits `KILL <integer literal>` (a session id) and `msdb.dbo.sp_stop_job`. Other `KILL` forms (a UOW GUID, `STATS JOB`, `QUERY NOTIFICATION SUBSCRIPTION`) stay other changes. `KILL n WITH STATUSONLY` alone is an exact-shape read (§6.2).
- No wrapper (Msg 6115). Permission: `ALTER ANY CONNECTION` [doc], so permissions remain the boundary as VISION.md intends.
- **Session ids are reused** (§3.2, F1). A later sqlh-built kill checks identity in the same batch: `sqlh kill 53 --login-time 2026-10-23T09:12:03.123` sends `IF EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE session_id = 53 AND login_time = @t) KILL 53; ELSE THROW 50000, N'sqlh: session 53 is not the one named', 1;`. `KILL` takes only a constant [mem], which the `IF` allows. A window of microseconds remains.
- Never through MCP's read tools; through `execute` when listed (D15).

### 7.6 Dynamic SQL and per-database execution (D13)

**(b) Literal recursion.** One string literal of dynamic SQL is classified as its own batch:
- `EXEC sp_executesql N'...'` (the statement named `@stmt` or first positional; its parameter declarations and values can't change the text [inf]) and `EXEC (N'...')` with no concatenation, `AT` or `AS`. J32's parameterized form qualifies.
- After unescaping `''`, under **the caller's whole policy**: the mode (CLI read-only, `sqlh mcp`, or the run's granted kinds), not only "read or write". So `WAITFOR`, app locks and session context stay out of every MCP call however they are wrapped (G7, I4 refused today).
- One level deep: dynamic SQL inside the literal is `unvetted`. The literal's bytes count against the batch's work budget. A refusal inside it is reported at the literal's line.
- **Tests to name:** `guard::tests::literal_dynamic_sql_inherits_the_mcp_policy` with literals holding `WAITFOR DELAY`, `EXEC sp_getapplock` and `EXEC sp_set_session_context`, each refused under the MCP policy; the same three under the CLI read-only policy; and a literal holding `UPDATE dbo.T` refused read-only and allowed under `--allow data`.

**(d) sqlh-owned per-database execution** for the everyday cross-database read (G4):
- CLI `--each-db [PATTERN]`; MCP `query` gains `databases` (an array of names or one glob).
- sqlh lists, in its own SQL, the online databases the login can open (`sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1`, filtered by the pattern), then runs the same classified batches after `USE [db]` for each, on one connection, inside the one wrapper. Nothing dynamic to classify, and it works in MCP.
- **Output:** when every database's result has the same columns, one result set with a leading `database` column (compact, and within `--max-results`); otherwise labeled result sets. An error in one database is labeled and the run goes on to the next; the exit is 1 if any failed. `-t` is per batch per database. A contract change (DESIGN.md §5, golden tests).
- **(c) `sp_MSforeachdb` literals:** dropped. It is undocumented and known to skip databases [mem].

### 7.7 Trusted community procedures (D14)

**The problem.** sqlh can't see a procedure's body. A name trusts whatever is installed under it; anyone with `ALTER` on the schema can change it (an agent too, through an earlier `--allow-write` run); a bare or two-part `sp_` name may resolve in the current database first [mem]; and `is_ms_shipped` proves nothing (`sp_MS_marksystemobject` sets it [mem]).

**An owner-written profile key `trusted_procedures`.** No built-in trust: starter entries go in the docs, since the procedures' parameters change often (FRK monthly, DarlingData's `@log_*` recently) and a stale built-in list would fail open. Each entry has:
- an exact three-part name (`master.dbo.sp_WhoIsActive`); other spellings are refused with advice to use it;
- an allowlist of **named** parameters (positional ones are refused, since their meaning shifts between versions);
- literal caps for parameters that wait (`@delta_interval`, `@Seconds`, `@sample_seconds`), below the run's `-t` or the call's timeout;
- the write parameters refused in every entry, by exact name: `@destination_table`; FRK's `@OutputDatabaseName`, `@OutputSchemaName`, `@OutputTableName`, every `@OutputTableName*` (for example `...WaitStats`, `...BlitzCache`, `...BlitzWho`), `@OutputTableRetentionDays` and `@OutputServerName` [src]; `@LogMessage`, `@log_to_table` and the other `@log_*` (retention means `DELETE`), `@LogToTable`, `@Email*`, `@PushBackupHistoryToListener`, `@WriteBackupsTo*`, `@keep_alive`, `@cleanup`, and for `sp_Blitz` `@CheckServerInfo = 1` (its CheckID 224 runs `xp_cmdshell 'net start'` when it is enabled [src]). Not a prefix `@Output*`: parameter names match case-insensitively under the usual server collations [mem], so it would also refuse `sp_WhoIsActive`'s `@output_column_list`, which only chooses and orders columns (the surveys' skill advice uses it), and FRK's `@OutputType = 'MARKDOWN'`, which picks the output's form [mem];
- optionally, a pinned hash.

**The same-batch check** sqlh sends before the call (for `master.dbo.sp_WhoIsActive`):

```sql
IF OBJECT_ID(N'master.dbo.sp_WhoIsActive', N'P') IS NULL
   OR NOT EXISTS (SELECT 1 FROM master.sys.sql_modules
                  WHERE object_id = OBJECT_ID(N'master.dbo.sp_WhoIsActive'))
   OR EXISTS (SELECT 1 FROM master.sys.sql_modules
              WHERE object_id = OBJECT_ID(N'master.dbo.sp_WhoIsActive')
                AND execute_as_principal_id IS NOT NULL)
   OR EXISTS (SELECT 1 FROM master.sys.crypt_properties
              WHERE class = 1 AND major_id = OBJECT_ID(N'master.dbo.sp_WhoIsActive'))
   -- only with a pinned hash:
   OR ISNULL(HASHBYTES('SHA2_256', OBJECT_DEFINITION(OBJECT_ID(N'master.dbo.sp_WhoIsActive'))), 0x) <> 0x...
    THROW 50000, N'sqlh: master.dbo.sp_WhoIsActive does not match its trusted entry', 1;
EXEC master.dbo.sp_WhoIsActive @find_block_leaders = 1;
```

- **Refuses:** a missing or invisible module (fail closed), `EXECUTE AS` other than CALLER [doc: `execute_as_principal_id` is NULL only for CALLER], and **any signature**. A certificate signature grants the certificate user's permissions without `EXECUTE AS` and without changing the module's text or hash [doc], so the hash alone can't see it; `sys.crypt_properties` can, and is read at every call, so a signature added after pinning is caught.
- **The hash pin is optional.** Without one, no `OBJECT_DEFINITION` is read. With one, the login needs `VIEW DEFINITION` on the module (`OBJECT_DEFINITION` returns NULL without it [mem]), and a NULL fails the check.
- **Visibility is unverified:** whether a login with only `EXECUTE` sees the module's rows in `sys.sql_modules` and `sys.crypt_properties` is [inf]. For `sys.sql_modules` the check fails closed (the `NOT EXISTS` line). For `sys.crypt_properties` it can't: a hidden row looks like no signature. The live test with a `Reader` login (§10) decides whether the check is sound for a least-privilege login; if not, an entry requires `VIEW DEFINITION` on the module.
- **What it can't see:** the check and the call are separate statements, so another session can alter the module between them (the module isn't locked: a time-of-check-to-time-of-use gap). The hash covers the named module only, not the procedures, dynamic SQL or `xp_` it runs.

**Where it runs:** inside the wrapper, in CLI read-only runs and in `sqlh mcp` under the entry's caps, bounded by the call's timeout. In MCP:
- `SET` options a procedure sets, its isolation level included, are restored when it returns [doc]; its `#temp` tables are dropped at its end [doc]; a `##` table it creates inside the wrapper is rolled back with it [inf].
- What can outlive the call on a pooled session: session-owned app locks (`@LockOwner = 'Session'`) and session context set inside it, which belongs to the session, not the module [inf]; and symmetric keys opened inside it, since open keys "are bound to the session not to the security context" and stay open until closed or the session ends [doc]; the master key likewise [inf].
- **Until §10 item 5's reset test passes for all three, `sqlh mcp` closes the connection after every trusted call** instead of returning it to the pool, as §6.3(c) does for `SHOWPLAN_*`. sqlh can't tell which procedures take an app lock, set session context or open a key (it reads no module body, §2), so the rule covers every entry. The cost is one login on the next call (148–185 ms measured, mcp-mode.md §5 [live, by hand]). If the test shows the reset clears all three, trusted calls return to the pool; if not, the close stays.
- Internal waits are bounded by the caps and the call's timeout.

**Never trusted, any mode:** `sp_HumanEvents` (it creates server event sessions), and Ola's `IndexOptimize`, `DatabaseIntegrityCheck` and `DatabaseBackup` (they refuse `@@TRANCOUNT <> 0` [src]; `DatabaseBackup`'s check is unverified, but `BACKUP` can't run in a transaction anyway). Procedures that end the wrapper mid-call (§3.2, F6) can be trusted only knowing that their later statements autocommit.

### 7.8 The `execute` tool (D15)

**Not in Phase 6 (decision 36, D15(a)).** `sqlh mcp` stays read-only; this section is kept as the design for a later phase, after an agent trial with the kinds in the CLI.

- **Offered** only when `sqlh mcp` is launched with `--execute`, and only for listed profiles (decision 18) whose **explicit** `allow` list (not the `flag` default) names at least one kind E allows. In the first increment those are `tuning`, `kill` and `maintenance` (ranks 2–4, §5.4), with `maintenance`'s local `COPY_ONLY` backup left out by default: one call could write a full copy of a database to a path that only looks local, or fill a volume (§7.3), and no investigation needs it. A backup through `execute` would be its own later sub-choice (D15).
- **Arguments:** `sql`, a required `kinds` array, and the usual `profile`, `database`, `timeout`, `format`, `max_rows`, `max_cell`. `kinds` must cover every kind the classifier finds; a mismatch is a tool error that names the kinds found. The harness's approval prompt then shows what the call will change. It informs the prompt; it doesn't enforce anything beyond what the classifier enforces.
- **Runs** like `--allow` with those kinds: classifier on, deny tier on, wrapper per kind (§5.5). No blanket (`--allow-write`) ever.
- **Connections:** not pooled. Each call logs in with the application name `sqlh mcp (allow: K)` and closes after, so nothing a change leaves on a session reaches a read call. A login per call (about 150–200 ms on the dev LAN) is acceptable for rare changes.
- **Annotations:** `readOnlyHint: false`, `destructiveHint: true`.
- **Later (D15(b)):** `data`, `unvetted` and `remote` when listed, after an agent trial with the first increment (Phase 7).

---

## 8. Showplans

**Kinds of plan.**
- **Estimated:** `SET SHOWPLAN_XML|ALL|TEXT ON`, which compiles and never runs. Today it works only as separate batches, `SET SHOWPLAN_XML ON` / `GO` / statement / `GO` / `SET SHOWPLAN_XML OFF`, and only for statements the guard allows (Q2, J20 pass; Q3–Q5 refused) [probe].
- **Actual:** `SET STATISTICS XML|PROFILE ON`; the statement runs inside the wrapper and the plan arrives after its last row (Q7, Q14 pass; Q8, a user procedure, refused) [probe].
- **Cached:** `sys.dm_exec_query_plan`, `sys.dm_exec_text_query_plan` (statement offsets, no 128-level XML limit), `sys.dm_exec_query_plan_stats` (2019+, opt-in), `sys.query_store_plan.query_plan`. All ordinary reads (Q25, J1).

**Problems today.**
1. **Size:** one plan is tens to hundreds of KB. `--max-cell 500` cuts it to a useless prefix; `max_cell 0` lets one call return up to 1,000,000 bytes.
2. **The closing `ROLLBACK` is only compiled** under `SHOWPLAN_*`, `FMTONLY` and `PARSEONLY`: the CLI is safe, `sqlh mcp` drops the connection [live, by hand].
3. **`SET SHOWPLAN_XML ON; SELECT ...` in one batch passes the guard** (Q6) and fails on the server [doc; error 1067, mem].
4. **Estimated plans of writes, procedures and `sp_executesql` are refused** (Q3–Q5), although under SHOWPLAN nothing runs [doc for DML; mem for `EXEC` of a procedure, which needs a live proof].
5. **Actual plans are lost when a count stop or `-t` ends the batch,** and plan result sets count against `--max-results 20`.

**What first-class support means.**
- **(a) Hygiene** (Phase 6, D8(a); §6.8): `OFF` before the rollback; refuse SHOWPLAN not alone in its batch. Fixes problems 2 and 3.
- **(b) A sqlh-owned estimated-plan mode** (`--plan`; an MCP `plan` tool). sqlh arms `SET SHOWPLAN_XML ON` before each batch, as `--check` arms `NOEXEC`, and refuses `SHOWPLAN_*`, `NOEXEC`, `PARSEONLY` and `FMTONLY` in the input. The wrapper stays.
  - Under the mode's own rule, DML, `MERGE`, `NEXT VALUE FOR`, user-procedure `EXEC` and literal `sp_executesql` would be allowed, since nothing executes. That needs a live proof first: a scratch procedure that advances a scratch sequence is a detector the rollback can't hide (§10).
  - Whether a `deny` profile may plan writes is a sub-question; nothing runs, so yes is the natural answer.
- **(c) An actual-plan mode:** `STATISTICS XML` under the ordinary guard; rows drained without being shown, bounded by `-t`, so the plan arrives. Until then the skill teaches `SELECT ... INTO #sink`.
- **(d) A summary** (DESIGN.md §13's M3 `explain`):
  - per operator: NodeId, depth, physical and logical operator, object and index, estimated vs actual rows, executions, cost %, actual elapsed and CPU ms, warnings (spill, implicit conversion, no join predicate, columns with no statistics, excessive grant);
  - per statement: CE model, optimization level and early-abort reason, DOP, memory grant (requested, granted, used), compile time, compiled vs runtime parameter values, missing-index groups with impact, plan-level waits;
  - the full XML stays retrievable through `-o file.sqlplan`;
  - parser options: a bounded, linear XML scanner of sqlh's own (no new crate; must fail closed and fit the 3 MB and 350 KiB budgets); server-side XQuery (only for xml from DMVs or Query Store, costs server CPU, fails at 128 nesting levels, can't reach SHOWPLAN output); or an interim of `SHOWPLAN_ALL`/`STATISTICS PROFILE` tabular output (passes today, compact, but lacks missing indexes, the memory grant, parameter values and newer warnings).

**Phase.** (a) in Phase 6. (b) to (d) are their own task after classifier v2: output-contract work with golden tests, and M3's `explain` (D8).

---

## 9. Decisions for the owner

Each is independent unless it says otherwise. Once decided, each is recorded in PLAN.md §6 with DESIGN.md §7 (and §5 where output changes) updated in the same change.

### The shape (most of Part 2 depends on it; Part 1 doesn't)

**D1. The shape: are kinds of change granted separately, per profile, or does `--allow-write` stay the one switch? And their names.**
- Options:
  - (a) `--allow-write` stays the one switch; add only the deny tier (D10) and Part 1.
  - (b) Two layers (§5): six kinds granted per profile and used per run, `--allow-write` kept as the blanket switch for everything outside the deny tier.
  - (c) Two layers, and retire `--allow-write`: every change names its kinds, and "other changes" get kinds of their own.
- Names: `data`, `maintenance`, `tuning`, `kill`, `remote`, `unvetted`. Alternatives: `write` or `writes` for `data` (rejected: `--allow write` next to `--allow-write` reads as the same switch while meaning much less); `procedures` or `code` for `unvetted`; `linked` for `remote` (too narrow: paths too). A seventh kind, `settings`, is not recommended in Phase 6.
- Trade-offs: (a) leaves a `deny` profile (every new profile, decision 12) no way to `UPDATE STATISTICS`, force a plan or `KILL` short of moving it to `flag`, which opens DML too. (b) adds a profile key, a flag and refusal wording, and `--help` must give up a line elsewhere. (c) breaks existing scripts and agents' habits for little gain over (b).
- **Recommendation: (b), with these names.** D10–D12 and D15 depend on it; Part 1, D13(b), D13(d) and D14(b) don't.
- **Decided 2026-10-23: (b), with these names** (PLAN.md §6, decision 22).

### Part 1: read-only runs and `sqlh mcp`

**D2. Refusal wording (§6.9): an output-contract change.**
- Options:
  - (a) Now: the unknown-procedure and unknown-DBCC texts, named statements, the flush advice. With v2: every refusal names its kind or label and the switch that lifts it, or says none does.
  - (b) Only the unknown-procedure text, as PLAN.md's interim step asks.
  - (c) Keep "would write".
- Trade-offs: (a) changes golden tests twice. (c) keeps steering agents to `--allow-write` for reads, which PLAN.md already calls wrong. Optional within (a): a JSON `kind` member (contract 2.2).
- **Recommendation: (a)**, without the JSON member until a consumer needs it.
- **Decided 2026-10-23: (a), without the JSON member** (PLAN.md §6, decision 23).

**D3. Contention in read-only runs and `sqlh mcp` (§6.4).**
- Options:
  - (a) Refuse every member: the lock-holding hints, REPEATABLE READ and SERIALIZABLE, `DEADLOCK_PRIORITY` above LOW, and `LOCK_TIMEOUT` outside 0 to the run's own (capped at 10,000 ms when the run has none, never −1 or a variable); MCP advice names the profile's `lock_timeout`.
  - (b) Refuse only the write-intent hints (`UPDLOCK`, `XLOCK`, `TABLOCKX`) and `DEADLOCK_PRIORITY` above LOW; note the rest.
  - (c) Keep passing them, with a note.
- Sub-question, SNAPSHOT: (i) a run setting (`--snapshot`, `snapshot = true`) like decision 7's, after the live test, and then refuse an in-SQL SNAPSHOT with advice naming it; (ii) leave it as today.
- Trade-offs: (a) over-blocks the rare legitimate SERIALIZABLE read. (c) leaves the one risk the wrapper makes worse, and lets in-SQL `SET`s override decision 7. The cap's 10 s is decision 7's default; the owner may pick another number.
- **Recommendation: (a), and SNAPSHOT (i)** once the live test passes (it needs the owner's approval, §10).
- **Decided 2026-10-23: (a), and SNAPSHOT (i) after its live test** (PLAN.md §6, decision 24).

**D4. Remote reach (§6.5).**
- **D4a, four-part reads through a DBA-configured linked server:**
  - (i) allow, with a note naming the linked server;
  - (ii) refuse, lifted by `--allow remote` (Part 2).
  - Trade-offs: (i) follows "permissions are the boundary" (the mapping is a DBA's grant) and keeps cross-replica health checks; the rollback can't reach the other side, and the mapping may exceed the profile's rights. Depends on the MSDTC test: if four-part reads fail inside the wrapper, (i) changes nothing in practice; if they promote, each such run costs a DTC transaction.
  - **Recommendation: (i)**, decided after the MSDTC test.
- **D4b, paths an agent chose:**
  - (i) refuse UNC, URL, device, variable and expression paths in the listed functions' path arguments, and non-local literals in any of their arguments and in any other `fn_`/`dm_` function, every spelling of the name matched (bare, `::`, `sys.`, `master.sys.`, `master..`, bracketed); `fn_dump_dblog` is `unvetted`; teach `system_health*.xel`; two-step advice for a variable;
  - (ii) (i), plus later a sqlh-owned read for audit and XE files on a share the server is configured to write;
  - (iii) allow all, documented.
  - Trade-offs: (i) newly refuses the textbook variable forms of deadlock-history and audit reads (B35, F20), which pass today; the relative `system_health*.xel` form covers deadlock history. It also makes `RESTORE HEADERONLY|FILELISTONLY|LABELONLY` from a share `remote`: refused in read-only runs and MCP, run alone and without the wrapper under `--allow remote` (§5.5, §6.5). That is P9's main case, another server's backup file before a restore, so with (i) it needs `--allow remote` (Part 2) or a local copy of the file. (iii) lets a misled agent make the service account authenticate to any host.
  - **Recommendation: (i) now, (ii) if audits on shares prove common.**
- **Decided 2026-10-23: D4a (i), final once the MSDTC question is answered (not yet tested), and D4b (i)** (PLAN.md §6, decision 25).

**D5. Read list round 2 and the reads that need a special path (§6.1–§6.3).**
- Parts:
  - (a) round 2's table, the msdb rule, the scratch additions (`#temp` statistics and indexes, `MERGE INTO #t`); sub-choice for the one-shot `sp_server_diagnostics`, which runs about 5 s on the server (§6.1): (a1) allowed in MCP too, a bounded internal wait inside the call's timeout, or (a2) CLI only;
  - (b) `CHECKDB ... ESTIMATEONLY` and `sp_estimate_data_compression_savings` stay `maintenance`;
  - (c) `DBCC MEMORYSTATUS` on the list, documented as many result sets, with the memory views taught first;
  - (d) the exact-shape class, alone and outside the wrapper: `RESTORE HEADERONLY|FILELISTONLY|LABELONLY FROM DISK = N'<local literal>'` and `KILL <int> WITH STATUSONLY`;
  - (e) "can user X read this": `EXECUTE AS USER|LOGIN = N'<literal>'` (no `NO REVERT`, no `COOKIE`) in CLI read-only runs, inside the wrapper with the classifier on; refused in MCP until a reset test shows the reset reverts it. The `IMPERSONATE` permission is the boundary; the reads run with the impersonated principal's rights, which can be higher than the caller's. Alternative: a sqlh-owned `perms PRINCIPAL [OBJECT]` command.
- Options: all of (a)–(e); (a)–(c) only; none until v2.
- Trade-offs: every entry needs a live check in the wrapper (§10); (a1) puts a 5 s server-side wait the caller doesn't choose into MCP, where `WAITFOR` stays refused, while (a2) leaves MCP's health check to the DMVs; (d) adds a run mode without the wrapper, narrow and exact; (e) is a common question with no other path.
- **Recommendation: all of (a)–(e)**, each entry gated on its live check, with (a1) once §10 item 4 has measured the one-shot call's time; (e) in the CLI first.
- **Decided 2026-10-23: all of (a)–(e), with (a1)**, each entry gated on its live check (PLAN.md §6, decision 26).

**D6. Waits (§6.6). Conditional on D3(a).**
- Options:
  - (a) a `:pause N` line between batches, in the CLI and in MCP's `sql`; client-side; each pause and their sum capped (60 s proposed), and within `-t` or the call's timeout; a `-- paused N s` line; presets later;
  - (b) allow `WAITFOR DELAY '<literal>'` in CLI read-only runs under a cap; still refused in MCP;
  - (c) keep refusing; teach two-call deltas.
- Trade-offs: (a) is new syntax and an output line, but it serves any DMV in both front ends and needs no server worker. (b) holds a worker and doesn't help MCP. (c) costs tokens, gives an uncontrolled interval, and leaves spin loops as the workaround.
- **Recommendation: (a)**, and the cap's number from the owner.
- **Decided 2026-10-23: (a), capped at 60 s** (PLAN.md §6, decision 27).

**D7. sqlh-owned reads (§6.7).**
- Options:
  - (a) `activity` and `errorlog` as CLI commands and MCP tools in Phase 6, and `info` reporting view permissions;
  - (b) the error-log recipe in DESIGN.md §7 and the skill only; `activity` later;
  - (c) neither.
- Trade-offs: (a) is new code, output shapes and MCP size (decision 20's gate), and gives MCP a `sp_WhoIsActive` substitute with no trust question. (b) leaves MCP with hand-written DMV joins in an incident.
- **Recommendation: (a).**
- **Decided 2026-10-23: (a)** (PLAN.md §6, decision 28).

**D8. How much showplan work goes into Phase 6 (§8).**
- Options: (a) hygiene only; (b) also the estimated-plan mode for reads; (c) also estimated plans of writes and procedures, after the detector proof.
- Trade-offs: the plan modes are output-contract work; hygiene is cheap and ends MCP's dropped connection.
- **Recommendation: (a) in Phase 6; (b) next; (c) after the proof, with `deny` profiles allowed to plan writes.**
- **Decided 2026-10-23: (a) in Phase 6; (b) next; (c) after the proof, with `deny` profiles allowed to plan writes** (PLAN.md §6, decision 29).

**D9. Small items, taken together (§6.10).** Each is yes or no.
- (a) The NOLOCK note. Alternatives: refuse them and point to `--read-uncommitted` (over-blocks a common habit), or nothing.
- (b) Host-disclosure reads stay allowed, documented. Alternative: a profile setting that refuses them.
- (c) The application name per run kind. Cost: a pooled MCP connection keeps one name, so `execute` needs its own connections.
- (d) Refuse the literal all-wildcard `SAMPLED`/`DETAILED` call. Alternative: a note (it arrives after the scan) or nothing.
- (e) Close MCP's session-state holes until tested, with advice to run them with the CLI. Cost: MCP loses `OPEN SYMMETRIC KEY`/`OPEN MASTER KEY` with `DECRYPTBYKEY` reads (F15, I23 pass today) and `SET IDENTITY_INSERT #t` until the reset test passes; the CLI keeps them.
- **Recommendation: yes to all five.**
- **Decided 2026-10-23: yes to all five** (PLAN.md §6, decision 30).

### Part 2: changes (D10–D12 and D15 need D1(b); D13(b), D13(d) and D14(b) don't, apart from their `--allow` mentions)

**D10. The deny tier (§7.1).**
- Options:
  - (a) none (today: `--allow-write` skips every check);
  - (b) the fixed list in every mode, worded as an accident guard, plus `DROP DATABASE` and `sp_configure` enabling `xp_cmdshell`, OLE Automation or CLR, by any part of the option's name (W8–W10);
  - (c) (b), plus refusing dynamic-SQL forms under `--allow-write` unless `unvetted` is granted;
  - (d) (b), plus a profile-extendable list;
  - (e) (b), plus the deny tier reading one literal of dynamic SQL with D13(b)'s rule in every mode, `--allow-write` and `--allow unvetted` included.
- Trade-offs: (b) is a new refusal that offers no flag, and catches only what is named. (c) refuses legitimate write scripts and still misses synonyms and wrappers. (d) is more configuration for a per-server dial. (e) catches W1 and W6 at linear cost, reusing D13(b)'s code; a variable, a concatenation and `EXEC @var` still get past it.
- **Recommendation: (b), and (e) once D13(b) exists.** Revisit (d) if asked.
- **Decided 2026-10-23: (b), and (e) once D13(b) exists; none of the candidates added** (PLAN.md §6, decision 31).

**D11. Grants per kind, and the wrapper per kind (§5.5, §7.2).**
- Options:
  - (a) the profile's `allow` list, honored under `deny`; defaults by `write_policy` (`flag`/`allow`: every kind; `deny`: none); `--allow K` only for listed kinds; the wrapper per kind, kept only when every granted kind keeps it; `unvetted` never with a kind that drops it; un-created `#x` write targets refused without the wrapper;
  - (b) listed kinds always on for the profile, no per-run flag;
  - (c) a per-run flag only, refused under `deny` (the draft's D8).
- Sub-choice, the local `COPY_ONLY` backup: (i) it stays in `maintenance` (§7.3); (ii) it moves to "other changes". Either way `execute`'s first increment leaves it out (D15).
- Trade-offs: (a) makes each change visible on its run's command line and keeps `deny` meaningful. (b) is simpler but silent per run. (c) leaves `deny` profiles with nothing. In every option the list is an accident guard (R19). (i) lets a `deny` profile that lists `maintenance` take a copy before a risky fix; one run can also write a full copy of a database to a path that only looks local (§2) or fill a volume. (ii) closes that, at the cost of `--allow-write` for the copy.
- **Recommendation: (a), with (i)** for the CLI, where the command line shows the target.
- **Decided 2026-10-23: (a), with (i)** (PLAN.md §6, decision 32).

**D12. Kill (§7.5).**
- Options:
  - (a) a `kill` kind (`KILL <session id>`, `sp_stop_job`), no wrapper; an identity-checked `sqlh kill` later;
  - (b) `--allow-write` only (today);
  - (c) refuse raw `KILL` everywhere and offer only the identity-checked command.
- Trade-offs: (a) lets a `deny` profile end a runaway without DML; session-id reuse remains until the later command. (c) is safest but a new command before anything else works.
- **Recommendation: (a)**, the identity-checked command when agents use `kill`.
- **Decided 2026-10-23: (a)** (PLAN.md §6, decision 33).

**D13. Dynamic SQL and cross-database work (§7.6).**
- Options: (a) keep refusing; (b) one literal classified under the caller's whole policy, with the named tests; (c) `sp_MSforeachdb` literals; (d) sqlh-owned per-database execution (`--each-db`, MCP `databases`).
- Trade-offs: (b) enables faithful parameter-sniffing repros and plan-cache recipes. (c) depends on an undocumented procedure that skips databases. (d) covers the cursor-and-`EXEC (@sql)` loop with nothing dynamic, in both front ends, at the cost of an output shape.
- **Recommendation: (b) and (d); drop (c).**
- **Decided 2026-10-23: (b) and (d); (c) dropped** (PLAN.md §6, decision 34).

**D14. Trusted community procedures (§7.7).**
- Options:
  - (a) never: they stay `unvetted`, reachable through `--allow unvetted` (in the wrapper) or `--allow-write`;
  - (b) owner-written `trusted_procedures` entries: named parameters only, write parameters refused by exact name, literal caps; the same-batch check (no signature, no `EXECUTE AS`, optional hash pin); CLI and MCP, inside the wrapper, and in MCP on a connection closed after every trusted call until the reset test passes;
  - (c) (b), plus built-in templates.
- Trade-offs: (b) is the owner's trust decision per server, with a check against drift that admits its gaps (time of check vs use, nested code). In MCP, sqlh can't tell which procedures take a session-owned app lock, set session context or open a key, so until §10 item 5's reset test passes for all three, every trusted call costs the next call a login (148–185 ms); after it, trusted calls are pooled like any other. Alternatives: refuse trusted entries in MCP until the test passes, or an owner-set per-entry key (`mcp = true`) after reading the source, which trusts the owner's reading. (c) fails open when the procedures change.
- **Recommendation: (b)**, starter entries in the docs.
- **Decided 2026-10-23: (b), with the connection closed after each trusted call in MCP until the reset test passes** (PLAN.md §6, decision 35).

**D15. The `execute` tool (§7.8; the follow-up to decision 16).**
- Options:
  - (a) none in Phase 6;
  - (b) `execute` for any listed kind;
  - (c) `execute` for listed `tuning`, `kill` and `maintenance` only (ranks 2–4), without `maintenance`'s local `COPY_ONLY` backup, with the required `kinds` argument, unpooled connections and the deny tier.
- Trade-offs: under any `execute` tool the last check is the harness prompt, often set to always allow (decision 16). (c) offers the likeliest real uses (force a plan, end a runaway, update one statistic) and keeps `data` and `unvetted` in the CLI until a trial shows how agents use it. It leaves the backup out because one call could copy a whole database to a path that only looks local, or fill a volume, and no investigation needs it (§7.3). Whether the Query Store procedures are transactional doesn't matter without a wrapper.
- **Recommendation: (c)** once D10 and D11 are built; (b) after an agent trial.
- **Decided 2026-10-23: (a), none in Phase 6** (PLAN.md §6, decision 36). The design above is kept for a later phase, after an agent trial with the kinds in the CLI.

---

## 10. Open questions and live tests still needed

Live tests run only against `sqlh_test` on the test server, never production, with decision 5's `Reader` login where a permission matters. **Approval** marks a test that changes server-level or database-level state beyond what the live tests may do today (tables of their own in `sqlh_test`, and its compatibility level under decision 21).

**In order:**
1. **MSDTC, first (D4a depends on it).** A four-part `SELECT` through a loopback linked server inside `BEGIN TRANSACTION`: does it run, promote to a distributed transaction, or fail; and with the MSDTC service stopped? **Approval:** a linked server is a server-level object.
2. **The exact-shape reads (D5(d)).** `RESTORE HEADERONLY`, `FILELISTONLY` and `LABELONLY` inside the wrapper (expect 3021) and alone without it; `KILL n WITH STATUSONLY` inside (6115?) and alone, against a session the test owns. **Approval** for the backup file the RESTORE checks need: a `COPY_ONLY` backup of `sqlh_test` writes a file on the server's disk.
3. **SNAPSHOT (D3).** In-SQL `SET TRANSACTION ISOLATION LEVEL SNAPSHOT` as the first statement inside the wrapper; the run setting in the session setup; an MCP call's `database` `USE` after `BEGIN TRANSACTION`. **Approval:** `ALLOW_SNAPSHOT_ISOLATION ON` on `sqlh_test` is a database setting outside decision 21.
4. **Round 2 inside the wrapper, with permissions one grant at a time (D5):** `sp_server_diagnostics` (repeat 0, and its time from request to last row, expected about 5 s, for D5(a1)), `DBCC SQLPERF(LOGSPACE)`, `DBCC MEMORYSTATUS` (and its result count), `CHECKIDENT ... NORESEED`, `sp_autostats` (display), `sp_spaceused @updateusage = 'false'`, `sp_helplogins` and the role procedures, the msdb procedures (`SQLAgentReaderRole`), `DBCC CHECKCONSTRAINTS` on a named table of the test's own. Also: `sp_who`, `sp_who2` and `DBCC INPUTBUFFER` with `VIEW SERVER PERFORMANCE STATE` alone; `sys.dm_db_stats_histogram` and masked columns without `UNMASK`. Read-only, for D10(b): `sp_configure`'s source (`OBJECT_DEFINITION`), whether it matches `@configname` with `LIKE`, and `SELECT name FROM sys.configurations WHERE name LIKE N'%[_]%'`.
5. **MCP reset (D9(e), D5(e), D14).** Through `sqlh mcp --pool 1` on one SPID, as 5.4 did: `SET STATISTICS PROFILE`, `SHOWPLAN_ALL`/`TEXT`, `AUTOPILOT`, `IDENTITY_INSERT`, a session trace flag (`DBCC TRACEON(3604)` without `-1`), `EXECUTE AS USER` without `REVERT`, and, for D14, a session-owned app lock, session context and a symmetric key each set or opened inside a procedure; until all three pass, `sqlh mcp` closes the connection after every trusted call (§7.7). **Approval** for `OPEN SYMMETRIC KEY`/`OPEN MASTER KEY`: they need a master key and a key in `sqlh_test`, objects beyond tables.
6. **Showplan (D8, §8).** That §6.8's `OFF` requests keep the MCP connection (same SPID, the next call's `@@TRANCOUNT` 1). Error 1067 for SHOWPLAN not alone [mem]. Whether `EXEC dbo.Proc`, `EXEC sys.sp_executesql` and `EXEC xp_*` ever execute under `SHOWPLAN_XML`, detected by a scratch procedure that advances a scratch sequence. **Approval:** a procedure and a sequence are objects beyond tables.
7. **Inside the wrapper, no approval needed** (own tables in `sqlh_test`): `DBCC CHECKDB ... ESTIMATEONLY` (does it create a snapshot?), `sp_estimate_data_compression_savings`, `DBCC UPDATEUSAGE`, `CHECKIDENT ... RESEED`, `CREATE STATISTICS`, `sp_autostats ... 'OFF'` (undone by the rollback?), `UPDATE STATISTICS` (does it still commit independently on 2025?), `DBCC CHECKCONSTRAINTS` on a named table; `ALTER DATABASE sqlh_test SET COMPATIBILITY_LEVEL = <its current level>` (expect 226; within decision 21).
8. **Needs approval, server state:** whether `CREATE`/`ALTER EVENT SESSION`, `DBCC TRACEON` and `sp_configure` with a value run in a user transaction, and whether `ROLLBACK` undoes them (read `sys.server_event_sessions`, `sys.dm_xe_sessions`, `sys.configurations` after); the `sp_query_store_*` procedures (they need Query Store on `sqlh_test`, a database setting).
9. **Trusted procedures (D14).** With a `Reader` login holding only `EXECUTE`: are the module's rows visible in `sys.sql_modules` and `sys.crypt_properties`? Does the check refuse a signed copy and an `EXECUTE AS OWNER` copy? **Approval:** installing community procedures on the test server.
10. **sqlh-owned reads (D7).** `activity` with and without `VIEW SERVER PERFORMANCE STATE`, and with a busy and an idle pooled connection of its own; whether `HAS_PERMS_BY_NAME` answers for `master.sys.xp_readerrorlog`; `errorlog`'s two paths, including `INSERT INTO #log EXEC sys.sp_readerrorlog ...` inside the wrapper as a `Reader` holding only `VIEW SERVER STATE`, then only `VIEW ANY ERROR LOG` (the live test so far ran only the `xp_` form, as the test profile); `USE` inside the wrapper for D13(d).
11. **In a lab, against a host the owner controls:** does a UNC path given to `sys.fn_xe_file_target_read_file` or `sys.dm_os_enumerate_filesystem` make SQL Server 2025 connect and authenticate to that host [inf]? **Approval.**
12. **Agent trials (Phase 7):** the pause, `activity` and `errorlog` in use; later, the `execute` tool's first increment.

**From source, no server:** Ola's `DatabaseBackup` `@@TRANCOUNT` check; where `sp_PressureDetector`'s `WAITFOR` is; `sp_QuickieStore`'s logging; `sp_BlitzCache`'s `@OutputServerName` path; whether `sp_Blitz`'s default call (`@CheckServerInfo = 0`) runs `xp_cmdshell 'net localgroup administrators'` (CheckIDs 260, 261) when it is enabled.

**From the docs:** whether SQL Server 2025 Standard supports `ONLINE = ON` rebuilds; the GA status of `ABORT_QUERY_EXECUTION`; which statements raise Msg 226 and 574 (the messages' texts are [live: sys.messages]; which statements raise them is [doc]).

**Doc fixes, no decision needed:**
- DESIGN.md §7's "other `sp_help*`" (§6.1; done 2026-11-02).
- DESIGN.md §7's "and so does `INSERT INTO #t EXEC xp_readerrorlog ...` (or `sp_readerrorlog`)": the `sp_` form is unmeasured; mark it inferred until item 10 measures it.
- DESIGN.md §7's backstop note, and the skill (Phase 8): a CLR function with `EXTERNAL_ACCESS` or `UNSAFE` called from a `SELECT` can send what it reads off the server, and enabling such CLR for an agent's login defeats read-only intent (§2, §4).

---

## Appendix A: probe evidence

**How.** The release exe of `f905385` (2,808,320 bytes; built after the last source change, with a clean tree) was run offline against `127.0.0.1,9`, where nothing listens, with no profiles and no `SQLCMD*` variables. The guard runs before the connect: ✓ means it let the input through (exit 2 at the connect), ✗ means it refused (exit 5); the message is shortened. Rows marked "with `--allow-write`" ran with that flag and no profile (`write_policy` `flag`). Multi-line inputs are joined with ⏎; long ones are cut with …. Synthetic names throughout (`Lnk`, `host`, `Sales`, `dbo.Orders`).

**Groups.** A interim step and argument checks; B blocking, sessions, locks; C waits and resources; Q Query Store, plans, showplan (Q, not D, so that no ID reads as a decision); E indexes, statistics, storage, integrity; F configuration, logs, Agent, backups, security, linked servers; G community procedures and dynamic SQL; H session state and scratch; I and J follow-up cases (J39–J48: function spellings, mixed separators, further path arguments, a partial `sp_configure` name); W `--allow-write`.

**Against the earlier draft's evidence.** (1) The interim procedures pass (A1–A9, A22, A23), as the draft's uncommitted build showed and `eb5f568` did not. (2) `sp_configure;1 'x', 1` (A12), `sp_configure GET, 1` (A13), `sp_spaceused;1 ...` (A14) and `sp_spaceused MOVE, ...` (A15) are refused; they passed on the draft's stale build, which predates `f905385`'s review fixes. (3) The draft's UNC cases spelled the path with one backslash (`N'\host\share'`); with `\\host` (C12–C14) the verdict is the same, ✓. (4) The draft's own 72 cases, re-run on `f905385`, give exactly its working-tree build's verdicts (none of them exercised the review fixes), and differ from `eb5f568` only on the interim procedures. No verdict differs from what the critiques reported for the same input.

| ID | Input | Verdict at `f905385` |
|---|---|---|
| A1 | `EXEC xp_readerrorlog 0, 1, N'Login failed'` | ✓ |
| A2 | `EXEC sys.sp_readerrorlog 0, 1, N'error'` | ✓ |
| A3 | `sp_readerrorlog 0, 1, N'deadlock'` | ✓ |
| A4 | `EXEC sys.sp_enumerrorlogs` | ✓ |
| A5 | `EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'Software\Microsoft\MSSQLServer\MSSQLServer', N'BackupDirectory'` | ✓ |
| A6 | `EXEC xp_fixeddrives` | ✓ |
| A7 | `EXEC master..xp_msver 'ProductVersion'` | ✓ |
| A8 | `EXEC sp_configure` | ✓ |
| A9 | `EXEC sys.sp_configure 'max server memory (MB)'` | ✓ |
| A10 | `EXEC sp_configure 'show advanced options', 1` | ✗ line 1: sp_configure with @configvalue … changes a server setting (advice: sys.configurations) |
| A11 | `EXEC sp_configure @configname = 'x', @configvalue = 1` | ✗ line 1: sp_configure with @configvalue … changes a server setting (advice: sys.configurations) |
| A12 | `EXEC sp_configure;1 'x', 1` | ✗ line 1: sp_configure with @configvalue … changes a server setting (advice: sys.configurations) |
| A13 | `EXEC sp_configure GET, 1` | ✗ line 1: sp_configure with @configvalue … changes a server setting (advice: sys.configurations) |
| A14 | `EXEC sp_spaceused;1 N'dbo.Orders', 'true'` | ✗ line 1: sp_spaceused with @updateusage … runs DBCC UPDATEUSAGE |
| A15 | `EXEC sp_spaceused MOVE, N'true'` | ✗ line 1: sp_spaceused with @updateusage … runs DBCC UPDATEUSAGE |
| A16 | `EXEC sp_spaceused @objname = N'dbo.Orders', @updateusage = N'FALSE'` | ✗ line 1: sp_spaceused with @updateusage … runs DBCC UPDATEUSAGE |
| A17 | `EXEC xp_instance_regwrite N'HKEY_LOCAL_MACHINE', N'x', N'y', N'REG_SZ', N'z'` | ✗ line 1: EXEC xp_instance_regwrite would write |
| A18 | `EXEC xp_regread N'HKEY_LOCAL_MACHINE', N'x', N'y'` | ✗ line 1: EXEC xp_regread would write |
| A19 | `EXEC xp_cmdshell 'dir'` | ✗ line 1: EXEC xp_cmdshell would write |
| A20 | `EXEC xp_enumerrorlogs` | ✗ line 1: EXEC xp_enumerrorlogs would write |
| A21 | `EXEC dbo.xp_readerrorlog 0, 1` | ✗ line 1: EXEC dbo.xp_readerrorlog: qualifier rule (advice: sys. spelling) |
| A22 | `CREATE TABLE #log (LogDate datetime, ProcessInfo nvarchar(64), [Text] nvarchar(max)); ⏎ INSERT INTO #log EXEC sys.sp_readerrorlog 0, 1; ⏎ SELECT TOP (50) LogDate, ProcessInfo, [Text] FROM #log ORDER BY LogDate DESC;` | ✓ |
| A23 | `EXEC sys.xp_readerrorlog 0, 1, N'Login failed', NULL, '2026-10-23 08:00', '2026-10-23 10:00', N'desc'` | ✓ |
| A24 | `EXEC sp_help_revlogin` | ✗ line 1: EXEC sp_help_revlogin would write |
| A25 | `EXEC sp_helprotect` | ✓ |
| B1 | `EXEC sp_who2 'active'` | ✓ |
| B2 | `sp_who2 'active'` | ✓ |
| B3 | `SELECT r.session_id, r.blocking_session_id, t.text FROM sys.dm_exec_requests AS r CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) AS t` | ✓ |
| B4 | `SELECT s.session_id, ib.event_info FROM sys.dm_exec_sessions AS s CROSS APPLY sys.dm_exec_input_buffer(s.session_id, NULL) AS ib` | ✓ |
| B5 | `EXEC sp_WhoIsActive` | ✗ line 1: EXEC sp_WhoIsActive would write |
| B6 | `sp_WhoIsActive @find_block_leaders = 1, @sort_order = '[blocked_session_count] DESC'` | ✗ line 1: sp_WhoIsActive starts the batch … (a typo?); only read-only system procedures … |
| B7 | `CREATE TABLE #t (c int); ⏎ INSERT INTO #t EXEC dbo.sp_WhoIsActive @get_plans = 1;` | ✗ line 2: EXEC dbo.sp_WhoIsActive would write |
| B8 | `EXEC master.dbo.sp_WhoIsActive` | ✗ line 1: EXEC master.dbo.sp_WhoIsActive would write |
| B9 | `DBCC INPUTBUFFER(53)` | ✓ |
| B10 | `CREATE TABLE #ib (EventType nvarchar(30), Parameters smallint, EventInfo nvarchar(4000)); ⏎ INSERT INTO #ib EXEC ('DBCC INPUTBUFFER(53)');` | ✗ line 2: EXEC (dynamic SQL) would write |
| B11 | `EXEC sp_lock` | ✓ |
| B12 | `DBCC OPENTRAN` | ✓ |
| B13 | `DBCC SQLPERF(LOGSPACE)` | ✗ line 1: DBCC SQLPERF would write |
| B14 | `EXEC sys.sp_server_diagnostics` | ✗ line 1: EXEC sys.sp_server_diagnostics would write |
| B15 | `EXEC sys.sp_server_diagnostics @repeat_interval = 0` | ✗ line 1: EXEC sys.sp_server_diagnostics would write |
| B16 | `KILL 53` | ✗ line 1: KILL is not allowed in read-only mode |
| B17 | `KILL 53 WITH STATUSONLY` | ✗ line 1: KILL is not allowed in read-only mode |
| B18 | `EXEC msdb.dbo.sp_stop_job @job_name = N'Nightly ETL'` | ✗ line 1: EXEC msdb.dbo.sp_stop_job would write |
| B19 | `SELECT 1 AS [kill]` | ✓ |
| B20 | `SET DEADLOCK_PRIORITY HIGH; SELECT 1;` | ✓ |
| B21 | `DECLARE @p int = 10; SET DEADLOCK_PRIORITY @p; SELECT 1;` | ✓ |
| B22 | `SET LOCK_TIMEOUT -1; SELECT 1;` | ✓ |
| B23 | `DECLARE @t int = -1; SET LOCK_TIMEOUT @t; SELECT 1;` | ✓ |
| B24 | `SET LOCK_TIMEOUT 30000; ⏎ SELECT OBJECT_NAME(object_id), name FROM sys.indexes WHERE object_id = OBJECT_ID(N'dbo.Orders');` | ✓ |
| B25 | `SELECT * FROM dbo.Orders WITH (TABLOCKX, HOLDLOCK)` | ✓ |
| B26 | `SELECT * FROM dbo.Orders (UPDLOCK) WHERE OrderId = 1` | ✓ |
| B27 | `SELECT * FROM dbo.Orders AS o WITH (NOLOCK, XLOCK)` | ✓ |
| B28 | `SELECT * FROM dbo.Orders WITH (SERIALIZABLE)` | ✓ |
| B29 | `SELECT * FROM dbo.Orders WITH (READPAST, ROWLOCK)` | ✓ |
| B30 | `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; SELECT 1;` | ✓ |
| B31 | `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; SELECT 1;` | ✓ |
| B32 | `EXEC sp_getapplock @Resource = N'r', @LockMode = N'Exclusive'` | ✗ line 1: EXEC sp_getapplock would write |
| B33 | `EXEC sp_set_session_context @key = N'k', @value = 1` | ✗ line 1: EXEC sp_set_session_context would write |
| B34 | `SELECT CAST(event_data AS xml) FROM sys.fn_xe_file_target_read_file(N'system_health*.xel', NULL, NULL, NULL) WHERE object_name = N'xml_deadlock_report'` | ✓ |
| B35 | `DECLARE @p nvarchar(260); ⏎ SELECT @p = REPLACE(CAST(t.target_data AS xml).value(N'(EventFileTarget/File/@name)[1]', N'nvarchar(260)'), N'_0_', N'*') FROM sys.dm_xe_session_targets AS t JOIN sys.dm_xe_sessions AS s ON s.address = t.event…` | ✓ |
| B36 | `DBCC TRACEON(1222, -1)` | ✗ line 1: DBCC TRACEON would write |
| B37 | `SELECT N'KILL ' + CAST(session_id AS nvarchar(10)) AS kill_cmd, login_name FROM sys.dm_exec_sessions WHERE is_user_process = 1 AND open_transaction_count > 0` | ✓ |
| C1 | `SELECT wait_type, waiting_tasks_count, wait_time_ms INTO #w1 FROM sys.dm_os_wait_stats; ⏎ WAITFOR DELAY '00:00:10'; ⏎ SELECT w.wait_type, w.wait_time_ms - o.wait_time_ms AS delta_ms FROM sys.dm_os_wait_stats AS w JOIN #w1 AS o ON o.wait_…` | ✗ line 2: WAITFOR is not allowed in read-only mode |
| C2 | `SELECT wait_type, wait_time_ms INTO #w1 FROM sys.dm_os_wait_stats; ⏎ DECLARE @t datetime2 = SYSDATETIME(); ⏎ WHILE SYSDATETIME() < DATEADD(second, 10, @t) SET @t = @t; ⏎ SELECT w.wait_type, w.wait_time_ms - o.wait_time_ms AS delta_ms FRO…` | ✓ |
| C3 | `WAITFOR DELAY '00:00:05'` | ✗ line 1: WAITFOR is not allowed in read-only mode |
| C4 | `SELECT 'WAITFOR' AS w` | ✓ |
| C5 | `DBCC SQLPERF('sys.dm_os_wait_stats', CLEAR)` | ✗ line 1: DBCC SQLPERF would write |
| C6 | `ALTER RESOURCE GOVERNOR RESET STATISTICS` | ✗ line 1: ALTER would write |
| C7 | `DBCC MEMORYSTATUS` | ✗ line 1: DBCC MEMORYSTATUS would write |
| C8 | `SELECT TOP (20) * FROM sys.dm_os_buffer_descriptors` | ✓ |
| C9 | `DBCC DROPCLEANBUFFERS` | ✗ line 1: DBCC DROPCLEANBUFFERS would write |
| C10 | `DBCC FREESYSTEMCACHE('ALL')` | ✗ line 1: DBCC FREESYSTEMCACHE would write |
| C11 | `CHECKPOINT` | ✗ line 1: CHECKPOINT is not allowed in read-only mode |
| C12 | `SELECT * FROM sys.dm_os_enumerate_filesystem(N'\\host\share', N'*')` | ✓ |
| C13 | `SELECT * FROM sys.dm_os_file_exists(N'\\host\share\x.bak')` | ✓ |
| C14 | `SELECT * FROM sys.fn_trace_gettable(N'\\host\share\x.trc', DEFAULT)` | ✓ |
| C15 | `EXEC sp_BlitzFirst @Seconds = 5` | ✗ line 1: EXEC sp_BlitzFirst would write |
| C16 | `SELECT * FROM sys.dm_os_volume_stats(DB_ID(), 1)` | ✓ |
| Q1 | `SET SHOWPLAN_XML ON` | ✓ |
| Q2 | `SET SHOWPLAN_XML ON ⏎ GO ⏎ SELECT * FROM dbo.Orders WHERE CustomerId = 42 ⏎ GO ⏎ SET SHOWPLAN_XML OFF` | ✓ |
| Q3 | `SET SHOWPLAN_XML ON ⏎ GO ⏎ UPDATE dbo.Orders SET Status = 1 WHERE OrderId = 1 ⏎ GO ⏎ SET SHOWPLAN_XML OFF` | ✗ line 3: UPDATE dbo.Orders would write |
| Q4 | `SET SHOWPLAN_XML ON ⏎ GO ⏎ EXEC dbo.usp_GetOrders @CustomerId = 42 ⏎ GO ⏎ SET SHOWPLAN_XML OFF` | ✗ line 3: EXEC dbo.usp_GetOrders would write |
| Q5 | `SET SHOWPLAN_XML ON ⏎ GO ⏎ EXEC sys.sp_executesql N'SELECT * FROM dbo.Orders WHERE CustomerId = @c', N'@c int', @c = 42 ⏎ GO ⏎ SET SHOWPLAN_XML OFF` | ✗ line 3: EXEC sys.sp_executesql would write |
| Q6 | `SET SHOWPLAN_XML ON; SELECT 1;` | ✓ |
| Q7 | `SET STATISTICS XML ON; SELECT 1;` | ✓ |
| Q8 | `SET STATISTICS XML ON; ⏎ EXEC dbo.usp_GetOrders @CustomerId = 42;` | ✗ line 2: EXEC dbo.usp_GetOrders would write |
| Q9 | `SET FMTONLY ON; SELECT * FROM dbo.Orders;` | ✓ |
| Q10 | `SET PARSEONLY ON` | ✓ |
| Q11 | `SET NOEXEC ON` | ✓ |
| Q12 | `SELECT COUNT_BIG(*) FROM dbo.Orders OPTION (QUERYTRACEON 9481)` | ✓ |
| Q13 | `SET STATISTICS IO, TIME ON; SELECT 1;` | ✓ |
| Q14 | `SET STATISTICS PROFILE ON; SELECT 1;` | ✓ |
| Q15 | `ALTER DATABASE CURRENT SET QUERY_STORE CLEAR` | ✗ line 1: ALTER would write |
| Q16 | `ALTER DATABASE CURRENT SET QUERY_STORE = ON` | ✗ line 1: ALTER would write |
| Q17 | `EXEC sys.sp_query_store_force_plan @query_id = 42, @plan_id = 7` | ✗ line 1: EXEC sys.sp_query_store_force_plan would write |
| Q18 | `EXEC sys.sp_query_store_set_hints @query_id = 42, @query_hints = N'OPTION (ABORT_QUERY_EXECUTION)'` | ✗ line 1: EXEC sys.sp_query_store_set_hints would write |
| Q19 | `EXEC sys.sp_query_store_flush_db` | ✗ line 1: EXEC sys.sp_query_store_flush_db would write |
| Q20 | `DBCC FREEPROCCACHE (0x060006001ECA270EC0215D05000000000000000000000000)` | ✗ line 1: DBCC FREEPROCCACHE would write |
| Q21 | `DBCC FREEPROCCACHE` | ✗ line 1: DBCC FREEPROCCACHE would write |
| Q22 | `ALTER DATABASE SCOPED CONFIGURATION CLEAR PROCEDURE_CACHE 0x060006001ECA270EC0215D05000000000000000000000000` | ✗ line 1: ALTER would write |
| Q23 | `EXEC sys.sp_recompile N'dbo.Orders'` | ✗ line 1: EXEC sys.sp_recompile would write |
| Q24 | `EXEC sp_describe_first_result_set N'SELECT 1'` | ✓ |
| Q25 | `SELECT n.r.value(N'@PhysicalOp', N'nvarchar(60)') FROM sys.dm_exec_query_stats AS qs CROSS APPLY sys.dm_exec_query_plan(qs.plan_handle) AS qp CROSS APPLY qp.query_plan.nodes(N'//RelOp') AS n(r)` | ✓ |
| Q26 | `DBCC PROCCACHE` | ✗ line 1: DBCC PROCCACHE would write |
| Q27 | `ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 4` | ✗ line 1: ALTER would write |
| E1 | `SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, 'DETAILED')` | ✓ |
| E2 | `SELECT * FROM sys.dm_db_index_physical_stats(DB_ID(), OBJECT_ID(N'dbo.Orders'), NULL, NULL, 'SAMPLED')` | ✓ |
| E3 | `DECLARE @m sysname = N'DETAILED'; ⏎ SELECT * FROM sys.dm_db_index_physical_stats(NULL, NULL, NULL, NULL, @m)` | ✓ |
| E4 | `DBCC SHOW_STATISTICS (N'dbo.Orders', N'IX_Orders_CustomerId') WITH STAT_HEADER` | ✓ |
| E5 | `EXEC sp_autostats N'dbo.Orders'` | ✗ line 1: EXEC sp_autostats would write |
| E6 | `UPDATE STATISTICS dbo.Orders IX_Orders_CustomerId WITH FULLSCAN` | ✗ line 1: UPDATE STATISTICS would write |
| E7 | `UPDATE STATISTICS #work` | ✗ line 1: UPDATE STATISTICS would write |
| E8 | `CREATE STATISTICS s ON #work (c)` | ✗ line 1: CREATE would write |
| E9 | `ALTER INDEX ALL ON #work REBUILD` | ✗ line 1: ALTER would write |
| E10 | `CREATE INDEX ix ON #work (c)` | ✓ |
| E11 | `CREATE INDEX ix ON dbo.Orders (CustomerId)` | ✗ line 1: CREATE INDEX dbo.Orders would write |
| E12 | `DBCC CHECKIDENT ('dbo.Orders', NORESEED)` | ✗ line 1: DBCC CHECKIDENT would write |
| E13 | `DBCC CHECKDB WITH ESTIMATEONLY` | ✗ line 1: DBCC CHECKDB would write |
| E14 | `DBCC CHECKCONSTRAINTS (N'dbo.Orders') WITH ALL_CONSTRAINTS` | ✗ line 1: DBCC CHECKCONSTRAINTS would write |
| E15 | `DBCC CHECKDB (N'Sales', REPAIR_ALLOW_DATA_LOSS)` | ✗ line 1: DBCC CHECKDB would write |
| E16 | `DBCC PAGE (N'Sales', 1, 100, 3)` | ✗ line 1: DBCC PAGE would write |
| E17 | `DBCC WRITEPAGE (N'Sales', 1, 100, 0, 1, 0x00)` | ✗ line 1: DBCC WRITEPAGE would write |
| E18 | `SELECT * FROM sys.dm_db_page_info(DB_ID(), 1, 100, 'DETAILED')` | ✓ |
| E19 | `SELECT TOP (10) * FROM sys.dm_db_database_page_allocations(DB_ID(), OBJECT_ID(N'dbo.Orders'), NULL, NULL, 'LIMITED')` | ✓ |
| E20 | `EXEC sp_estimate_data_compression_savings N'dbo', N'Orders', NULL, NULL, N'PAGE'` | ✗ line 1: EXEC sp_estimate_data_compression_savings would write |
| E21 | `SELECT ar.replica_server_name, drs.synchronization_state_desc FROM [Lnk].master.sys.dm_hadr_database_replica_states AS drs JOIN [Lnk].master.sys.availability_replicas AS ar ON ar.replica_id = drs.replica_id` | ✓ |
| E22 | `SELECT TOP (5) * INTO #r FROM Lnk.Sales.dbo.Orders` | ✓ |
| E23 | `SELECT * FROM Lnk..sys.databases` | ✓ |
| E24 | `SELECT Sales.dbo.Orders.OrderId FROM Sales.dbo.Orders` | ✓ |
| E25 | `ALTER INDEX IX_Orders_CustomerId ON dbo.Orders REBUILD WITH (ONLINE = ON (WAIT_AT_LOW_PRIORITY (MAX_DURATION = 1 MINUTES, ABORT_AFTER_WAIT = BLOCKERS)))` | ✗ line 1: ALTER would write |
| E26 | `DBCC SHRINKFILE (N'Sales_log', 1024)` | ✗ line 1: DBCC SHRINKFILE would write |
| E27 | `SELECT [Current LSN], Operation FROM sys.fn_dblog(NULL, NULL) WHERE Operation = N'LOP_DELETE_ROWS'` | ✓ |
| E28 | `SELECT * FROM sys.fn_dump_dblog(NULL, NULL, N'DISK', 1, N'\\host\share\Sales_LOG.trn', DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAUL…` | ✓ |
| E29 | `UPDATE #o SET Status = 0 FROM dbo.Orders AS #o WHERE #o.OrderId = 1` | ✓ |
| E30 | `DBCC LOGINFO` | ✗ line 1: DBCC LOGINFO would write |
| E31 | `DBCC SHOWCONTIG` | ✗ line 1: DBCC SHOWCONTIG would write |
| F1 | `EXEC msdb.dbo.sp_help_job` | ✗ line 1: EXEC msdb.dbo.sp_help_job would write |
| F2 | `EXEC msdb.dbo.sp_help_jobactivity @job_name = N'Nightly ETL'` | ✗ line 1: EXEC msdb.dbo.sp_help_jobactivity would write |
| F3 | `SELECT j.name, msdb.dbo.agent_datetime(h.run_date, h.run_time) AS run_at FROM msdb.dbo.sysjobhistory AS h JOIN msdb.dbo.sysjobs AS j ON j.job_id = h.job_id` | ✓ |
| F4 | `EXEC msdb.dbo.sp_start_job @job_name = N'Nightly ETL'` | ✗ line 1: EXEC msdb.dbo.sp_start_job would write |
| F5 | `RESTORE HEADERONLY FROM DISK = N'D:\Backup\Sales_FULL.bak'` | ✗ line 1: RESTORE is not allowed in read-only mode |
| F6 | `RESTORE FILELISTONLY FROM DISK = N'D:\Backup\Sales_FULL.bak'` | ✗ line 1: RESTORE is not allowed in read-only mode |
| F7 | `RESTORE DATABASE Sales FROM DISK = N'D:\Backup\Sales_FULL.bak' WITH REPLACE` | ✗ line 1: RESTORE is not allowed in read-only mode |
| F8 | `BACKUP DATABASE Sales TO DISK = N'\\host\share\Sales.bak' WITH COPY_ONLY` | ✗ line 1: BACKUP is not allowed in read-only mode |
| F9 | `SELECT TOP (10) bs.database_name, bs.backup_finish_date, bmf.physical_device_name FROM msdb.dbo.backupset AS bs JOIN msdb.dbo.backupmediafamily AS bmf ON bmf.media_set_id = bs.media_set_id ORDER BY bs.backup_finish_date DESC` | ✓ |
| F10 | `ALTER AVAILABILITY GROUP ag FAILOVER` | ✗ line 1: ALTER would write |
| F11 | `EXEC sp_helplogins` | ✗ line 1: EXEC sp_helplogins would write |
| F12 | `EXEC sys.xp_logininfo N'DOMAIN\user', N'all'` | ✗ line 1: EXEC sys.xp_logininfo would write |
| F13 | `EXECUTE AS USER = N'app_reader'; ⏎ SELECT * FROM sys.fn_my_permissions(N'dbo.Orders', N'OBJECT'); ⏎ REVERT;` | ✗ line 1: EXECUTE AS would write |
| F14 | `EXECUTE AS LOGIN = N'x' WITH NO REVERT` | ✗ line 1: EXECUTE AS would write |
| F15 | `OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE c` | ✓ |
| F16 | `OPEN MASTER KEY DECRYPTION BY PASSWORD = '...'` | ✓ |
| F17 | `EXEC sys.sp_testlinkedserver N'Lnk'` | ✗ line 1: EXEC sys.sp_testlinkedserver would write |
| F18 | `EXEC msdb.dbo.sp_send_dbmail @recipients = N'dba@example.com', @body = N'x'` | ✗ line 1: EXEC msdb.dbo.sp_send_dbmail would write |
| F19 | `SELECT TOP (50) event_time, statement FROM sys.fn_get_audit_file(N'\\host\share\*.sqlaudit', DEFAULT, DEFAULT)` | ✓ |
| F20 | `DECLARE @p nvarchar(260); ⏎ SELECT @p = log_file_path + N'*' FROM sys.server_file_audits WHERE name = N'AgentAudit'; ⏎ SELECT TOP (50) event_time, action_id, statement FROM sys.fn_get_audit_file(@p, DEFAULT, DEFAULT);` | ✓ |
| F21 | `EXEC sp_cycle_errorlog` | ✗ line 1: EXEC sp_cycle_errorlog would write |
| F22 | `CREATE EVENT SESSION s ON SERVER ADD EVENT sqlserver.sql_batch_completed` | ✗ line 1: CREATE would write |
| F23 | `ALTER EVENT SESSION [system_health] ON SERVER STATE = STOP` | ✗ line 1: ALTER would write |
| F24 | `SHUTDOWN WITH NOWAIT` | ✗ line 1: SHUTDOWN is not allowed in read-only mode |
| F25 | `DECLARE @o int; EXEC sp_OACreate 'Scripting.FileSystemObject', @o OUT` | ✗ line 1: EXEC sp_OACreate would write |
| F26 | `SELECT name, data_source FROM sys.servers` | ✓ |
| F27 | `EXEC sp_linkedservers` | ✗ line 1: EXEC sp_linkedservers would write |
| F28 | `EXEC ('SELECT 1') AT Lnk` | ✗ line 1: EXEC (dynamic SQL) would write |
| F29 | `SELECT * FROM OPENQUERY(Lnk, 'SELECT 1')` | ✗ line 1: OPENQUERY is not allowed in read-only mode |
| F30 | `EXEC Lnk.master.sys.sp_who` | ✗ line 1: EXEC Lnk.master.sys.sp_who: qualifier rule (advice: sys. spelling) |
| F31 | `GRANT SELECT ON dbo.Orders TO public` | ✗ line 1: GRANT is not allowed in read-only mode |
| G1 | `EXEC sp_executesql N'SELECT 1'` | ✗ line 1: EXEC sp_executesql would write |
| G2 | `EXEC ('SELECT 1')` | ✗ line 1: EXEC (dynamic SQL) would write |
| G3 | `EXEC sp_MSforeachdb N'USE [?]; SELECT DB_NAME()'` | ✗ line 1: EXEC sp_MSforeachdb would write |
| G4 | `DECLARE @db sysname, @sql nvarchar(max); ⏎ DECLARE c CURSOR LOCAL FAST_FORWARD FOR SELECT name FROM sys.databases WHERE state = 0; ⏎ OPEN c; FETCH NEXT FROM c INTO @db; ⏎ WHILE @@FETCH_STATUS = 0 ⏎ BEGIN ⏎ SET @sql = N'SELECT DB_NAME() A…` | ✗ line 7: EXEC (dynamic SQL) would write |
| G5 | `DECLARE @s nvarchar(100) = N'xp_cmdshell'; EXEC @s 'dir'` | ✗ line 1: EXEC of a procedure named by a variable would write |
| G6 | `EXEC ('xp_cmdshell ''dir''')` | ✗ line 1: EXEC (dynamic SQL) would write |
| G7 | `EXEC sp_executesql N'WAITFOR DELAY ''00:00:01'''` | ✗ line 1: EXEC sp_executesql would write |
| G8 | `EXEC sp_BlitzCache` | ✗ line 1: EXEC sp_BlitzCache would write |
| G9 | `EXEC master.dbo.sp_Blitz @CheckServerInfo = 1` | ✗ line 1: EXEC master.dbo.sp_Blitz would write |
| G10 | `EXEC dbo.IndexOptimize @Databases = N'USER_DATABASES', @Execute = 'N'` | ✗ line 1: EXEC dbo.IndexOptimize would write |
| G11 | `SELECT TOP (20) * FROM DBA.dbo.CommandLog ORDER BY ID DESC` | ✓ |
| G12 | `SELECT HASHBYTES('SHA2_256', OBJECT_DEFINITION(OBJECT_ID(N'master.dbo.sp_WhoIsActive')))` | ✓ |
| G13 | `SELECT dbo.fn_x(1) AS v` | ✓ |
| G14 | `SELECT TOP (5) * FROM dbo.syn_RemoteOrders` | ✓ |
| G15 | `SELECT cp.crypt_type_desc FROM master.sys.crypt_properties AS cp WHERE cp.major_id = OBJECT_ID(N'master.dbo.sp_WhoIsActive')` | ✓ |
| H1 | `SELECT NEXT VALUE FOR dbo.seq` | ✗ line 1: NEXT VALUE FOR dbo.seq advances a sequence |
| H2 | `USE master; SELECT 1;` | ✓ |
| H3 | `SET IMPLICIT_TRANSACTIONS ON; SELECT 1;` | ✓ |
| H4 | `SET XACT_ABORT ON; SELECT 1;` | ✓ |
| H5 | `SET ROWCOUNT 0; SELECT 1;` | ✓ |
| H6 | `SET AUTOPILOT ON` | ✓ |
| H7 | `CREATE TABLE #t (id int IDENTITY, c int); SET IDENTITY_INSERT #t ON;` | ✓ |
| H8 | `SELECT COUNT_BIG(*) FROM dbo.Orders WITH (NOLOCK)` | ✓ |
| H9 | `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; SELECT 1;` | ✓ |
| H10 | `SET TRANSACTION ISOLATION LEVEL SNAPSHOT; ⏎ SELECT COUNT_BIG(*) FROM dbo.Orders;` | ✓ |
| H11 | `DBCC OUTPUTBUFFER (53)` | ✗ line 1: DBCC OUTPUTBUFFER would write |
| H12 | `CREATE TABLE #t (c int); ⏎ MERGE INTO #t AS t USING (SELECT 1 AS c) AS s ON t.c = s.c WHEN NOT MATCHED THEN INSERT (c) VALUES (s.c);` | ✗ line 2: MERGE is not allowed in read-only mode |
| H13 | `SELECT name INTO ##g FROM sys.objects` | ✗ line 1: INTO ##g would write to a global temp table |
| H14 | `BEGIN TRAN; SELECT 1; COMMIT;` | ✗ line 1: BEGIN TRAN would write |
| H15 | `SELECT * FROM sys.dm_db_log_info(DB_ID())` | ✓ |
| I1 | `SELECT wait_type, wait_time_ms INTO #w1 FROM sys.dm_os_wait_stats ⏎ GO ⏎ :pause 10 ⏎ GO ⏎ SELECT 1` | ✓ |
| I2 | `SELECT * FROM sys.dm_os_enumerate_filesystem(N'Z:\', N'*')` | ✓ |
| I3 | `SELECT * FROM sys.fn_xe_file_target_read_file(N'\\?\UNC\host\share\x*.xel', NULL, NULL, NULL)` | ✓ |
| I4 | `EXEC sp_executesql N'EXEC sp_getapplock @Resource = N''r'', @LockMode = N''Exclusive'''` | ✗ line 1: EXEC sp_executesql would write |
| I5 | `CREATE TABLE #w (spid int, status nvarchar(60), login sysname, host sysname, blkby nvarchar(10), db sysname NULL, cmd nvarchar(32), cpu int, io bigint, lastbatch nvarchar(32), prog nvarchar(128), spid2 int, req int); ⏎ INSERT INTO #w EXE…` | ✓ |
| I6 | `DBCC CHECKTABLE (N'dbo.Orders') WITH NO_INFOMSGS` | ✗ line 1: DBCC CHECKTABLE would write |
| I7 | `SELECT * FROM sys.dm_exec_session_wait_stats WHERE session_id = @@SPID` | ✓ |
| I8 | `SET CONTEXT_INFO 0x01; SELECT 1;` | ✓ |
| I9 | `ALTER EVENT SESSION system_health ON SERVER DROP EVENT sqlserver.xml_deadlock_report` | ✗ line 1: ALTER would write |
| I10 | `KILL 'D5499C66-E398-45CA-BF7E-DC9C194B48CF'` | ✗ line 1: KILL is not allowed in read-only mode |
| I11 | `RESTORE VERIFYONLY FROM DISK = N'D:\Backup\Sales_FULL.bak'` | ✗ line 1: RESTORE is not allowed in read-only mode |
| I12 | `RESTORE LABELONLY FROM DISK = N'D:\Backup\Sales_FULL.bak'` | ✗ line 1: RESTORE is not allowed in read-only mode |
| I13 | `SELECT * FROM sys.dm_server_registry` | ✓ |
| I14 | `SELECT servicename, service_account FROM sys.dm_server_services` | ✓ |
| I15 | `SELECT HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER PERFORMANCE STATE') AS perf, HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW ANY ERROR LOG') AS errlog` | ✓ |
| I16 | `SET QUERY_GOVERNOR_COST_LIMIT 0; SELECT 1;` | ✓ |
| I17 | `SET FORCEPLAN ON; SELECT 1;` | ✓ |
| I18 | `DBCC TRACEON(3604)` | ✗ line 1: DBCC TRACEON would write |
| I19 | `SELECT * FROM dbo.Orders WITH (TABLOCK)` | ✓ |
| I20 | `SELECT * FROM dbo.Orders WITH (HOLDLOCK)` | ✓ |
| I21 | `SELECT * FROM dbo.Orders WITH (REPEATABLEREAD)` | ✓ |
| I22 | `SELECT 1 AS SERIALIZABLE` | ✓ |
| I23 | `OPEN SYMMETRIC KEY k DECRYPTION BY CERTIFICATE c; SELECT DECRYPTBYKEY(SecretCol) FROM dbo.Secrets;` | ✓ |
| I24 | `EXEC master.sys.sp_who2` | ✓ |
| I25 | `EXEC [master].[dbo].[sp_who2] 'active'` | ✓ |
| I26 | `SELECT TOP (10) qs.total_worker_time, st.text FROM sys.dm_exec_query_stats AS qs CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) AS st ORDER BY qs.total_worker_time DESC` | ✓ |
| I27 | `SELECT * FROM sys.dm_db_stats_histogram(OBJECT_ID(N'dbo.Orders'), 1)` | ✓ |
| I28 | `SELECT CAST(target_data AS xml) FROM sys.dm_xe_session_targets AS t JOIN sys.dm_xe_sessions AS s ON s.address = t.event_session_address WHERE s.name = N'system_health' AND t.target_name = N'ring_buffer'` | ✓ |
| I29 | `EXEC sp_spaceused N'dbo.Orders'` | ✓ |
| I30 | `EXEC sp_helpindex N'dbo.Orders'` | ✓ |
| J1 | `SELECT r.session_id, p.query_plan FROM sys.dm_exec_requests AS r CROSS APPLY sys.dm_exec_text_query_plan(r.plan_handle, r.statement_start_offset, r.statement_end_offset) AS p` | ✓ |
| J2 | `SELECT request_session_id, resource_type, request_mode, request_status FROM sys.dm_tran_locks` | ✓ |
| J3 | `SELECT counter_name, cntr_value FROM sys.dm_os_performance_counters WHERE counter_name = N'Batch Requests/sec'` | ✓ |
| J4 | `SELECT DB_NAME(vfs.database_id), mf.physical_name, vfs.io_stall_read_ms FROM sys.dm_io_virtual_file_stats(NULL, NULL) AS vfs JOIN sys.master_files AS mf ON mf.database_id = vfs.database_id AND mf.file_id = vfs.file_id` | ✓ |
| J5 | `SELECT TOP (10) q.query_id, p.plan_id, rs.avg_duration FROM sys.query_store_query AS q JOIN sys.query_store_plan AS p ON p.query_id = q.query_id JOIN sys.query_store_runtime_stats AS rs ON rs.plan_id = p.plan_id ORDER BY rs.avg_duration …` | ✓ |
| J6 | `SELECT actual_state_desc, readonly_reason FROM sys.database_query_store_options` | ✓ |
| J7 | `SELECT * FROM msdb.dbo.suspect_pages` | ✓ |
| J8 | `SELECT * FROM sys.dm_db_missing_index_details` | ✓ |
| J9 | `EXEC sp_create_plan_guide @name = N'g', @stmt = N'SELECT 1', @type = N'SQL', @module_or_batch = NULL, @params = NULL, @hints = N'OPTION (MAXDOP 1)'` | ✗ line 1: EXEC sp_create_plan_guide would write |
| J10 | `EXEC sys.sp_query_store_unforce_plan @query_id = 42, @plan_id = 7` | ✗ line 1: EXEC sys.sp_query_store_unforce_plan would write |
| J11 | `EXEC sp_updatestats` | ✗ line 1: EXEC sp_updatestats would write |
| J12 | `ALTER INDEX IX_Orders_CustomerId ON dbo.Orders REORGANIZE` | ✗ line 1: ALTER would write |
| J13 | `DBCC SHRINKDATABASE (N'Sales')` | ✗ line 1: DBCC SHRINKDATABASE would write |
| J14 | `DBCC UPDATEUSAGE (0)` | ✗ line 1: DBCC UPDATEUSAGE would write |
| J15 | `EXEC sp_change_users_login 'Report'` | ✗ line 1: EXEC sp_change_users_login would write |
| J16 | `EXEC sp_helpsrvrolemember N'sysadmin'` | ✗ line 1: EXEC sp_helpsrvrolemember would write |
| J17 | `EXEC sp_helplinkedsrvlogin` | ✗ line 1: EXEC sp_helplinkedsrvlogin would write |
| J18 | `EXEC msdb.dbo.sp_help_jobhistory @job_name = N'Nightly ETL'` | ✗ line 1: EXEC msdb.dbo.sp_help_jobhistory would write |
| J19 | `EXEC sp_MSforeachtable N'SELECT COUNT(*) FROM ?'` | ✗ line 1: EXEC sp_MSforeachtable would write |
| J20 | `SET SHOWPLAN_ALL ON` | ✓ |
| J21 | `DBCC CHECKCONSTRAINTS` | ✗ line 1: DBCC CHECKCONSTRAINTS would write |
| J22 | `EXEC sys.sp_query_store_remove_plan @plan_id = 7` | ✗ line 1: EXEC sys.sp_query_store_remove_plan would write |
| J23 | `KILL STATS JOB 12` | ✗ line 1: KILL is not allowed in read-only mode |
| J24 | `SELECT name, is_disabled FROM sys.server_event_sessions` | ✓ |
| J25 | `SELECT * FROM sys.fn_xe_file_target_read_file(N'C:\XE\capture*.xel', NULL, NULL, NULL)` | ✓ |
| J26 | `SELECT * FROM sys.fn_xe_file_target_read_file(N'https://acct.blob.core.windows.net/xe/x*.xel', NULL, NULL, NULL)` | ✓ |
| J27 | `SELECT * FROM dbo.Orders WITH (SNAPSHOT)` | ✓ |
| J28 | `SELECT * FROM sys.dm_exec_sessions WHERE program_name = N'sqlh'` | ✓ |
| J29 | `SELECT state_desc, is_read_committed_snapshot_on, snapshot_isolation_state_desc FROM sys.databases` | ✓ |
| J30 | `BACKUP DATABASE Sales TO DISK = N'D:\Backup\Sales_copy.bak' WITH COPY_ONLY` | ✗ line 1: BACKUP is not allowed in read-only mode |
| J31 | `BACKUP DATABASE Sales TO URL = N'https://acct.blob.core.windows.net/bak/Sales.bak'` | ✗ line 1: BACKUP is not allowed in read-only mode |
| J32 | `EXEC sp_executesql N'SELECT name FROM sys.objects WHERE object_id = @id', N'@id int', @id = 5` | ✗ line 1: EXEC sp_executesql would write |
| J33 | `DROP DATABASE Sales` | ✗ line 1: DROP would write |
| J34 | `ALTER DATABASE Sales SET SINGLE_USER WITH ROLLBACK IMMEDIATE` | ✗ line 1: ALTER would write |
| J35 | `EXEC sp_ineachdb N'SELECT DB_NAME()'` | ✗ line 1: EXEC sp_ineachdb would write |
| J36 | `SELECT pg.name FROM sys.plan_guides AS pg` | ✓ |
| J37 | `DBCC DBINFO WITH TABLERESULTS` | ✗ line 1: DBCC DBINFO would write |
| J38 | `DBCC IND (N'Sales', N'dbo.Orders', -1)` | ✗ line 1: DBCC IND would write |
| J39 | `SELECT * FROM fn_trace_gettable(N'\\host\share\x.trc', DEFAULT)` | ✓ |
| J40 | `SELECT * FROM ::fn_trace_gettable(N'\\host\share\x.trc', DEFAULT)` | ✓ |
| J41 | `SELECT * FROM master..fn_trace_gettable(N'\\host\share\x.trc', DEFAULT)` | ✓ |
| J42 | `SELECT * FROM [sys].[fn_get_audit_file](N'\\host\share\*.sqlaudit', DEFAULT, DEFAULT)` | ✓ |
| J43 | `SELECT * FROM fn_dump_dblog(NULL, NULL, N'DISK', 1, N'\\host\share\Sales_LOG.trn', DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, DEFAULT, …` | ✓ |
| J44 | `SELECT * FROM master.sys.fn_get_audit_file(N'/\host\share\*.sqlaudit', DEFAULT, DEFAULT)` | ✓ |
| J45 | `SELECT * FROM sys.dm_os_enumerate_filesystem(N'\/host\share', N'*')` | ✓ |
| J46 | `SELECT * FROM sys.fn_get_audit_file(N'C:\Audit\*.sqlaudit', N'\\host\share\x.sqlaudit', DEFAULT)` | ✓ |
| J47 | `SELECT * FROM sys.fn_xe_file_target_read_file(N'C:\XE\x*.xel', NULL, N'\\host\share\x_0_1.xel', 0)` | ✓ |
| J48 | `EXEC sp_configure 'cmdshell', 1` | ✗ line 1: sp_configure with @configvalue … changes a server setting (advice: sys.configurations) |
| W1 | `EXEC ('xp_cmdshell ''dir''')` | ✓ (with `--allow-write`) |
| W2 | `RESTORE DATABASE Sales FROM DISK = N'D:\Backup\Sales_FULL.bak' WITH REPLACE` | ✓ (with `--allow-write`) |
| W3 | `BACKUP DATABASE Sales TO DISK = N'\\host\share\Sales.bak' WITH COPY_ONLY` | ✓ (with `--allow-write`) |
| W4 | `SHUTDOWN WITH NOWAIT` | ✓ (with `--allow-write`) |
| W5 | `DECLARE @s nvarchar(100) = N'xp_cmdshell'; EXEC @s 'dir'` | ✓ (with `--allow-write`) |
| W6 | `EXEC sp_executesql N'SHUTDOWN'` | ✓ (with `--allow-write`) |
| W7 | `UPDATE #o SET Status = 0 FROM dbo.Orders AS #o WHERE #o.OrderId = 1` | ✓ (with `--allow-write`) |
| W8 | `EXEC sp_configure 'cmdshell', 1` | ✓ (with `--allow-write`) |
| W9 | `EXEC sp_configure 'Ole Auto', 1` | ✓ (with `--allow-write`) |
| W10 | `EXEC sp_configure 'clr en', 1` | ✓ (with `--allow-write`) |
| W11 | `WAITFOR DELAY '00:00:05'` | ✓ (with `--allow-write`) |

---

## Appendix B: the critiques' issues and how this document handles them

Two critiques reviewed the earlier draft: a safety lens (an over-privileged login driven carelessly or by prompt injection) and a DBA-practicality lens (a senior production DBA in an incident). One row per issue.

| # | Lens, severity | Issue | Handling |
|---|---|---|---|
| S1 | safety, high | Full `RESTORE` and `BACKUP` sat in `maintenance`, one `--allow-write` from replacing a database; nothing checked a `BACKUP` target, so a backup to a share copies the database off the server | **Adopted with a change.** `RESTORE DATABASE`/`LOG` (every form) and `BACKUP` to anything but local literals are in the deny tier (§7.1, D10); W2 and W3 show both run under `--allow-write` today. Change: `maintenance` keeps only a `COPY_ONLY` backup to a local path, since any other backup also disturbs the DBA's backup chain (§7.3); whether it keeps even that is D11's sub-choice, and `execute`'s first increment leaves it out (D15). |
| S2 | safety, high | The trusted-procedure check saw `EXECUTE AS` but not certificate signing, which elevates without changing the text or hash; the check and call aren't atomic; the hash covers only the top module | **Adopted with a change.** The same-batch check refuses any row in `sys.crypt_properties`, at every call; the time-of-check gap and nested code are stated (§7.7, D14). Change: the critique's "keep the MCP refusal" is not adopted; per the DBA lens (P5), trusted calls run in MCP under caps. What a trusted procedure can leave on a pooled session (a session-owned app lock, session context, an open key; this document adds them) is handled by a mechanism, not by telling procedures apart, which sqlh can't: until §10 item 5's reset test passes for all three, `sqlh mcp` closes the connection after every trusted call (§7.7, D14). |
| S3 | safety, medium | The deny tier was called "never run", but dynamic SQL, `EXEC @var`, synonyms and renamed objects evade a lexical name match under `--allow-write` | **Adopted.** Worded as an accident guard in §1, §2, §7.1 and DESIGN.md §7, with W1, W5 and W6 as evidence; what gets past what is stated by mode, `--allow unvetted` included. The refusal text claims only that no switch lifts it. D10(e) lets the deny tier read one literal with D13(b)'s rule in every mode (W1, W6), recommended once D13(b) exists; variables, concatenation and `EXEC @var` still get past. A stronger pass that also refuses dynamic forms under `--allow-write` is D10(c), not recommended. |
| S4 | safety, medium | Recursive classification of literal dynamic SQL could reopen MCP's `WAITFOR`/app-lock/session-context rule unless it inherits the mode | **Adopted.** The literal is classified under the caller's whole policy; tests named (§7.6, D13). |
| S5 | safety, medium | User functions are blanket reads, but a CLR `EXTERNAL_ACCESS`/`UNSAFE` function can exfiltrate from a `SELECT` | **Adopted.** Named in §2 and §4 (`disclosure`, `external_side_effect`) as a limit; §10's doc fixes put it in DESIGN.md §7's backstop note and the skill (Phase 8): enabling such CLR for an agent's login defeats read-only intent. Functions stay reads. |
| S6 | safety, medium | `CHECKDB ... ESTIMATEONLY` and `sp_estimate_data_compression_savings` were routed to read, though heavy and leaving snapshot or tempdb artifacts | **Adopted.** Both stay `maintenance`, out of read-only runs and MCP (§6.1, §7.3, D5(b)). |
| S7 | safety, low | The draft's working-tree probe binary predated `f905385`'s argument-check fixes, so its rows showed phantom bypasses | **Adopted.** Every probe re-run on `f905385` (Appendix A, 286 cases); the working-tree references are gone. |
| S8 | safety, low | `SET LOCK_TIMEOUT` up to the run's own value was scratch, so `--lock-timeout 0` allowed `-1`, an unbounded wait, contradicting the draft's D4(a) | **Adopted.** Folded into contention: a literal from 0 to the run's value, capped at 10,000 ms when the run has none, never −1 or a variable (§6.4, D3). |
| S9 | safety, low | The path check is a prefix test: mapped drives, `SUBST`, device paths and leading spaces slip past | **Adopted with a change.** The limit is documented (§2, §6.5); leading whitespace is trimmed, and any two leading separators in either direction (`\\`, `//`, `/\`, `\/`; J44, J45), `\\?\` and `\??\` count as non-local (§6.2). Not adopted: a configured drive-letter allowlist, since sqlh can't know which letters are shares on the server and the service account's rights are the boundary there. |
| P1 | DBA, high | The draft's eleven categories, set by its D8 switches rather than by policy; names a DBA wouldn't guess; refusals didn't name the switch | **Adopted.** Two layers (§5): six grantable kinds in DBA words, reason labels only in text; `read`, `scratch` and the deny tier are never switch names; every refusal names its switch (§6.9). D1 asks the shape first. |
| P2 | DBA, high | `write_policy` was the only profile dial, so a `deny` profile had no path to maintenance, tuning or KILL without DML; column E inverted the risk order | **Adopted with a change.** The profile's `allow` list holds under `deny` (§7.2, D11); E is derived from an explicit ranking (§5.4). Change: E's first increment offers only listed `tuning`, `kill` and `maintenance` (without its backup), not every listed kind (D15(c)). |
| P3 | DBA, high | The draft's `--allow CATEGORY` dropped the wrapper for every category: wrong for unvetted code, and it turns the `#alias` gap into a real write | **Adopted.** The wrapper per kind (§5.5): kept for `unvetted` and `remote`, dropped for the rest; without it, a `#x` write target the input doesn't create is refused. Added: `unvetted` can't share a run with a kind that drops the wrapper. |
| P4 | DBA, high | The sampler menu was too narrow (no per-query or per-session deltas); a client-side pause between batches is the general fix, conditional on contention being refused | **Adopted with a change.** A `:pause N` line between batches, in the CLI and in MCP's `sql`, capped and client-side; presets on top later; conditional on D3 (§6.6, D6). Change: one directive for both front ends instead of a separate `pause_seconds` argument, since MCP calls already take `GO` batches. |
| P5 | DBA, high | MCP, the agents' main path, had no `sp_WhoIsActive` substitute; trusted procedures were CLI-only; a mandatory hash conflicted with least privilege | **Adopted.** `activity` and `errorlog` as MCP tools and CLI commands in Phase 6 (§6.7, D7); trusted entries allowed in MCP under caps, on a connection closed after each call until the reset test passes; the hash pin optional and documented as needing `VIEW DEFINITION` (§7.7, D14). |
| P6 | DBA, medium | Refusing every non-literal path breaks the textbook system_health and audit reads; audits on shares are common; `fn_dump_dblog` with a UNC path passes and wasn't on the list | **Adopted.** Teach `system_health*.xel`; two-step advice for a variable; server-configured share paths as a later sub-option D4b(ii); unlisted `fn_`/`dm_` functions with a non-local literal fail closed, every literal argument of a listed function gets the local test, every spelling of a name counts (bare, `::`, `sys.`, `master..`, bracketed; J39–J43), and `fn_dump_dblog` is `unvetted` and `remote` (§6.5). |
| P7 | DBA, medium | The draft's D5 bundled a DBA-configured linked-server read with an agent-chosen SMB path, and hung on an untested MSDTC fact | **Adopted.** Split into D4a and D4b; the MSDTC test is first in §10, and D4a's recommendation (allow with a note) waits on it. |
| P8 | DBA, medium | The error log is oldest first for least-privilege logins, so `--max-rows 100` shows startup lines | **Adopted.** The `INSERT ... EXEC` plus `ORDER BY LogDate DESC` recipe goes into DESIGN.md §7 and the skill; an `errorlog` command and tool, newest first, with `--since` and `--search` (§6.7, D7). |
| P9 | DBA, medium | `RESTORE ...ONLY`, `KILL ... WITH STATUSONLY` and `CHECKCONSTRAINTS` are pure reads with no read-only path; Msg 6115 was mis-tagged [doc] | **Adopted.** The exact-shape class, alone and outside the wrapper (§6.2, D5(d)); `CHECKCONSTRAINTS` with a named table is read, unscoped it is `maintenance` (§6.1); 6115 and 3021 are now [live: sys.messages]. A file on a share, the case that matters, is `remote` under D4b(i): `--allow remote` runs it alone and without the wrapper (§5.5), and D4b's trade-offs say so. |
| P10 | DBA, medium | Excluding `program_name 'sqlh'` hides a sqlh write run that is the real head blocker; the MCP "open transaction" premise was wrong | **Adopted.** `activity` excludes only the report's own session and that `sqlh mcp`'s idle pooled ones; a busy pooled session, which can be a head blocker, shows, marked as its own (§6.7); the application name per run kind (D9(c)); the MCP sentence corrected from decision 17's live test (§3.2, F1). |
| P11 | DBA, medium | Contention advice named flags MCP can't set; SNAPSHOT is unusable under the wrapper but was listed as scratch | **Adopted with a change.** MCP advice names the profile's `lock_timeout`; SNAPSHOT becomes a proposed run setting, open pending a live test (§6.4, D3). Not adopted: a `lock_timeout` call argument, since one capped at the profile's value could only lower it, as the in-SQL literal already can. |
| P12 | DBA, medium | The everyday cross-database read (a cursor building `EXEC (@sql)`) stayed refused; `sp_MSforeachdb` is a poor basis | **Adopted.** sqlh-owned per-database execution, `--each-db` and MCP `databases` (§7.6, D13(d)); D13(c) dropped. |
| P13 | DBA, medium | Plan-regression fixes shared a switch with `RECONFIGURE` and cache clears; `CLEAR PROCEDURE_CACHE <handle>` was missing; flush got no useful advice | **Adopted.** A `tuning` kind (§7.4) including the one-handle `CLEAR PROCEDURE_CACHE`; `sp_query_store_flush_db`'s refusal carries the aggregation advice (§6.9). |
| P14 | DBA, low | A note arrives after the scan; the all-wildcard `DETAILED` call loads a struggling server | **Adopted with a change.** The literal all-wildcard `SAMPLED`/`DETAILED` call is refused with a rewrite advice (D9(d)). Change: unscoped `CHECKCONSTRAINTS` goes to `maintenance` rather than getting a cost refusal. |
| P15 | DBA, low | "Can user X read this?" has no read-only path | **Adopted as an option.** `EXECUTE AS USER\|LOGIN = N'<literal>'` in CLI read-only runs, refused in MCP until a reset test; a sqlh `perms` command as the alternative (D5(e)). |
| P16 | DBA, low | `DBCC MEMORYSTATUS` returns dozens of result sets, past `--max-results 20` | **Adopted.** On the list, documented as such, with the memory views taught first (§6.1, D5(c)). |
| P17 | DBA, low | The draft's build facts were stale (HEAD `eb5f568`, an uncommitted interim step) | **Adopted.** Restated against `f905385`; the "commit the interim step" follow-up is dropped. |
