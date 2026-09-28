//! A read-only Postgres query. **Behind the `sql` feature; off by default.**
//!
//! A whole-result `SELECT` and nothing else. No pushdown, no incremental read, no cursor: the
//! engine's contract with a value is *produce it, digest it, compare the digest*, and a
//! source that produced half a table would be producing a different value each time it was
//! asked. The row cap this runtime is honest about — roughly a hundred thousand — is the
//! reason aggregating upstream is the advice rather than a limitation to work around, and the
//! query is where a person does that.

use std::sync::Arc;

use dagpane_core::frame::{ColumnHint, Frame, FrameBuilder};
use dagpane_core::ColumnType;
use postgres::types::Type;
use postgres::{Client, NoTls, Row};

use crate::error::SourceError;
use crate::version::{Version, VersionPart};
use crate::Source;

/// A read-only query against a Postgres database.
///
/// The connection string is **never** printed: [`Source::describe`] renders the host, port
/// and database from it and nothing else, because a DSN carries a password and every error
/// this type produces ends up somewhere a person can read.
#[derive(Clone, Debug)]
pub struct SqlSource {
    dsn: String,
    query: String,
    /// A column whose maximum is watched by [`Source::version`], if the operator named one.
    watch: Option<String>,
}

impl SqlSource {
    /// A query against a database.
    ///
    /// # Errors
    ///
    /// [`SourceError::Misconfigured`] for a statement that is not a single read. See
    /// [`is_read_only`] for exactly what that check does and — more importantly — what it
    /// does not.
    pub fn new(dsn: impl Into<String>, query: impl Into<String>) -> Result<SqlSource, SourceError> {
        let dsn = dsn.into();
        let raw = query.into();
        if !is_read_only(&raw) {
            return Err(SourceError::Misconfigured {
                source: describe_dsn(&dsn),
                reason: "a source's query is one `select` or `with`, and carries no `;` — this \
                         is a guard against a typo, never against a hostile manifest"
                    .to_string(),
            });
        }
        // Stored WITHOUT the trailing `;` the check above tolerates. Every other method wraps
        // this string in a subquery — `select count(*) from (<query>) as _dagpane_src` — and a
        // semicolon in the middle of one is a syntax error, so keeping it made a source that
        // validated cleanly and then failed on every read with a bare "db error".
        let query = query_without_terminator(&raw).to_string();
        Ok(SqlSource {
            dsn,
            query,
            watch: None,
        })
    }

    /// Watch this column's maximum in [`Source::version`], alongside the row count.
    ///
    /// **Name one.** Without it the version is the row count alone, and a row count is
    /// unchanged by every `UPDATE` — which is the stale-but-equal case [`Version`] warns
    /// about, and the common one for a table of orders whose statuses change. An
    /// `updated_at` or a monotonic id closes it.
    pub fn watching(mut self, column: impl Into<String>) -> SqlSource {
        self.watch = Some(column.into());
        self
    }

    /// The query, as given.
    pub fn query(&self) -> &str {
        &self.query
    }

    fn connect(&self) -> Result<Client, SourceError> {
        Client::connect(&self.dsn, NoTls).map_err(|e| SourceError::Unreachable {
            source: self.describe(),
            reason: e.to_string(),
        })
    }
}

impl Source for SqlSource {
    fn describe(&self) -> String {
        format!("query on {}", describe_dsn(&self.dsn))
    }

    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
        // `prepare` asks the server for the result's column types without running the query,
        // which is the one place in this crate where `schema` is genuinely cheaper than
        // `load` — so it is used rather than reading a row and looking at it.
        let mut client = self.connect()?;
        let statement = client
            .prepare(&self.query)
            .map_err(|e| SourceError::Unreadable {
                source: self.describe(),
                reason: e.to_string(),
            })?;

