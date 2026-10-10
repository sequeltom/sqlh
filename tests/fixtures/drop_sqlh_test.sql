-- Removes the scratch database created by tests/fixtures/sqlh_test.sql:
--   sqlh --profile test --allow-write -i tests/fixtures/drop_sqlh_test.sql

IF DB_ID(N'sqlh_test') IS NOT NULL
BEGIN
    ALTER DATABASE sqlh_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE sqlh_test;
END;
GO
