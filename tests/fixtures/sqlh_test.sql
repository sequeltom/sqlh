-- Fixtures for the live tests in tests/cli.rs. Safe to rerun.
-- Creates the scratch database sqlh_test on a dev/test server. Run once with:
--   sqlh --profile test --allow-write -i tests/fixtures/sqlh_test.sql
-- Remove it with tests/fixtures/drop_sqlh_test.sql.

IF DB_ID(N'sqlh_test') IS NULL
    CREATE DATABASE sqlh_test;
GO
USE sqlh_test;
GO
-- check_never_executes: --check must never insert here.
IF OBJECT_ID(N'dbo.check_probe', N'U') IS NULL
    CREATE TABLE dbo.check_probe (id int NOT NULL);
-- A sequence, for guard tests of NEXT VALUE FOR (which a rollback can't undo).
IF OBJECT_ID(N'dbo.review_seq', N'SO') IS NULL
    CREATE SEQUENCE dbo.review_seq AS bigint START WITH 1;
GO