        statement
            .columns()
            .iter()
            .map(|c| {
                column_type(c.type_())
                    .map(|ty| (c.name().to_string(), ty))
                    .ok_or_else(|| SourceError::Unreadable {
                        source: self.describe(),
                        reason: unmapped(c.name(), c.type_()),
                    })
            })
            .collect()
    }

    fn load(&self, mut into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
        let mut client = self.connect()?;
        let rows = client
            .query(&self.query, &[])
            .map_err(|e| SourceError::Unreadable {
                source: self.describe(),
                reason: e.to_string(),
            })?;

        // The result carries its own column types even when it has no rows, so an empty
        // answer still builds a frame with the right schema rather than a zero-column one —
        // which is what makes an empty table render as an empty table and not as an error.
        let statement_columns = match rows.first() {
            Some(row) => row
                .columns()
                .iter()
                .map(|c| (c.name().to_string(), c.type_().clone()))
                .collect::<Vec<_>>(),
            None => {
                // The connection that answered the query is still open, so ask it rather than
                // opening a second one: a `prepare` on a fresh connection would pay another
                // round of TLS and authentication to learn what this one already knows.
                let statement =
                    client
                        .prepare(&self.query)
                        .map_err(|e| SourceError::Unreadable {
                            source: self.describe(),
                            reason: e.to_string(),
                        })?;
                statement
                    .columns()
                    .iter()
                    .map(|c| (c.name().to_string(), c.type_().clone()))
                    .collect()
            }
        };

        let mut plan = Vec::with_capacity(statement_columns.len());
        for (name, ty) in &statement_columns {
            let column_ty = column_type(ty).ok_or_else(|| SourceError::Unreadable {
                source: self.describe(),
                reason: unmapped(name, ty),
            })?;
            // The row count is known exactly — the result set is already in hand — so a
            // streaming builder can size its buffers once instead of growing them. `distinct`
            // stays unknown: counting it would mean a pass over every value to save a backend
            // a decision it can make for itself, and guessing it from a Postgres type name is
            // exactly the guess the hint exists to replace with a measurement.
            let hint = ColumnHint {
                rows: Some(rows.len()),
                distinct: None,
            };
            let col = into.begin_column(name, column_ty, hint);
            plan.push((col, column_ty, ty.clone()));
        }

        for row in &rows {
            for (i, (col, column_ty, pg_ty)) in plan.iter().enumerate() {
                push_cell(into.as_mut(), *col, *column_ty, pg_ty, row, i).map_err(|reason| {
                    SourceError::Unreadable {
                        source: self.describe(),
                        reason,
                    }
                })?;
            }
        }

        Ok(into.finish())
    }

    /// The row count, and the maximum of a watched column if one was named.
    ///
    /// Both come from one query wrapped around the source's own, so the version is of
    /// *exactly* what the source would load — not of a table the query happens to mention.
    /// A query with a `limit`, a join or a `where` is versioned by what it returns.
    fn version(&self) -> Result<Version, SourceError> {
        let mut client = self.connect()?;
        let probe = match &self.watch {
            Some(column) => format!(
                "select count(*)::int8, max({})::text from ({}) as _dagpane_src",
                quote_ident(column),
                self.query
            ),
            None => format!(
                "select count(*)::int8, null::text from ({}) as _dagpane_src",
                self.query
            ),
        };

        let row = client
            .query_one(&probe, &[])
            .map_err(|e| SourceError::Unreadable {
                source: self.describe(),
                reason: e.to_string(),
            })?;
        let count: i64 = row.get(0);
        let watermark: Option<String> = row.get(1);

        Ok(Version::of(
            b's',
            &[
                VersionPart::Num(count.max(0) as u64),
                watermark
                    .as_deref()
                    .map_or(VersionPart::Absent, VersionPart::Text),
            ],
        ))
    }
}

/// Postgres type → the four column types this runtime has.
///
/// Deliberately short, and **everything absent from it is an error naming the column** rather
/// than a silent conversion. Two kinds of thing are absent, for two different reasons, and
/// the second was found by running this against a database rather than by reasoning about it:
///
///   * `numeric`, `json`, arrays — nothing among four types holds them losslessly. A
///     `numeric` rendered as text sorts lexically, so `9` comes after `10` in a bar chart and
///     nobody can see why.
///   * `date`, `timestamp`, `timestamptz`, `uuid` — these *look* like text and are not. The
///     client speaks Postgres's binary protocol, where a date is a day offset rather than a
///     string, so reading one "as text" fails at the wire. Adding a date library to paper
///     over that would put a calendar in the crate that has four column types on purpose.
///     Casting in the query is one word, yields ISO-8601 — which sorts correctly as a
///     string, the only reason a date column is usable here at all — and leaves the format
///     with the person who knows what the pane is for.
fn column_type(ty: &Type) -> Option<ColumnType> {
    match *ty {
        Type::INT2 | Type::INT4 | Type::INT8 => Some(ColumnType::Int),
        Type::FLOAT4 | Type::FLOAT8 => Some(ColumnType::Float),
        Type::BOOL => Some(ColumnType::Bool),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => Some(ColumnType::Text),
        _ => None,
    }
}

/// What to tell somebody whose column this source will not read.
///
/// The advice is the message. "Unsupported type" sends a person to the source of this crate;
/// the exact expression to paste sends them back to their own query.
fn unmapped(name: &str, ty: &Type) -> String {
    format!(
        "column {name:?} is `{}`, which is not one of this runtime's four column types \
         (int, float, bool, text); select it as `{name}::text` — ISO-8601 sorts correctly as \
         a string — or cast it to a number",
        ty.name()
    )
}

