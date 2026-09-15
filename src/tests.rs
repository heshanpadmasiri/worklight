//! Correctness tests. They run against real SQLite in isolated temporary
//! databases; no shared default database is used.

mod agent_tests;
mod cli_tests;
mod orchestrator_tests;
mod process_tests;
mod storage_tests;
mod support;
