//! sqlh: a SQL Server query tool for AI agents and harnesses. The binary (`src/main.rs`) only
//! calls `main`; everything else is here, so that other entry points (PLAN.md Phase 5) can
//! share the execution core (`run`).

mod app;
mod cli;
mod config;
mod console;
mod credman;
mod db;
#[allow(dead_code)] // until sqlh mcp (Phase 5.3) calls it
mod engine;
mod error;
mod files;
mod guard;
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
