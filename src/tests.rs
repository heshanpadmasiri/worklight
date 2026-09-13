//! Correctness tests. They run against real SQLite in isolated temporary
//! databases; no shared default database is used.

mod orchestrator_tests;
mod process_tests;
mod storage_tests;
mod support;
