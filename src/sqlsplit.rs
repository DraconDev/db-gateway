//! Splitting a SQL script into statements.
//!
//! A `.sql` file is not a list of lines ending in semicolons: a
//! semicolon can sit inside a string literal, a comment can hold one,
//! and a trigger body holds a whole `BEGIN ... END` block. SQLite
//! itself knows where a statement ends -- `sqlite3_complete` answers
//! exactly that question -- so the splitter asks SQLite rather than
//! guessing.

/// Split a SQL script into its statements.
///
/// The split uses SQLite's own `sqlite3_complete`, so a semicolon
/// inside a string, a comment, or a trigger body does not end a
/// statement. Statements are returned with their trailing semicolon
/// removed and surrounding whitespace trimmed; a statement that holds
/// nothing but whitespace and comments is dropped, because there is
/// nothing to run.
pub fn split_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let bytes = script.as_bytes();
    while index < bytes.len() {
        // Look for the next semicolon. A semicolon inside a string or a
        // comment is not found here: the check below asks SQLite
        // whether the text up to it is a complete statement, and SQLite
        // knows about literals and comments.
        let Some(offset) = script[index..].find(';') else {
            break;
        };
        let end = index + offset;
        if !is_complete(&script[start..=end]) {
            // Not a statement boundary: the semicolon is inside a
            // string, a comment or a trigger body. Keep looking.
            index = end + 1;
            continue;
        }
        let statement = strip_leading_comments(&script[start..end]);
        if !statement.is_empty() {
            statements.push(statement.to_string());
        }
        index = end + 1;
        start = index;
    }
    // A trailing statement without a semicolon is still a statement.
    let tail = strip_leading_comments(&script[start..]);
    if !tail.is_empty() {
        statements.push(tail.to_string());
    }
    statements
}

/// Whether SQLite says the text is a complete statement.
///
/// SQLite wants a NUL-terminated C string, so the text is copied into
/// a buffer with a terminating zero rather than pointed at.
fn is_complete(text: &str) -> bool {
    let mut buffer = Vec::with_capacity(text.len() + 1);
    buffer.extend_from_slice(text.as_bytes());
    buffer.push(0);
    unsafe {
        rusqlite::ffi::sqlite3_complete(buffer.as_ptr() as *const std::os::raw::c_char) != 0
    }
}

/// The statement text with its leading comments removed.
///
/// A comment before a statement is not part of it, and sending the
/// comment along would put it in the statement the gateway records.
/// The comments INSIDE a statement are left alone: they are the
/// author's, and SQLite is happy to parse them.
fn strip_leading_comments(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            // A line comment runs to the end of the line.
            match after.find('\n') {
                Some(end) => {
                    rest = after[end + 1..].trim_start();
                    continue;
                }
                None => return "",
            }
        }
        if let Some(after) = rest.strip_prefix("/*") {
            // A block comment runs to its close. An unterminated one
            // holds no statement either way.
            match after.find("*/") {
                Some(end) => {
                    rest = after[end + 2..].trim_start();
                    continue;
                }
                None => return "",
            }
        }
        return rest.trim_end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_splits_at_its_semicolons() {
        let script = "CREATE TABLE a (id INTEGER);\nINSERT INTO a VALUES (1);\n";
        assert_eq!(
            split_statements(script),
            vec!["CREATE TABLE a (id INTEGER)", "INSERT INTO a VALUES (1)"]
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_does_not_end_a_statement() {
        let script = "INSERT INTO a VALUES ('a;b');\nSELECT 1;\n";
        let statements = split_statements(script);
        assert_eq!(statements.len(), 2, "{statements:?}");
        assert_eq!(statements[0], "INSERT INTO a VALUES ('a;b')");
    }

    #[test]
    fn a_comment_is_not_a_statement() {
        let script = "-- a comment\nSELECT 1;\n-- another\n";
        assert_eq!(split_statements(script), vec!["SELECT 1"]);
    }

    #[test]
    fn a_trailing_statement_without_a_semicolon_still_counts() {
        assert_eq!(
            split_statements("SELECT 1;\nSELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn an_empty_script_has_no_statements() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n\n  ").is_empty());
        assert!(split_statements("-- only a comment\n").is_empty());
        assert!(split_statements("/* only a comment */\n").is_empty());
    }

    #[test]
    fn a_statement_keeps_its_internal_structure() {
        // A trigger body holds its own semicolons, and the whole body is
        // one statement.
        let script = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET x = 1; END;\n";
        let statements = split_statements(script);
        assert_eq!(statements.len(), 1, "{statements:?}");
        assert!(statements[0].contains("BEGIN"));
        assert!(statements[0].contains("END"));
    }

    #[test]
    fn a_statement_after_a_comment_is_found() {
        // The comment is not part of the statement, but it must not
        // hide the statement that follows it.
        let script = "-- set up\nINSERT INTO a VALUES (1);\n";
        let statements = split_statements(script);
        assert_eq!(statements.len(), 1, "{statements:?}");
        assert!(statements[0].ends_with("INSERT INTO a VALUES (1)"));
    }
}
