# Security Policy

## Supported versions

Only the latest release gets security fixes. sqlh is at 0.x, so a fix ships in a new
release rather than as a patch to an older one.

## Reporting a vulnerability

Report a suspected vulnerability privately, through GitHub's private vulnerability
reporting for this repository: the **Security** tab, then **Report a vulnerability**.
Don't post vulnerability details in a public issue or discussion.

Include the affected version (`sqlh --version`) or commit, the impact, and short
steps to reproduce it. Don't send credentials, connection strings, production data
or other secrets.

sqlh is maintained by one person, so reports are reviewed as time allows, and no
response or fix timeline is guaranteed. Please coordinate public disclosure with the
maintainer.

## What counts

Read-only mode guards against accidents. It is not a security boundary (see the
README's safety section), so a statement that gets past the guard and changes data
under a login allowed to change it is a bug worth reporting, but the database's
permissions are what protect the data. Reports about the guard are welcome. Reports
about a password or other secret leaking to output, a log, another server or the
command line are the most important.
