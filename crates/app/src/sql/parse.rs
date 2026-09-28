//! The dialect: a lexer and a recursive-descent parser for exactly the statements that lower.
//!
//! Nothing here knows what a cell is. It turns text into a [`Select`], or says why it will
//! not — and "will not" is most of the file, because the value of a closed grammar is the
//! refusals it makes possible. Everything it accepts, [`super::lower`] can lower.

use dagpane_core::expr::{BinOp, Func, UnOp};
use dagpane_core::transform::{Agg, How};
use dagpane_core::Value;

use super::SqlError;

// ── the tree ───────────────────────────────────────────────────────────────────────────

/// A parsed statement.
#[derive(Clone, Debug)]
pub(super) struct Select {
    /// The select list. Empty with `star` set is a bare `select *`.
    pub items: Vec<Item>,
    /// Whether the list began with `*`.
    pub star: bool,
    /// Where `star` was written, for an error about it.
    pub star_at: usize,
    pub from: Table,
    pub join: Option<JoinClause>,
    pub filter: Option<Sql>,
    pub group_by: Vec<Ref>,
    pub order_by: Option<(Ref, bool)>,
    pub limit: Option<usize>,
}

/// A table in the `from` or a `join`: the cell it names, and the alias the statement uses.
#[derive(Clone, Debug)]
pub(super) struct Table {
    pub cell: String,
    pub alias: String,
    pub at: usize,
}

#[derive(Clone, Debug)]
pub(super) struct JoinClause {
    pub how: How,
    pub table: Table,
    /// The equality pairs of the `on`, left side first. A conjunction and nothing else.
    pub on: Vec<(Ref, Ref)>,
    pub at: usize,
}

/// One select-list item.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub value: Sql,
    /// The `as` name, or `None` for a bare column reference that keeps its own name.
    pub alias: Option<String>,
    pub at: usize,
}

/// A column reference, possibly qualified by a table alias.
#[derive(Clone, Debug)]
pub(super) struct Ref {
    pub qualifier: Option<String>,
    pub name: String,
    pub at: usize,
}

/// A SQL expression. Seven shapes: `case`, `between`, `in`, `is null` and `||` are desugared
/// while parsing, because each of them is spelling for something the expression language
/// already says and an AST node per spelling would be a node the lowering has to handle.
#[derive(Clone, Debug)]
pub(super) enum Sql {
    Lit(Value),
    Ref(Ref),
    Param(String),
    Unary(UnOp, Box<Sql>),
    Binary(BinOp, Box<Sql>, Box<Sql>),
    Call(Func, Vec<Sql>),
    Agg {
        func: Agg,
        /// `None` is `count(*)`.
        arg: Option<Box<Sql>>,
        at: usize,
    },
}

// ── errors ─────────────────────────────────────────────────────────────────────────────

fn syntax(at: usize, message: impl Into<String>) -> SqlError {
    SqlError::Syntax {
        at,
        message: message.into(),
    }
}

fn unsupported(at: usize, message: impl Into<String>) -> SqlError {
    SqlError::Unsupported {
        at,
        message: message.into(),
    }
}

// ── lexing ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A bare identifier or a keyword; case as written, because a column name is
    /// case-sensitive and a keyword is not.
    Word(String),
    /// A `"quoted identifier"`, which is never a keyword.
    Quoted(String),
    Lit(Value),
    Param(String),
    Sym(&'static str),
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    at: usize,
}

/// Two-character symbols first, so `<=` never lexes as `<` then `=`.
const SYMBOLS: [&str; 17] = [
    "<=", ">=", "<>", "!=", "||", "(", ")", ",", ".", "*", "+", "-", "/", "%", "=", "<", ">",
];

