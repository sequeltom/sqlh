-- sqlh smoke test. Read-only and safe on any server (temp tables only).
-- Run:  sqlh -S <server> [-d <db>] [-U <login>] [-C] -i tests/smoke.sql --continue-on-error
-- Expect exit 1: batch 3 raises errors on purpose, to show error reporting and line mapping.

PRINT 'batch 1: data types';
SELECT CAST(1 AS tinyint) AS ti, CAST(-2 AS smallint) AS si, 3 AS i,
       CAST(9223372036854775807 AS bigint) AS bi, CAST(1 AS bit) AS b,
       CAST(0.1 AS real) AS r, CAST(0.1 AS float) AS f, CAST(1e20 AS float) AS f_big,
       CAST(12.50 AS decimal(18,2)) AS d, CAST(-1.2345 AS numeric(10,4)) AS n,
       CAST(12.5 AS money) AS m, CAST(-0.0001 AS smallmoney) AS sm;
SELECT CAST('2024-01-05' AS date) AS dt, CAST('09:12:00.1234567' AS time(7)) AS tm,
       CAST('2024-01-05T09:12:00.123' AS datetime) AS dtm,
       CAST('2024-01-05T09:12:00' AS smalldatetime) AS sdt,
       CAST('2024-01-05T09:12:00.1234567' AS datetime2(7)) AS dt2,
       CAST('2024-01-05T09:12:00.12' AS datetime2(2)) AS dt2_2,
       CAST('2024-01-05T09:12:00.1234567+01:00' AS datetimeoffset(7)) AS dto,
       CAST('2024-01-05T23:30:00-05:30' AS datetimeoffset(0)) AS dto_neg;
SELECT N'héllo wörld ✓' AS nv, 'plain' AS v, CAST('fixed' AS char(10)) AS c,
       CAST(N'x' AS nchar(3)) AS nc, 'tab' + CHAR(9) + 'nl' + CHAR(10) + 'end' AS escapes,
       'NULL' AS null_string, CAST(NULL AS int) AS null_int, '--looks like metadata' AS dashes,
       REPLICATE(CAST('x' AS varchar(max)), 2000) AS long_text,
       CAST(0xDEADBEEF AS varbinary(4)) AS vb,
       CAST(REPLICATE(CAST(0xAB AS varbinary(max)), 1000) AS varbinary(max)) AS long_bin,
       CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS guid,
       CAST('<a b="1"/>' AS xml) AS x, CAST(42 AS sql_variant) AS sv;
SELECT TOP (150) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n
FROM sys.all_columns;  -- 150 rows on any server: hits --max-rows
RAISERROR('a severity-10 message', 10, 1);
GO
PRINT 'batch 2: temp table writes are allowed in read-only mode; runs twice';
CREATE TABLE #t (id int PRIMARY KEY, v nvarchar(20));
INSERT INTO #t VALUES (1, N'a'), (2, N'b');
UPDATE #t SET v = N'c' WHERE id = 2;
SELECT id, v FROM #t ORDER BY id;
DROP TABLE #t;
GO 2
PRINT 'batch 3: errors';
SELECT 1 AS before_error;
SELECT 1/0 AS div_by_zero;
SELECT 'still runs after a statement error' AS after_error;
SELECT * FROM dbo.sqlh_no_such_table;
GO
