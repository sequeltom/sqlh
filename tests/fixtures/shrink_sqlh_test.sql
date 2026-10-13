-- Gives the scratch database's space back after experiments that grew it (PLAN.md 3.4).
-- The live tests drop what they create, but a large write that is rolled back (the 2.5
-- harness-kill measurements) leaves the log at its peak size. Safe to rerun. Run with:
--   sqlh --profile test --allow-write -i tests/fixtures/shrink_sqlh_test.sql
USE sqlh_test;
GO
-- A scratch database needs no point-in-time recovery: in SIMPLE mode the log is reused after
-- each checkpoint instead of growing until a log backup.
IF (SELECT recovery_model_desc FROM sys.databases WHERE name = N'sqlh_test') <> N'SIMPLE'
    ALTER DATABASE sqlh_test SET RECOVERY SIMPLE;
GO
CHECKPOINT;
DBCC SHRINKFILE (sqlh_test_log, 64) WITH NO_INFOMSGS;
DBCC SHRINKFILE (sqlh_test, 64) WITH NO_INFOMSGS;
GO
SELECT name, type_desc, size * 8 / 1024 AS size_mb FROM sys.database_files;