fn lex(sql: &str) -> Result<Vec<Token>, SqlError> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Comments, which are the first thing a regular expression over SQL gets wrong.
        if sql[i..].starts_with("--") {
            i = sql[i..].find('\n').map(|n| i + n).unwrap_or(bytes.len());
            continue;
        }
        if sql[i..].starts_with("/*") {
            let end = sql[i + 2..]
                .find("*/")
                .ok_or_else(|| syntax(i, "this /* comment is never closed"))?;
            i += end + 4;
            continue;
        }
        let at = i;
        match c {
            b'\'' => {
                let (text, next) = quoted(sql, i, b'\'')?;
                out.push(Token {
                    tok: Tok::Lit(Value::text(text)),
                    at,
                });
                i = next;
            }
            b'"' => {
                let (name, next) = quoted(sql, i, b'"')?;
                out.push(Token {
                    tok: Tok::Quoted(name),
                    at,
                });
                i = next;
            }
            b':' => {
                let (name, next) = word(sql, i + 1)
                    .ok_or_else(|| syntax(at, "`:` must be followed by a parameter name"))?;
                out.push(Token {
                    tok: Tok::Param(name),
                    at,
                });
                i = next;
            }
            b'0'..=b'9' => {
                let (value, next) = number(sql, i)?;
                out.push(Token {
                    tok: Tok::Lit(value),
                    at,
                });
                i = next;
            }
            _ => {
                if let Some((name, next)) = word(sql, i) {
                    out.push(Token {
                        tok: Tok::Word(name),
                        at,
                    });
                    i = next;
                    continue;
                }
                let sym = SYMBOLS
                    .iter()
                    .find(|s| sql[i..].starts_with(**s))
                    .ok_or_else(|| {
                        syntax(
                            at,
                            format!(
                                "`{}` is not something this dialect can contain",
                                sql[i..].chars().next().unwrap_or('?')
                            ),
                        )
                    })?;
                out.push(Token {
                    tok: Tok::Sym(sym),
                    at,
                });
                i += sym.len();
            }
        }
    }
    Ok(out)
}

fn word(sql: &str, from: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let first = *bytes.get(from)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = from + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    Some((sql[from..end].to_string(), end))
}

