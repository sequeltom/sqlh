-- Fixtures for the live tests in tests/cli.rs. Safe to rerun.
-- Creates the scratch database sqlh_test on a dev/test server. Run once with:
--   sqlh --profile test --allow-write -i tests/fixtures/sqlh_test.sql
-- Remove it with tests/fixtures/drop_sqlh_test.sql.

IF DB_ID(N'sqlh_test') IS NULL
    CREATE DATABASE sqlh_test;
GO
-- No point-in-time recovery for scratch data: the log is reused after each checkpoint
-- (tests/fixtures/shrink_sqlh_test.sql gives back space a big experiment took).
IF (SELECT recovery_model_desc FROM sys.databases WHERE name = N'sqlh_test') <> N'SIMPLE'
    ALTER DATABASE sqlh_test SET RECOVERY SIMPLE;
GO
USE sqlh_test;
GO
-- The scratch marker: the live tests write only to a database that has this table, and
-- then only to tables of their own (dbo.probe_*), which they drop. One test also changes
-- sqlh_test's compatibility level, and puts it back (owner decision 21).
IF OBJECT_ID(N'dbo.sqlh_scratch', N'U') IS NULL
    CREATE TABLE dbo.sqlh_scratch (id int NOT NULL);
-- Replaced by the per-test tables.
IF OBJECT_ID(N'dbo.check_probe', N'U') IS NOT NULL
    DROP TABLE dbo.check_probe;
-- A sequence, for guard tests of NEXT VALUE FOR (which a rollback can't undo).
IF OBJECT_ID(N'dbo.review_seq', N'SO') IS NULL
    CREATE SEQUENCE dbo.review_seq AS bigint START WITH 1;
GO
