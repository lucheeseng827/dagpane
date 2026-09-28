//! SQL cells: a front end that **lowers to the nine verbs** and refuses everything else.
//!
//! ```toml
//! [[cell]]
//! name = "by_region"
//! sql = """
//!   select region, sum(amount) as revenue
//!   from sales
//!   where amount >= :min_amount
//!   group by region
//!   order by revenue desc
//! """
//! ```
//!
//! That cell compiles to exactly the steps somebody would have written by hand — a `filter`, a
//! `group_by`, a `sort` — and to nothing else. There is no SQL engine here, no second
//! evaluator, and no second set of null rules. ADR-0005 has the argument; three things about
//! it belong at the top of the file.
//!
//! # The edges are resolved, not scanned
//!
//! `ROADMAP.md` §6 put SQL last for one reason: the manifest's job is to produce **edges**, and
//! pulling a table name out of SQL text with a regular expression is wrong eventually — against
//! a comment containing a table name, a quoted identifier, an alias. A missed edge is a cell
//! that does not recompute when it should, which is a stale number on a page that looks
//! correct.
//!
//! So nothing here scans. The statement is **parsed**, its tables are resolved against their
//! aliases, and the resulting names become edges: `from sales` makes this cell depend on the
//! cell `sales`, exactly as `from = "sales"` would. A parameter is written `:name` and is an
//! edge on the same terms as `derive`'s `$name` — explicit, and the only kind of name in the
//! statement that can reach outside the tables it reads.
//!
//! # A closed grammar, so "refuse what you cannot resolve" is structural
//!
//! The gate `ROADMAP.md` §6 set is "a parser that can be shown to reject what it cannot
//! resolve, rather than guess". The parser here accepts **only** the subset that lowers, so
//! that property is a consequence of the grammar rather than a list of things to remember to
//! reject. A third-party SQL parser would have inverted it: it would accept all of SQL and
//! leave this file holding a blocklist, and a blocklist of SQL features is never complete —
//! which is the same "wrong eventually" the ROADMAP says about regular expressions, one level
//! up. That is why this is hand-written despite the ROADMAP anticipating a dependency, and the
//! cost is written down in ADR-0005: this is not SQL, it is a dialect that fits on a page.
//!
//! Everything outside it is refused **by name**, with what to write instead where there is
//! something: subqueries, `with`, `union`, `having`, `distinct`, window functions, `right` and
//! `full` joins, `cast`, `like`, and a comma-separated `from`.
//!
//! # What it lowers to
//!
//! | SQL | verb |
//! |---|---|
//! | `from t` | the cell's `from` |
//! | `join u on t.k = u.k` | `join` |
//! | `where a >= :p` | `filter` |
//! | `where <anything else>` | `derive` a boolean, then `filter` it |
//! | `x * 2 as y` | `derive` |
//! | `group by a` + `sum(b)` | `group_by` |
//! | `order by c desc` | `sort` |
//! | `limit n` | `limit` |
//! | the select list | `select` |
//!
//! Because the verbs are the whole target, their semantics are the whole semantics. Nulls,
//! type rules, division by zero, the duplicate-key refusal on a join — all of it is what
//! [`dagpane_core::transform`] and [`dagpane_core::expr`] already do and already document. A
//! SQL cell cannot behave differently from the pipeline it compiles to, because it *is* that
//! pipeline by the time anything runs.

use std::fmt;

mod lower;
mod parse;

pub(crate) use lower::lower;

/// Everything that can be wrong with a SQL cell.
///
/// Three kinds, and the split is the point: a reader who has written something outside the
/// dialect wants to be told it is outside the dialect, not that it is a syntax error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqlError {
    /// Not the grammar this dialect accepts.
    Syntax {
        /// Byte offset into the statement.
        at: usize,
        /// What was expected there.
        message: String,
    },
    /// Valid SQL, and not something this dialect lowers. Names what to write instead where
    /// there is something to name.
    Unsupported {
        /// Byte offset into the statement.
        at: usize,
        /// What is not supported, and the alternative.
        message: String,
    },
    /// It parses and it is in the dialect, but it does not mean anything: a qualifier naming
    /// no table, a select-list item that is neither grouped nor aggregated, a column on both
    /// sides of a join.
    Meaning {
        /// Byte offset into the statement.
        at: usize,
        /// What does not add up.
        message: String,
    },
}

impl SqlError {
    /// Where in the statement, as a byte offset.
    pub fn at(&self) -> usize {
        match self {
            SqlError::Syntax { at, .. }
            | SqlError::Unsupported { at, .. }
            | SqlError::Meaning { at, .. } => *at,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Syntax { message, .. } => f.write_str(message),
            SqlError::Unsupported { message, .. } => f.write_str(message),
            SqlError::Meaning { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for SqlError {}

/// The offending line of a statement with a caret under the offset.
///
/// A SQL cell is a multi-line string, and "at character 143" is a number nobody can use. This
/// is what the manifest error prints under the message.
pub fn point_at(sql: &str, at: usize) -> String {
    let at = at.min(sql.len());
    let start = sql[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = sql[at..].find('\n').map(|i| at + i).unwrap_or(sql.len());
    let line = sql[..at].matches('\n').count() + 1;
    let text = sql[start..end].trim_end();
    // Trimming the left has to move the caret with it, or a query indented inside a TOML
    // triple-quoted string points at the wrong character.
    let indent = text.len() - text.trim_start().len();
    let column = sql[start..at].chars().count().saturating_sub(indent);
    let head = format!("  line {line}: ");
    format!(
        "{head}{}\n{}{}^",
        text.trim_start(),
        " ".repeat(head.len()),
        " ".repeat(column)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caret_points_at_the_right_line_and_column() {
        let sql = "select a\nfrom t\nwhere amonut > 1";
        let at = sql.find("amonut").unwrap();
        let shown = point_at(sql, at);
        assert!(shown.contains("line 3: where amonut > 1"), "{shown}");
        let caret = shown.lines().nth(1).unwrap();
        let text = shown.lines().next().unwrap();
        assert_eq!(
            caret.find('^'),
            text.find("amonut"),
            "the caret is under the word:\n{shown}"
        );
    }

    #[test]
    fn an_indented_line_still_points_at_the_word() {
        // What a TOML triple-quoted query actually looks like.
        let sql = "\n  select a\n  from t\n  where amonut > 1\n";
        let at = sql.find("amonut").unwrap();
        let shown = point_at(sql, at);
        let caret = shown.lines().nth(1).unwrap();
        let text = shown.lines().next().unwrap();
        assert_eq!(caret.find('^'), text.find("amonut"), "\n{shown}");
    }
}