/// A `'string'` or a `"quoted identifier"`, with the delimiter doubled to include one.
fn quoted(sql: &str, from: usize, delim: u8) -> Result<(String, usize), SqlError> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut i = from + 1;
    loop {
        let Some(&c) = bytes.get(i) else {
            return Err(syntax(
                from,
                format!("this {} is never closed", delim as char),
            ));
        };
        if c == delim {
            if bytes.get(i + 1) == Some(&delim) {
                out.push(delim as char);
                i += 2;
                continue;
            }
            return Ok((out, i + 1));
        }
        let ch = sql[i..].chars().next().expect("indexed at a boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn number(sql: &str, from: usize) -> Result<(Value, usize), SqlError> {
    let bytes = sql.as_bytes();
    let mut end = from;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut float = false;
    if bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
        float = true;
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if matches!(bytes.get(end), Some(b'e') | Some(b'E')) {
        let mut probe = end + 1;
        if matches!(bytes.get(probe), Some(b'+') | Some(b'-')) {
            probe += 1;
        }
        if bytes.get(probe).is_some_and(u8::is_ascii_digit) {
            float = true;
            end = probe;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
    }
    let raw = &sql[from..end];
    if float {
        raw.parse()
            .map(|v| (Value::float(v), end))
            .map_err(|_| syntax(from, format!("`{raw}` is not a number")))
    } else {
        raw.parse()
            .map(|v| (Value::int(v), end))
            .map_err(|_| syntax(from, format!("`{raw}` does not fit in a 64-bit integer")))
    }
}

// ── parsing ────────────────────────────────────────────────────────────────────────────

/// Keywords this dialect knows about only in order to refuse them well. A reader who wrote
/// `having` wants to be told `having` is not supported, not that `having` is an unexpected
/// word.
const REFUSED: [(&str, &str); 14] = [
    (
        "with",
        "a `with` clause is not supported; write each step as its own cell, which is what a CTE is",
    ),
    (
        "union",
        "`union` is not supported; there is no way to stack two tables in this dialect",
    ),
    (
        "having",
        "`having` is not supported; filter the grouped cell in a second cell instead",
    ),
    (
        "distinct",
        "`distinct` is not supported; `group by` the columns you want distinct and select them",
    ),
    ("over", "window functions are not supported"),
    (
        "right",
        "a `right join` is not supported; it is a `left join` with the two tables swapped",
    ),
    ("full", "a `full join` is not supported"),
    ("cross", "a `cross join` is not supported"),
    (
        "natural",
        "a `natural join` is not supported; name the keys with `on`",
    ),
    (
        "offset",
        "`offset` is not supported; `limit` takes the first rows",
    ),
    (
        "cast",
        "`cast` is not supported; there is no conversion function in this dialect",
    ),
    (
        "like",
        "`like` is not supported; `contains(column, 'text')` is the substring test",
    ),
    (
        "exists",
        "`exists` is not supported; a `left semi join` is the same question",
    ),
    ("case", "internal"),
];

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    end: usize,
}

impl Parser<'_> {
    fn here(&self, message: impl Into<String>) -> SqlError {
        syntax(self.offset(), message)
    }

    fn offset(&self) -> usize {
        self.tokens.get(self.at).map(|t| t.at).unwrap_or(self.end)
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }

    /// Whether the next token is this keyword, matched without regard to case.
    fn at_word(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(keyword))
    }

    fn eat_word(&mut self, keyword: &str) -> bool {
        if self.at_word(keyword) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if self.peek()
            == Some(&Tok::Sym(
                SYMBOLS.iter().find(|s| **s == sym).copied().unwrap(),
            ))
        {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, keyword: &str) -> Result<(), SqlError> {
        if self.eat_word(keyword) {
            Ok(())
        } else {
            Err(self.here(format!("expected `{keyword}`")))
        }
    }

    fn expect_sym(&mut self, sym: &str) -> Result<(), SqlError> {
        if self.eat_sym(sym) {
            Ok(())
        } else {
            Err(self.here(format!("expected `{sym}`")))
        }
    }

    /// An identifier: a bare word that is not a keyword, or a quoted one.
    fn name(&mut self) -> Result<(String, usize), SqlError> {
        let at = self.offset();
        match self.peek().cloned() {
            Some(Tok::Quoted(n)) => {
                self.at += 1;
                Ok((n, at))
            }
            Some(Tok::Word(w)) => {
                self.refuse(&w, at)?;
                self.at += 1;
                Ok((w, at))
            }
            _ => Err(self.here("expected a name")),
        }
    }

    /// Stop on a keyword this dialect refuses, wherever it turns up.
    fn refuse(&self, word: &str, at: usize) -> Result<(), SqlError> {
        if let Some((_, message)) = REFUSED
            .iter()
            .find(|(k, m)| *m != "internal" && word.eq_ignore_ascii_case(k))
        {
            return Err(unsupported(at, *message));
        }
        Ok(())
    }

    // ── the statement ──────────────────────────────────────────────────────────────────

    fn statement(&mut self) -> Result<Select, SqlError> {
        if let Some(Tok::Word(w)) = self.peek().cloned() {
            self.refuse(&w, self.offset())?;
            if !w.eq_ignore_ascii_case("select") {
                return Err(unsupported(
                    self.offset(),
                    format!(
                        "a SQL cell is a single `select`; `{}` is not something it can do",
                        w.to_lowercase()
                    ),
                ));
            }
        }
        self.expect_word("select")?;

        let mut star = false;
        let mut star_at = 0;
        let mut items = Vec::new();
        loop {
            if self.peek() == Some(&Tok::Sym("*")) {
                star_at = self.offset();
                star = true;
                self.at += 1;
            } else {
                items.push(self.item()?);
            }
            if !self.eat_sym(",") {
                break;
            }
        }

        self.expect_word("from")?;
        let from = self.table()?;
        if self.peek() == Some(&Tok::Sym(",")) {
            return Err(unsupported(
                self.offset(),
                "a comma-separated `from` is not supported; write the join with `join … on …`",
            ));
        }
        let join = self.join()?;
        if self.at_word("join") || self.at_word("inner") || self.at_word("left") {
            return Err(unsupported(
                self.offset(),
                "a SQL cell joins at most two tables; put the second join in its own cell",
            ));
        }

        let filter = if self.eat_word("where") {
            Some(self.expression()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_word("group") {
            self.expect_word("by")?;
            loop {
                group_by.push(self.reference()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }

        let order_by = if self.eat_word("order") {
            self.expect_word("by")?;
            let column = self.reference()?;
            let descending = if self.eat_word("desc") {
                true
            } else {
                self.eat_word("asc");
                false
            };
            if self.eat_sym(",") {
                return Err(unsupported(
                    self.offset(),
                    "`order by` takes one column; `sort` is one column and a chain of sorts is \
                     not the same thing",
                ));
            }
            Some((column, descending))
        } else {
            None
        };

        let limit = if self.eat_word("limit") {
            let at = self.offset();
            match self.peek().cloned() {
                Some(Tok::Lit(Value::Int { v })) if v >= 0 => {
                    self.at += 1;
                    Some(v as usize)
                }
                _ => return Err(syntax(at, "`limit` takes a whole number of rows")),
            }
        } else {
            None
        };

        if let Some(Tok::Word(w)) = self.peek().cloned() {
            self.refuse(&w, self.offset())?;
        }
        if self.at < self.tokens.len() {
            return Err(self.here("expected the end of the statement"));
        }

        Ok(Select {
            items,
            star,
            star_at,
            from,
            join,
            filter,
            group_by,
            order_by,
            limit,
        })
    }

    fn item(&mut self) -> Result<Item, SqlError> {
        let at = self.offset();
        let value = self.expression()?;
        let alias = if self.eat_word("as") {
            Some(self.name()?.0)
        } else if matches!(self.peek(), Some(Tok::Word(_) | Tok::Quoted(_)))
            && !self.at_word("from")
        {
            // `select a b` is SQL's alias without `as`. Accepted, because refusing it would be
            // a surprise, but `from` is never an alias.
            Some(self.name()?.0)
        } else {
            None
        };
        Ok(Item { value, alias, at })
    }

    fn table(&mut self) -> Result<Table, SqlError> {
        let (cell, at) = self.name()?;
        if self.eat_sym(".") {
            return Err(unsupported(
                at,
                "a table here is the name of a cell, so it has no schema to qualify",
            ));
        }
        // `as t`, a bare `t`, or nothing — and a clause keyword is never an alias, or
        // `from sales where …` would read `where` as the table's name.
        let named = self.eat_word("as")
            || matches!(self.peek(), Some(Tok::Word(w)) if !is_clause_keyword(w))
            || matches!(self.peek(), Some(Tok::Quoted(_)));
        let alias = if named { self.name()?.0 } else { cell.clone() };
        Ok(Table { cell, alias, at })
    }

    fn join(&mut self) -> Result<Option<JoinClause>, SqlError> {
        let at = self.offset();
        let how = if self.eat_word("inner") {
            self.expect_word("join")?;
            How::Inner
        } else if self.eat_word("left") {
            // `left semi` and `left anti` are Spark's spelling, and they are the only way to
            // write the two verbs that filter one table by another.
            let how = if self.eat_word("semi") {
                How::Semi
            } else if self.eat_word("anti") {
                How::Anti
            } else {
                self.eat_word("outer");
                How::Left
            };
            self.expect_word("join")?;
            how
        } else if self.eat_word("join") {
            How::Inner
        } else {
            return Ok(None);
        };

        let table = self.table()?;
        self.expect_word("on")?;
        let mut on = Vec::new();
        loop {
            let left = self.reference()?;
            if !self.eat_sym("=") {
                return Err(unsupported(
                    self.offset(),
                    "a join's `on` matches equal columns; nothing else is supported",
                ));
            }
            let right = self.reference()?;
            on.push((left, right));
            if !self.eat_word("and") {
                break;
            }
        }
        Ok(Some(JoinClause { how, table, on, at }))
    }

    /// A column reference, for the places that take a column and not an expression.
    fn reference(&mut self) -> Result<Ref, SqlError> {
        let (first, at) = self.name()?;
        if self.eat_sym(".") {
            if self.eat_sym("*") {
                return Err(unsupported(
                    at,
                    "`table.*` is not supported; name the columns",
                ));
            }
            let (name, _) = self.name()?;
            return Ok(Ref {
                qualifier: Some(first),
                name,
                at,
            });
        }
        Ok(Ref {
            qualifier: None,
            name: first,
            at,
        })
    }
}

/// Words that end a table name rather than aliasing it.
fn is_clause_keyword(word: &str) -> bool {
    [
        "where", "group", "order", "limit", "join", "inner", "left", "on", "having", "union",
        "offset", "cross", "right", "full", "natural",
    ]
    .iter()
    .any(|k| word.eq_ignore_ascii_case(k))
}

/// Parse a statement.
///
/// # Errors
///
/// [`SqlError`], carrying an offset into `sql`.
pub(super) fn parse(sql: &str) -> Result<Select, SqlError> {
    let tokens = lex(sql)?;
    if tokens.is_empty() {
        return Err(syntax(0, "a SQL cell needs a `select`"));
    }
    let mut p = Parser {
        tokens: &tokens,
        at: 0,
        end: sql.len(),
    };
    p.statement()
}

// ── expressions ────────────────────────────────────────────────────────────────────────

/// The aggregate names, and the arity that makes each one an aggregate rather than a
/// function. `min` and `max` are both: `min(x)` is the aggregate over a column and
/// `min(a, b)` is the two-argument function, and the arity decides — which is what SQL does
/// and the only reading under which both remain writeable.
fn aggregate(name: &str, args: usize) -> Option<Agg> {
    let agg = match name {
        "sum" => Agg::Sum,
        "avg" => Agg::Mean,
        "count" => Agg::Count,
        "min" => Agg::Min,
        "max" => Agg::Max,
        _ => return None,
    };
    match (agg, args) {
        (Agg::Min | Agg::Max, 1) | (Agg::Sum | Agg::Mean | Agg::Count, _) => Some(agg),
        _ => None,
    }
}

impl Parser<'_> {
    fn expression(&mut self) -> Result<Sql, SqlError> {
        let mut left = self.conjunction()?;
        while self.eat_word("or") {
            left = Sql::Binary(BinOp::Or, Box::new(left), Box::new(self.conjunction()?));
        }
        Ok(left)
    }

    fn conjunction(&mut self) -> Result<Sql, SqlError> {
        let mut left = self.negation()?;
        while self.eat_word("and") {
            left = Sql::Binary(BinOp::And, Box::new(left), Box::new(self.negation()?));
        }
        Ok(left)
    }

    fn negation(&mut self) -> Result<Sql, SqlError> {
        if self.eat_word("not") {
            return Ok(Sql::Unary(UnOp::Not, Box::new(self.negation()?)));
        }
        self.predicate()
    }

    /// `x IS [NOT] NULL`, `x [NOT] BETWEEN a AND b` and `x [NOT] IN (…)` are spellings for
    /// things the expression language already says, so they are desugared here rather than
    /// carried into the tree. `between` evaluates its subject twice, which is free: nothing
    /// in either language has an effect.
    fn predicate(&mut self) -> Result<Sql, SqlError> {
        let left = self.comparison()?;
        let negated = self.eat_word("not");

        if self.eat_word("is") {
            if negated {
                return Err(self.here("write `is not null`, not `not is null`"));
            }
            let inner = self.eat_word("not");
            self.expect_word("null")?;
            let test = Sql::Call(Func::IsNull, vec![left]);
            return Ok(if inner {
                Sql::Unary(UnOp::Not, Box::new(test))
            } else {
                test
            });
        }

        if self.eat_word("between") {
            let low = self.comparison()?;
            self.expect_word("and")?;
            let high = self.comparison()?;
            let range = Sql::Binary(
                BinOp::And,
                Box::new(Sql::Binary(
                    BinOp::Ge,
                    Box::new(left.clone()),
                    Box::new(low),
                )),
                Box::new(Sql::Binary(BinOp::Le, Box::new(left), Box::new(high))),
            );
            return Ok(maybe_not(range, negated));
        }

        if self.eat_word("in") {
            self.expect_sym("(")?;
            let mut any: Option<Sql> = None;
            loop {
                let option = self.expression()?;
                let test = Sql::Binary(BinOp::Eq, Box::new(left.clone()), Box::new(option));
                any = Some(match any {
                    None => test,
                    Some(acc) => Sql::Binary(BinOp::Or, Box::new(acc), Box::new(test)),
                });
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return Ok(maybe_not(any.expect("at least one option"), negated));
        }

        if negated {
            return Err(self.here("expected `in`, `between` or `null` after `not`"));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Sql, SqlError> {
        let left = self.concatenation()?;
        let op = match self.peek() {
            Some(Tok::Sym("=")) => BinOp::Eq,
            Some(Tok::Sym("<>")) | Some(Tok::Sym("!=")) => BinOp::Ne,
            Some(Tok::Sym("<")) => BinOp::Lt,
            Some(Tok::Sym("<=")) => BinOp::Le,
            Some(Tok::Sym(">")) => BinOp::Gt,
            Some(Tok::Sym(">=")) => BinOp::Ge,
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.concatenation()?;
        if matches!(
            self.peek(),
            Some(Tok::Sym("=" | "<>" | "!=" | "<" | "<=" | ">" | ">="))
        ) {
            return Err(self.here("comparisons do not chain; write `a < b and b < c`"));
        }
        Ok(Sql::Binary(op, Box::new(left), Box::new(right)))
    }

    fn concatenation(&mut self) -> Result<Sql, SqlError> {
        let mut left = self.sum()?;
        while self.eat_sym("||") {
            left = Sql::Call(Func::Concat, vec![left, self.sum()?]);
        }
        Ok(left)
    }

    fn sum(&mut self) -> Result<Sql, SqlError> {
        let mut left = self.product()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym("+")) => BinOp::Add,
                Some(Tok::Sym("-")) => BinOp::Sub,
                _ => return Ok(left),
            };
            self.at += 1;
            left = Sql::Binary(op, Box::new(left), Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Sql, SqlError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym("*")) => BinOp::Mul,
                Some(Tok::Sym("/")) => BinOp::Div,
                Some(Tok::Sym("%")) => BinOp::Rem,
                _ => return Ok(left),
            };
            self.at += 1;
            left = Sql::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Sql, SqlError> {
        if self.eat_sym("-") {
            return Ok(Sql::Unary(UnOp::Neg, Box::new(self.unary()?)));
        }
        self.eat_sym("+");
        self.primary()
    }

    fn primary(&mut self) -> Result<Sql, SqlError> {
        let at = self.offset();
        match self.peek().cloned() {
            None => Err(self.here("the statement ends here, and a value was expected")),
            Some(Tok::Lit(v)) => {
                self.at += 1;
                Ok(Sql::Lit(v))
            }
            Some(Tok::Param(name)) => {
                self.at += 1;
                Ok(Sql::Param(name))
            }
            Some(Tok::Sym("(")) => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect_sym(")")?;
                Ok(inner)
            }
            Some(Tok::Sym("*")) => Err(unsupported(
                at,
                "`*` here is a multiplication with nothing on its left; a bare `*` belongs \
                 only in the select list",
            )),
            Some(Tok::Sym(s)) => Err(self.here(format!("`{s}` needs a value on its left"))),
            Some(Tok::Quoted(_)) => Ok(Sql::Ref(self.reference()?)),
            Some(Tok::Word(w)) => {
                self.refuse(&w, at)?;
                let lower = w.to_lowercase();
                match lower.as_str() {
                    "true" => {
                        self.at += 1;
                        return Ok(Sql::Lit(Value::bool(true)));
                    }
                    "false" => {
                        self.at += 1;
                        return Ok(Sql::Lit(Value::bool(false)));
                    }
                    "null" => {
                        self.at += 1;
                        return Ok(Sql::Lit(Value::Null));
                    }
                    "case" => {
                        self.at += 1;
                        return self.case(at);
                    }
                    _ => {}
                }
                // A call only when a `(` follows; otherwise it is a column, and a column may
                // legitimately be called `sum`.
                if self.tokens.get(self.at + 1).map(|t| &t.tok) != Some(&Tok::Sym("(")) {
                    return Ok(Sql::Ref(self.reference()?));
                }
                self.at += 2;
                self.call(&lower, at)
            }
        }
    }

    fn call(&mut self, name: &str, at: usize) -> Result<Sql, SqlError> {
        if name == "count" && self.eat_sym("*") {
            self.expect_sym(")")?;
            return Ok(Sql::Agg {
                func: Agg::Count,
                arg: None,
                at,
            });
        }
        let mut args = Vec::new();
        if !self.eat_sym(")") {
            loop {
                args.push(self.expression()?);
                if self.eat_sym(",") {
                    continue;
                }
                self.expect_sym(")")?;
                break;
            }
        }
        if name == "count" {
            return Err(unsupported(
                at,
                "`count(<expression>)` counts non-null values; this dialect's `count` counts \
                 rows, so it is written `count(*)`",
            ));
        }
        if let Some(func) = aggregate(name, args.len()) {
            return Ok(Sql::Agg {
                func,
                arg: Some(Box::new(args.pop().expect("one argument"))),
                at,
            });
        }
        let func = Func::lookup(name).ok_or_else(|| {
            let hint = dagpane_core::expr::nearest(name, Func::names())
                .map(|n| format!(" — did you mean `{n}`?"))
                .unwrap_or_default();
            unsupported(at, format!("there is no function `{name}`{hint}"))
        })?;
        Ok(Sql::Call(func, args))
    }

    /// `case when c then v … [else e] end`, and the simple `case x when v then r … end`.
    /// Both are `if` once the arms are nested, which is why neither reaches the tree.
    fn case(&mut self, at: usize) -> Result<Sql, SqlError> {
        let subject = if self.at_word("when") {
            None
        } else {
            Some(self.expression()?)
        };
        let mut arms: Vec<(Sql, Sql)> = Vec::new();
        while self.eat_word("when") {
            let test = self.expression()?;
            self.expect_word("then")?;
            let value = self.expression()?;
            let test = match &subject {
                None => test,
                Some(s) => Sql::Binary(BinOp::Eq, Box::new(s.clone()), Box::new(test)),
            };
            arms.push((test, value));
        }
        if arms.is_empty() {
            return Err(syntax(at, "a `case` needs at least one `when`"));
        }
        let mut result = if self.eat_word("else") {
            self.expression()?
        } else {
            Sql::Lit(Value::Null)
        };
        self.expect_word("end")?;
        for (test, value) in arms.into_iter().rev() {
            result = Sql::Call(Func::If, vec![test, value, result]);
        }
        Ok(result)
    }
}

fn maybe_not(value: Sql, negated: bool) -> Sql {
    if negated {
        Sql::Unary(UnOp::Not, Box::new(value))
    } else {
        value
    }
}
