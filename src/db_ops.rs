//! The database operations that are questions about a database rather
//! than changes to it: what SQLite would do with a statement, whether
//! the file is sound, how much space the file is wasting.
//!
//! These are the cheap commands an operator reaches for instead of
//! writing a script to ask the same thing. None of them is a fake: each
//! one runs the real SQLite operation and reports what SQLite said.
