//! sqlh: a SQL Server query tool for AI agents and harnesses. The binary (`src/main.rs`) only
//! calls `main`; everything else is here, so that other entry points (plan-history.md Phase 5) can
//! share the execution core (`run`).

mod app;
// build.rs's check of SQLH_VERSION, compiled here only for its tests.
#[cfg(test)]
mod build_version;
mod cli;
mod config;
mod console;
mod credman;
mod db;
mod dns;
mod engine;
mod error;
mod files;
mod guard;
mod mcp;
mod output;
mod profile;
mod run;
mod schema;
mod script;
mod secret;
mod signals;

/// The `sqlh` command: parses the arguments, runs the command and returns its exit code.
pub fn main() -> std::process::ExitCode {
    app::main()
}