/// One cell, widened to the column's type. Widening only: an `int2` into an `int8` column is
/// exact, and nothing here narrows.
fn push_cell(
    into: &mut dyn FrameBuilder,
    col: usize,
    column_ty: ColumnType,
    pg_ty: &Type,
    row: &Row,
    i: usize,
) -> Result<(), String> {
    match column_ty {
        ColumnType::Int => {
            let v = match *pg_ty {
                Type::INT2 => row.get::<_, Option<i16>>(i).map(i64::from),
                Type::INT4 => row.get::<_, Option<i32>>(i).map(i64::from),
                _ => row.get::<_, Option<i64>>(i),
            };
            into.push_int(col, v);
        }
        ColumnType::Float => {
            let v = match *pg_ty {
                Type::FLOAT4 => row.get::<_, Option<f32>>(i).map(f64::from),
                _ => row.get::<_, Option<f64>>(i),
            };
            into.push_float(col, v);
        }
        ColumnType::Bool => into.push_bool(col, row.get::<_, Option<bool>>(i)),
        ColumnType::Text => {
            // Dates and timestamps arrive as text because the query asked for it: the source
            // reads every non-scalar type through Postgres's own `::text` rendering rather
            // than through a date library this runtime does not have. ISO-8601 sorts
            // correctly as a string, which is why a date column is usable at all.
            let v: Option<String> = match *pg_ty {
                Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row.get(i),
                _ => row
                    .try_get::<_, Option<String>>(i)
                    .map_err(|e| format!("column {i} could not be read as text: {e}"))?,
            };
            match v.as_deref() {
                Some(s) => into.push_text(col, Some(s)),
                None => into.push_text(col, None),
            }
        }
    }
    Ok(())
}

/// One statement with at most one trailing `;` removed, and the surrounding space with it.
///
/// `trim_end_matches(';')` would take several, which would let `select 1;;;` through as one
/// statement. One terminator is a habit; a run of them is the typo this is guarding against.
fn query_without_terminator(query: &str) -> &str {
    let trimmed = query.trim();
    trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end()
}

/// Is this one read-only statement?
///
/// **What this is.** A guard against a typo — a manifest that says `delete from` where it
/// meant `select from`, or two statements where one was intended.
///
/// **What this is not, and the distinction matters.** It is *not* a security boundary. A
/// `select` can call a volatile function that writes; a comment can hide a keyword from a
/// prefix check; and anyone who can edit the manifest could just as easily change the DSN.
/// The boundary that actually holds is the **database role**: connect as a role with `select`
/// and nothing else, which the documentation says and this function cannot enforce. Writing
/// the check to look like a boundary it is not would be worse than not having it.
pub fn is_read_only(query: &str) -> bool {
    let trimmed = query_without_terminator(query);
    if trimmed.contains(';') {
        return false;
    }
    let head = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(head.as_str(), "select" | "with" | "table" | "values")
}

/// A DSN rendered as the host, port and database it names — never its password.
fn describe_dsn(dsn: &str) -> String {
    // A URL-shaped DSN: `postgres://user:pass@host:port/db?params`.
    if let Some(rest) = dsn
        .strip_prefix("postgres://")
        .or_else(|| dsn.strip_prefix("postgresql://"))
    {
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        return format!("postgres://{after_userinfo}");
    }

    // A keyword DSN: `host=... user=... password=...`. Keep the keys that are not secrets.
    let kept: Vec<&str> = dsn
        .split_whitespace()
        .filter(|kv| {
            let key = kv
                .split('=')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            matches!(key.as_str(), "host" | "hostaddr" | "port" | "dbname")
        })
        .collect();
    if kept.is_empty() {
        // Nothing recognisable, so nothing is echoed: a DSN this function does not
        // understand is the case where printing "what was left" leaks the password.
        "a postgres database".to_string()
    } else {
        format!("postgres {}", kept.join(" "))
    }
}

/// A SQL identifier, quoted. The only value this crate interpolates into a statement, and it
/// is an operator-supplied column name rather than anything a viewer can reach.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dsn_never_shows_its_password() {
        for dsn in [
            "postgres://app:hunter2@db.internal:5432/sales",
            "postgresql://app:hunter2@db.internal:5432/sales?sslmode=require",
        ] {
            let shown = describe_dsn(dsn);
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("db.internal:5432/sales"), "{shown}");
        }

        let kw = describe_dsn("host=db.internal port=5432 dbname=sales user=app password=hunter2");
        assert!(!kw.contains("hunter2"), "{kw}");
        assert!(kw.contains("db.internal"), "{kw}");
        assert!(
            !kw.contains("user=app"),
            "a username is not needed to identify a source: {kw}"
        );

        // Unparseable: echo nothing rather than echo the half that might be the secret.
        assert_eq!(describe_dsn("something opaque"), "a postgres database");
    }

    #[test]
    fn the_typo_guard_accepts_reads_and_refuses_the_obvious_mistakes() {
        assert!(is_read_only("select * from sales"));
        assert!(is_read_only("  SELECT 1  "));
        assert!(is_read_only("with t as (select 1) select * from t"));
        assert!(is_read_only("select * from sales;"));

        assert!(!is_read_only("delete from sales"));
        assert!(!is_read_only("update sales set amount = 0"));
        assert!(!is_read_only("select 1; drop table sales"));
        assert!(!is_read_only(""));
    }

    #[test]
    fn an_identifier_with_a_quote_in_it_cannot_close_its_own_quoting() {
        assert_eq!(quote_ident("updated_at"), "\"updated_at\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
