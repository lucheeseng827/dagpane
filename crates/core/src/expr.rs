//! The expression language — the eighth verb, and the only place a manifest *computes* a
//! value rather than choosing one.
//!
//! Seven verbs can reshape a table but cannot produce a number the data does not already
//! contain. `margin = revenue - cost` was the wall, and every workaround for it — a
//! pre-computed column in the CSV, a view in the warehouse, a Rust closure — moved work out
//! of the app and into something the app's author could not edit.
//!
//! # The one rule that shapes the whole module
//!
//! **A bare name is a column. An edge is spelled `$`.**
//!
//! ```text
//! revenue - cost              two columns, no edges, this cell does not depend on anything new
//! amount * (1 - $discount)    one column and one edge, and the edge is visible in the text
//! ```
//!
//! That is not a stylistic choice. ADR-0001 says the manifest's job is to produce **edges**,
//! and an edge inferred wrongly is a wrong app — a cell that recomputes when it should not is
//! a cost, but a cell that does not recompute when it should is a stale number on a page that
//! looks correct. A language where `rate` might be a column or might be a cell, decided by
//! looking somewhere else in the file, is a language where the compiler has to *infer* which,
//! and where a reader cannot tell by reading. The sigil removes the question: the set of
//! edges an expression declares is exactly the set of `$` tokens in it, computable by the
//! lexer, and a `$name` that resolves to nothing is a compile error rather than an edge that
//! quietly does not exist.
//!
//! # Types are checked before a row is read
//!
//! [`Expr::parse`] checks the syntax. [`Expr::bind`] checks everything else against a
//! [`Scope`] — every column exists, every parameter exists, every operator is applied to
//! types it has a meaning for — and returns a [`Program`] whose output [`ColumnType`] is
//! already known. Binding is per evaluation and costs one walk of the syntax tree;
//! evaluation is per row and never looks up a name, because binding replaced every name with
//! an index.
//!
//! The same [`Expr::bind`] runs twice in the product: once by `dagpane check`, against the
//! schema a source has at compile time, and once by the pipeline, against the frame that
//! actually arrived. The first is best-effort — a cell whose input schema cannot be known
//! before the app runs is simply not checked — and the second is total. Neither is a
//! different implementation of the other, which is the only way the two can't drift.
//!
//! # Nulls, and where this differs from SQL
//!
//! [`crate::transform`] follows SQL's null rules. This module is **stricter, deliberately**:
//! any null operand makes the whole operation null, `and` and `or` included. SQL says
//! `false AND null` is `false`; here it is null.
//!
//! The rule SQL is protecting is short-circuiting, and the case where it differs is exactly
//! the case where the reader would rather see that something was missing. One sentence —
//! *a null anywhere in an expression makes its result null* — is also a rule an app author
//! can hold in their head, which a three-valued truth table is not. [`Func::Coalesce`] is
//! how you opt out, and it is the only function here that sees a null and keeps going.
//!
//! An arithmetic result that does not exist is null too, not an error and not an infinity:
//! division and remainder by zero, an `i64` that overflows, a comparison against a NaN.
//! A page that prints `inf` is a bug report; a page that prints `—` is a missing value, which
//! is what it is.
//!
//! # What is not here
//!
//! No user-defined functions, no aggregates (an expression sees one row — `group_by` is the
//! verb that sees many), no `select`-style column globs, no regular expressions, no dates.
//! The function set is closed and is listed in [`Func`]. Growing it is a code change and a
//! release note, which is the point: every name in an expression resolves to a column, a
//! declared parameter, or one of thirteen functions, and nothing else can be true.

use std::fmt;
use std::str::FromStr;

use crate::value::{ColumnType, Value};

// ── types ──────────────────────────────────────────────────────────────────────────────

/// The type of a value inside an expression.
///
/// [`ColumnType`] plus `Null`, which a column cannot be but a literal and an unmatched `if`
/// branch can. `Null` unifies with everything and disappears the moment the other side of an
/// operator has a type, so `amount + null` is still an `int` expression that happens to
/// evaluate to null.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer.
    Int,
    /// 64-bit float.
    Float,
    /// UTF-8 text.
    Text,
    /// Boolean.
    Bool,
    /// The type of the `null` literal: compatible with every other type, and a type no
    /// column can have. An expression whose type is `Null` is rejected by [`Expr::bind`],
    /// because "a column of nothing" has no representation.
    Null,
}

impl Ty {
    /// The expression type of a column's elements.
    pub fn of_column(ty: ColumnType) -> Ty {
        match ty {
            ColumnType::Int => Ty::Int,
            ColumnType::Float => Ty::Float,
            ColumnType::Text => Ty::Text,
            ColumnType::Bool => Ty::Bool,
        }
    }

    /// The column type that can hold this, or `None` for [`Ty::Null`].
    pub fn column_type(self) -> Option<ColumnType> {
        match self {
            Ty::Int => Some(ColumnType::Int),
            Ty::Float => Some(ColumnType::Float),
            Ty::Text => Some(ColumnType::Text),
            Ty::Bool => Some(ColumnType::Bool),
            Ty::Null => None,
        }
    }

    /// The type of a value, or `None` for a list or a table — the two things an expression
    /// has no operator for. Callers turn that `None` into an error that names the parameter,
    /// which is more useful than anything this function could say on its own.
    pub fn of_value(value: &Value) -> Option<Ty> {
        match value {
            Value::Null => Some(Ty::Null),
            Value::Bool { .. } => Some(Ty::Bool),
            Value::Int { .. } => Some(Ty::Int),
            Value::Float { .. } => Some(Ty::Float),
            Value::Text { .. } => Some(Ty::Text),
            Value::List { .. } | Value::Frame { .. } => None,
        }
    }

    /// Whether this is a number, or a null that could stand in for one.
    fn numeric(self) -> bool {
        matches!(self, Ty::Int | Ty::Float | Ty::Null)
    }

    /// The type both of these fit in, or `None` if there isn't one.
    ///
    /// `Null` meets anything; an `int` and a `float` meet at `float`, which is the same
    /// widening [`Value::as_float`] does and the same one [`crate::transform::Agg::Sum`]
    /// applies to an integer column.
    fn unify(self, other: Ty) -> Option<Ty> {
        match (self, other) {
            (Ty::Null, t) | (t, Ty::Null) => Some(t),
            (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => Some(Ty::Float),
            (a, b) if a == b => Some(a),
            _ => None,
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Text => "text",
            Ty::Bool => "bool",
            Ty::Null => "null",
        })
    }
}

// ── errors ─────────────────────────────────────────────────────────────────────────────

/// Everything that can be wrong with an expression.
///
/// Every variant names the thing that is wrong. That is the whole design brief for this
/// enum: the author of a bad expression is looking at a manifest, not at a stack trace, and
/// "no column `revnue`" followed by the columns that do exist is the difference between a
/// typo found in five seconds and one found by reading a blank pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprError {
    /// The text is not an expression. Carries a byte offset into it and what was expected.
    Syntax {
        /// Byte offset into the expression text, for a caller that wants to point at it.
        at: usize,
        /// What went wrong there, in a sentence.
        message: String,
    },
    /// A bare name that is not a column of the table at this point in the pipeline.
    UnknownColumn {
        /// The name as written.
        name: String,
        /// The columns that do exist, in the table's own order.
        available: Vec<String>,
        /// The closest existing name, when one is close enough to be worth suggesting.
        nearest: Option<String>,
    },
    /// A `$name` that names nothing declared above this cell.
    UnknownParam {
        /// The name as written, without the `$`.
        name: String,
        /// The parameters this expression was given.
        available: Vec<String>,
        /// The closest declared name, when there is one.
        nearest: Option<String>,
    },
    /// An operator or function applied to types it has no meaning for, a function that does
    /// not exist, or one called with the wrong number of arguments.
    Type(String),
    /// The expression's type is `null` — `expr = "null"`, or a `coalesce` of nothing but
    /// nulls. There is no column type for it, so it is refused rather than guessed at.
    AlwaysNull,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Syntax { at, message } => write!(f, "at character {at}: {message}"),
            ExprError::UnknownColumn {
                name,
                available,
                nearest,
            } => {
                write!(f, "no column `{name}`")?;
                if let Some(n) = nearest {
                    write!(f, " — did you mean `{n}`?")?;
                }
                write!(f, "; the table here has {}", list(available))
            }
            ExprError::UnknownParam {
                name,
                available,
                nearest,
            } => {
                write!(f, "`${name}` is not a parameter of this expression")?;
                if let Some(n) = nearest {
                    write!(f, " — did you mean `${n}`?")?;
                }
                write!(f, "; it has {}", list(available))
            }
            ExprError::Type(message) => f.write_str(message),
            ExprError::AlwaysNull => {
                f.write_str("this expression is always null, so there is no column type for it")
            }
        }
    }
}

impl std::error::Error for ExprError {}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The closest of `candidates` to `name`, if one is close enough to suggest.
///
/// Public because the manifest checker spells the same "did you mean" for the verbs that
/// name a column directly — `select`, `sort`, `group_by`, `scalar` — and two implementations
/// of one message is how the two drift apart.
///
/// Damerau-Levenshtein distance, accepted at 2 or below and never more than a third of the
/// name's length, so a three-letter typo does not suggest an unrelated three-letter column.
///
/// Damerau rather than plain Levenshtein because a **transposition is the commonest typo
/// there is** and plain Levenshtein charges 2 for one — which puts `bnad` for `band` over a
/// four-letter name's budget and loses exactly the suggestion a person most wants.
///
/// Case is ignored, because `Revenue` for `revenue` is the second most common version of
/// this mistake and the most annoying one to spot by eye.
pub fn nearest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let lower = name.to_lowercase();
    let limit = (lower.chars().count() / 3).clamp(1, 2);
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let d = distance(&lower, &candidate.to_lowercase());
        if d <= limit && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, c)| c.to_string())
}

/// Damerau-Levenshtein distance over chars, in the optimal-string-alignment form: three rows
/// of the matrix rather than all of it, and no substring is transposed twice.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row: Vec<usize> = vec![0; b.len() + 1];
    for i in 0..a.len() {
        row[0] = i + 1;
        for j in 0..b.len() {
            let cost = usize::from(a[i] != b[j]);
            let mut best =
                std::cmp::min(std::cmp::min(row[j] + 1, prev[j + 1] + 1), prev[j] + cost);
            if i > 0 && j > 0 && a[i] == b[j - 1] && a[i - 1] == b[j] {
                best = std::cmp::min(best, before[j - 1] + 1);
            }
            row[j + 1] = best;
        }
        std::mem::swap(&mut before, &mut prev);
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

// ── the vocabulary ─────────────────────────────────────────────────────────────────────

/// The unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// `-x`. Numeric. An `i64` negation that would overflow yields null.
    Neg,
    /// `not x`. Boolean.
    Not,
}

/// The binary operators, in the order they bind — loosest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// `or`. Both sides boolean; a null on either side makes the result null.
    Or,
    /// `and`. Same rule.
    And,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `+`. Numeric only — there is no text `+`, because an operator that concatenates when
    /// it cannot add is the single most reliable source of wrong answers in any expression
    /// language that has one. [`Func::Concat`] says what it means.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`. **Always yields a float**, `int / int` included, and division by zero is null
    /// rather than an error or an infinity. An integer division that silently truncated
    /// would be a wrong number rendered confidently, which is the one thing a dashboard must
    /// not do — the same reasoning that makes [`crate::transform::Agg::Sum`] a float.
    Div,
    /// `%`. `int % int` stays an int; anything with a float in it is a float remainder.
    /// A zero right-hand side is null.
    Rem,
}

impl BinOp {
    fn symbol(self) -> &'static str {
        match self {
            BinOp::Or => "or",
            BinOp::And => "and",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
        }
    }

    fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }
}

/// The closed function set. Fifteen, and adding one is a code change and a release note.
///
/// Every numeric function that can fail — an overflowing `abs`, a comparison against a NaN —
/// yields null rather than an error, for the reason the module docs give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    /// `abs(x)`. Numeric; keeps its argument's type.
    Abs,
    /// `round(x)`. Always float, like its two neighbours: rounding is a float operation, and
    /// one that returned an int would smuggle an `i64` range question into a language with
    /// no error value.
    Round,
    /// `floor(x)`. Always float.
    Floor,
    /// `ceil(x)`. Always float.
    Ceil,
    /// `min(a, b)`. Numeric; `int` only when both are.
    Min,
    /// `max(a, b)`. Same.
    Max,
    /// `lower(s)`. Locale-independent Unicode case conversion — which is precisely why this
    /// exists where [`crate::transform::Comparison::Contains`] has no case-insensitive
    /// variant: a function with a defined answer is not the same problem as a comparison
    /// whose meaning would change per locale.
    Lower,
    /// `upper(s)`. Same.
    Upper,
    /// `trim(s)`. Leading and trailing Unicode whitespace.
    Trim,
    /// `len(s)`. **Characters, not bytes** — the number a person counts.
    Len,
    /// `concat(a, b, …)`. Two or more text arguments. A null argument makes the whole thing
    /// null; `coalesce` is how you write a default for it.
    Concat,
    /// `contains(haystack, needle)`. Whether the second appears in the first. Case-sensitive,
    /// like [`crate::transform::Comparison::Contains`], and for the same reason: a
    /// case-insensitive answer needs a locale this crate does not have.
    Contains,
    /// `coalesce(a, b, …)`. The first argument that is not null.
    Coalesce,
    /// `is_null(x)`. Whether the value is missing — and, with `coalesce`, one of the two
    /// functions here that sees a null and keeps going. Without it a null is something an
    /// expression can propagate but never *ask about*, which makes "how many of these are
    /// missing?" an unwriteable question.
    IsNull,
    /// `if(cond, then, else)`. `cond` is boolean, and a null `cond` makes the result null
    /// rather than choosing the `else` branch — "we do not know" is not "no".
    If,
}

impl Func {
    fn name(self) -> &'static str {
        match self {
            Func::Abs => "abs",
            Func::Round => "round",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
            Func::Min => "min",
            Func::Max => "max",
            Func::Lower => "lower",
            Func::Upper => "upper",
            Func::Trim => "trim",
            Func::Len => "len",
            Func::Concat => "concat",
            Func::Contains => "contains",
            Func::Coalesce => "coalesce",
            Func::IsNull => "is_null",
            Func::If => "if",
        }
    }

    /// Every function's name, for a "did you mean" on a misspelt call.
    const ALL: [Func; 15] = [
        Func::Abs,
        Func::Round,
        Func::Floor,
        Func::Ceil,
        Func::Min,
        Func::Max,
        Func::Lower,
        Func::Upper,
        Func::Trim,
        Func::Len,
        Func::Concat,
        Func::Contains,
        Func::Coalesce,
        Func::IsNull,
        Func::If,
    ];

    /// The function with this name, or `None`.
    ///
    /// Public because the SQL dialect in `dagpane-app` resolves its calls against the same
    /// closed set — a second list of function names is a second thing to keep in step, and
    /// the one that fell behind would silently accept a call this language cannot evaluate.
    pub fn lookup(name: &str) -> Option<Func> {
        Func::ALL.iter().copied().find(|f| f.name() == name)
    }

    /// Every function's name, for a "did you mean" on a misspelt call.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Func::ALL.iter().map(|f| f.name())
    }

    /// This function's name, as it is written in an expression.
    pub fn spelling(self) -> &'static str {
        self.name()
    }
}

// ── the syntax tree ────────────────────────────────────────────────────────────────────

/// A parsed expression: checked for syntax, and for nothing else.
///
/// Holds the text it was parsed from, so an error about it can quote it back.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    root: Term,
    text: String,
}

/// One node of a parsed expression, with names still as the author wrote them.
#[derive(Clone, Debug, PartialEq)]
enum Term {
    Lit(Value),
    Column(String),
    Param(String),
    Unary(UnOp, Box<Term>),
    Binary(BinOp, Box<Term>, Box<Term>),
    Call(Func, Vec<Term>),
}

impl Expr {
    /// Parse an expression.
    ///
    /// Syntax only: a name that does not exist and an operator applied to the wrong type are
    /// both legal here and are caught by [`Expr::bind`], which is the only place that knows
    /// what the names mean.
    ///
    /// # Errors
    ///
    /// [`ExprError::Syntax`], carrying an offset into `text`.
    pub fn parse(text: &str) -> Result<Expr, ExprError> {
        let tokens = lex(text)?;
        let mut p = Parser {
            tokens: &tokens,
            at: 0,
            end: text.len(),
        };
        let root = p.expression()?;
        if p.at < p.tokens.len() {
            return Err(p.here("expected the end of the expression"));
        }
        Ok(Expr {
            root,
            text: text.to_string(),
        })
    }

    /// The text this was parsed from.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Every column this expression reads, in first-use order and deduplicated.
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.root, &mut |t| {
            if let Term::Column(n) = t {
                if !out.iter().any(|e| e == n) {
                    out.push(n.clone());
                }
            }
        });
        out
    }

    /// Every `$parameter` this expression reads, in first-use order and deduplicated.
    ///
    /// **This is the edge set.** A compiler builds one graph edge per name in here and none
    /// from anywhere else, which is what makes the edges of a derived column exactly the
    /// ones somebody typed.
    pub fn params(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.root, &mut |t| {
            if let Term::Param(n) = t {
                if !out.iter().any(|e| e == n) {
                    out.push(n.clone());
                }
            }
        });
        out
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

// ── building one without parsing one ───────────────────────────────────────────────────

/// Construct an expression from parts, for a front end that parsed something else.
///
/// The SQL cells in `dagpane-app` are the caller: SQL's `=`, `<>`, `IS NULL`, `BETWEEN`, `IN`
/// and `CASE` all mean something this language can say and none of them *spell* it the same
/// way, so the translation is a tree walk rather than a string rewrite. Going through text —
/// rendering SQL back into dagpane-expression source and re-parsing it — would put quoting and
/// precedence between the two languages, and a bug there would be a wrong expression that
/// still parsed.
///
/// Every constructor also renders the text, fully parenthesised, so [`Expr::text`] still
/// returns something a person can read: it is what an error about a lowered expression quotes
/// back, and "the expression I actually built" is the only honest thing to show there.
impl Expr {
    /// A constant.
    pub fn literal(value: Value) -> Expr {
        let text = render_literal(&value);
        Expr {
            root: Term::Lit(value),
            text,
        }
    }

    /// A column of the frame this will run over.
    pub fn column(name: impl Into<String>) -> Expr {
        let name = name.into();
        let text = render_name(&name);
        Expr {
            root: Term::Column(name),
            text,
        }
    }

    /// A `$parameter` — a reference to another cell, and an edge wherever it is compiled.
    pub fn param(name: impl Into<String>) -> Expr {
        let name = name.into();
        Expr {
            root: Term::Param(name.clone()),
            text: format!("${name}"),
        }
    }

    /// `-a` or `not a`.
    pub fn unary(op: UnOp, a: Expr) -> Expr {
        let text = match op {
            UnOp::Neg => format!("(-{})", a.text),
            UnOp::Not => format!("(not {})", a.text),
        };
        Expr {
            root: Term::Unary(op, Box::new(a.root)),
            text,
        }
    }

    /// `a op b`.
    pub fn binary(op: BinOp, a: Expr, b: Expr) -> Expr {
        let text = format!("({} {} {})", a.text, op.symbol(), b.text);
        Expr {
            root: Term::Binary(op, Box::new(a.root), Box::new(b.root)),
            text,
        }
    }

    /// `f(a, b, …)`. The argument count is not checked here — [`Expr::bind`] checks it, along
    /// with the types, and one place that knows the rules is better than two.
    pub fn call(func: Func, args: Vec<Expr>) -> Expr {
        let text = format!(
            "{}({})",
            func.name(),
            args.iter()
                .map(|a| a.text.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Expr {
            root: Term::Call(func, args.into_iter().map(|a| a.root).collect()),
            text,
        }
    }
}

/// A literal as this language's source spells it, so a built expression reads back.
fn render_literal(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool { v } => v.to_string(),
        Value::Int { v } => v.to_string(),
        // `1` would read back as an int and change the expression's type. A float literal has
        // to keep its point.
        Value::Float { v } if v.fract() == 0.0 && v.is_finite() => format!("{v:.1}"),
        Value::Float { v } => v.to_string(),
        Value::Text { v } => format!("'{}'", v.replace('\'', "''")),
        other => format!("<{}>", other.type_name()),
    }
}

/// A column name as this language's source spells it: bare when the identifier grammar accepts
/// it, in backticks when it does not.
fn render_name(name: &str) -> String {
    let plain = ident(name, 0).is_some_and(|(taken, end)| end == name.len() && taken == name);
    if plain && !matches!(name, "true" | "false" | "null" | "and" | "or" | "not") {
        name.to_string()
    } else {
        format!("`{name}`")
    }
}

impl FromStr for Expr {
    type Err = ExprError;
    fn from_str(s: &str) -> Result<Expr, ExprError> {
        Expr::parse(s)
    }
}

fn walk(term: &Term, f: &mut impl FnMut(&Term)) {
    f(term);
    match term {
        Term::Lit(_) | Term::Column(_) | Term::Param(_) => {}
        Term::Unary(_, a) => walk(a, f),
        Term::Binary(_, a, b) => {
            walk(a, f);
            walk(b, f);
        }
        Term::Call(_, args) => args.iter().for_each(|a| walk(a, f)),
    }
}

// ── lexing ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(Value),
    Name(String),
    Param(String),
    Not,
    Op(BinOp),
    Open,
    Close,
    Comma,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    at: usize,
}

fn syntax(at: usize, message: impl Into<String>) -> ExprError {
    ExprError::Syntax {
        at,
        message: message.into(),
    }
}

/// Turn the text into tokens.
///
/// Three lexical decisions, each of which exists because the expression is written inside a
/// TOML string and has to survive that:
///
/// * **Strings use single quotes.** `expr = "region == 'north'"` needs no escaping in TOML;
///   a double-quoted literal would need a backslash at every appearance. `''` inside one is a
///   literal quote, SQL's rule.
/// * **Backticks quote a name.** Real CSV headers contain spaces and dots. `` `order date` ``
///   is how you name one; a bare name is the identifier grammar and nothing else.
/// * **`$` is one token with the name it prefixes**, not an operator applied to it, so the
///   edge set is something the lexer can hand over without the parser's help.
fn lex(text: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let at = i;
        match c {
            b'(' => {
                out.push(Token { tok: Tok::Open, at });
                i += 1;
            }
            b')' => {
                out.push(Token {
                    tok: Tok::Close,
                    at,
                });
                i += 1;
            }
            b',' => {
                out.push(Token {
                    tok: Tok::Comma,
                    at,
                });
                i += 1;
            }
            b'+' | b'-' | b'*' | b'/' | b'%' => {
                let op = match c {
                    b'+' => BinOp::Add,
                    b'-' => BinOp::Sub,
                    b'*' => BinOp::Mul,
                    b'/' => BinOp::Div,
                    _ => BinOp::Rem,
                };
                out.push(Token {
                    tok: Tok::Op(op),
                    at,
                });
                i += 1;
            }
            b'=' | b'!' | b'<' | b'>' => {
                let two = bytes.get(i + 1) == Some(&b'=');
                let op = match (c, two) {
                    (b'=', true) => BinOp::Eq,
                    (b'!', true) => BinOp::Ne,
                    (b'<', true) => BinOp::Le,
                    (b'>', true) => BinOp::Ge,
                    (b'<', false) => BinOp::Lt,
                    (b'>', false) => BinOp::Gt,
                    (b'=', false) => {
                        return Err(syntax(at, "`=` is not an operator here; equality is `==`"))
                    }
                    _ => return Err(syntax(at, "`!` is not an operator here; use `not` or `!=`")),
                };
                out.push(Token {
                    tok: Tok::Op(op),
                    at,
                });
                i += if two { 2 } else { 1 };
            }
            b'$' => {
                let (name, next) = ident(text, i + 1)
                    .ok_or_else(|| syntax(at, "`$` must be followed by a parameter name"))?;
                out.push(Token {
                    tok: Tok::Param(name),
                    at,
                });
                i = next;
            }
            b'\'' => {
                let (s, next) = string(text, i)?;
                out.push(Token {
                    tok: Tok::Lit(Value::text(s)),
                    at,
                });
                i = next;
            }
            b'`' => {
                let end = text[i + 1..]
                    .find('`')
                    .ok_or_else(|| syntax(at, "this ` is never closed"))?;
                let name = &text[i + 1..i + 1 + end];
                if name.is_empty() {
                    return Err(syntax(at, "`` is not a name"));
                }
                out.push(Token {
                    tok: Tok::Name(name.to_string()),
                    at,
                });
                i = i + end + 2;
            }
            b'0'..=b'9' => {
                let (value, next) = number(text, i)?;
                out.push(Token {
                    tok: Tok::Lit(value),
                    at,
                });
                i = next;
            }
            _ => {
                let (name, next) = ident(text, i).ok_or_else(|| {
                    syntax(
                        at,
                        format!(
                            "`{}` is not something an expression can contain",
                            &text[i..].chars().next().unwrap_or('?')
                        ),
                    )
                })?;
                let tok = match name.as_str() {
                    "true" => Tok::Lit(Value::bool(true)),
                    "false" => Tok::Lit(Value::bool(false)),
                    "null" => Tok::Lit(Value::Null),
                    "and" => Tok::Op(BinOp::And),
                    "or" => Tok::Op(BinOp::Or),
                    "not" => Tok::Not,
                    _ => Tok::Name(name),
                };
                out.push(Token { tok, at });
                i = next;
            }
        }
    }
    Ok(out)
}

/// An identifier starting at `from`, and where it ends. ASCII letters, digits and `_`, not
/// starting with a digit — a name outside that set is written in backticks.
fn ident(text: &str, from: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let first = *bytes.get(from)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = from + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    Some((text[from..end].to_string(), end))
}

/// A single-quoted string starting at `from`, with `''` for a literal quote.
fn string(text: &str, from: usize) -> Result<(String, usize), ExprError> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = from + 1;
    loop {
        let Some(&c) = bytes.get(i) else {
            return Err(syntax(from, "this ' is never closed"));
        };
        if c == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            return Ok((out, i + 1));
        }
        // Walk by character, not by byte: a quote cannot appear inside a multi-byte
        // sequence, so finding one is enough to delimit, but the text between two of them
        // has to come out as it went in.
        let ch = text[i..].chars().next().expect("indexed at a boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
}

/// A number starting at `from`. A `.` or an exponent makes it a float; otherwise it is an
/// `i64`, and one that does not fit is an error naming it rather than a silent `f64`.
fn number(text: &str, from: usize) -> Result<(Value, usize), ExprError> {
    let bytes = text.as_bytes();
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
    let raw = &text[from..end];
    if float {
        let v: f64 = raw
            .parse()
            .map_err(|_| syntax(from, format!("`{raw}` is not a number")))?;
        Ok((Value::float(v), end))
    } else {
        let v: i64 = raw.parse().map_err(|_| {
            syntax(
                from,
                format!("`{raw}` does not fit in a 64-bit integer; write it as `{raw}.0` if you meant a float"),
            )
        })?;
        Ok((Value::int(v), end))
    }
}

// ── parsing ────────────────────────────────────────────────────────────────────────────

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    end: usize,
}

impl Parser<'_> {
    fn here(&self, message: impl Into<String>) -> ExprError {
        let at = self.tokens.get(self.at).map(|t| t.at).unwrap_or(self.end);
        syntax(at, message)
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }

    fn eat(&mut self, want: &Tok) -> bool {
        if self.peek() == Some(want) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<Term, ExprError> {
        self.binary_chain(BinOp::Or, Parser::conjunction)
    }

    fn conjunction(&mut self) -> Result<Term, ExprError> {
        self.binary_chain(BinOp::And, Parser::negation)
    }

    fn binary_chain(
        &mut self,
        op: BinOp,
        mut next: impl FnMut(&mut Self) -> Result<Term, ExprError>,
    ) -> Result<Term, ExprError> {
        let mut left = next(self)?;
        while self.eat(&Tok::Op(op)) {
            let right = next(self)?;
            left = Term::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn negation(&mut self) -> Result<Term, ExprError> {
        if self.eat(&Tok::Not) {
            return Ok(Term::Unary(UnOp::Not, Box::new(self.negation()?)));
        }
        self.comparison()
    }

    /// Comparisons do not chain. `1 < x < 10` reads as a range and means something else in
    /// every language that allows it, so this one says so instead.
    fn comparison(&mut self) -> Result<Term, ExprError> {
        let left = self.sum()?;
        let op = match self.peek() {
            Some(Tok::Op(o)) if o.is_comparison() => *o,
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.sum()?;
        if matches!(self.peek(), Some(Tok::Op(o)) if o.is_comparison()) {
            return Err(self.here("comparisons do not chain; write `a < b and b < c`"));
        }
        Ok(Term::Binary(op, Box::new(left), Box::new(right)))
    }

    fn sum(&mut self) -> Result<Term, ExprError> {
        let mut left = self.product()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o @ (BinOp::Add | BinOp::Sub))) => *o,
                _ => return Ok(left),
            };
            self.at += 1;
            left = Term::Binary(op, Box::new(left), Box::new(self.product()?));
        }
    }

    fn product(&mut self) -> Result<Term, ExprError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o @ (BinOp::Mul | BinOp::Div | BinOp::Rem))) => *o,
                _ => return Ok(left),
            };
            self.at += 1;
            left = Term::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Term, ExprError> {
        if self.eat(&Tok::Op(BinOp::Sub)) {
            return Ok(Term::Unary(UnOp::Neg, Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Term, ExprError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(self.here("the expression ends here, and something was expected"));
        };
        match tok {
            Tok::Lit(v) => {
                self.at += 1;
                Ok(Term::Lit(v))
            }
            Tok::Param(name) => {
                self.at += 1;
                Ok(Term::Param(name))
            }
            Tok::Open => {
                self.at += 1;
                let inner = self.expression()?;
                if !self.eat(&Tok::Close) {
                    return Err(self.here("expected a `)`"));
                }
                Ok(inner)
            }
            Tok::Name(name) => {
                let name_at = self.tokens[self.at].at;
                self.at += 1;
                if !self.eat(&Tok::Open) {
                    return Ok(Term::Column(name));
                }
                let func = Func::lookup(&name).ok_or_else(|| {
                    let hint = nearest(&name, Func::names())
                        .map(|n| format!(" — did you mean `{n}`?"))
                        .unwrap_or_default();
                    syntax(name_at, format!("there is no function `{name}`{hint}"))
                })?;
                let mut args = Vec::new();
                if !self.eat(&Tok::Close) {
                    loop {
                        args.push(self.expression()?);
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        if self.eat(&Tok::Close) {
                            break;
                        }
                        return Err(self.here("expected a `,` or a `)`"));
                    }
                }
                Ok(Term::Call(func, args))
            }
            Tok::Not => Err(self.here("`not` needs something to negate")),
            Tok::Op(op) => Err(self.here(format!("`{}` needs a value on its left", op.symbol()))),
            Tok::Close => Err(self.here("this `)` closes nothing")),
            Tok::Comma => Err(self.here("a `,` only separates a function's arguments")),
        }
    }
}

// ── binding ────────────────────────────────────────────────────────────────────────────

/// What names an expression is allowed to use, and what they are.
///
/// Columns are given **in the frame's own order**, because the index this records is the
/// index [`Row::column`] will be asked for. Parameters are given in the order the caller
/// will supply their values.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    columns: Vec<(String, ColumnType)>,
    params: Vec<(String, Ty)>,
}

impl Scope {
    /// An empty scope: no columns and no parameters.
    pub fn new() -> Scope {
        Scope::default()
    }

    /// The columns an expression may read, in the frame's order — `frame.schema()`, in
    /// practice, and passing anything else silently rebinds the indices.
    pub fn with_columns(mut self, columns: Vec<(String, ColumnType)>) -> Scope {
        self.columns = columns;
        self
    }

    /// Declare `$name` as a value of this type. Appended, so the order of these calls is the
    /// order [`Row::param`] will be indexed in.
    pub fn with_param(mut self, name: impl Into<String>, ty: Ty) -> Scope {
        self.params.push((name.into(), ty));
        self
    }
}

/// An expression bound to a [`Scope`]: every name resolved to an index, every operator
/// checked, and the output column type already decided.
///
/// The names are gone by this point, which is the whole reason the type exists. Evaluation
/// runs once per row and a name lookup per row is a hash of a short string per row; an index
/// is not.
#[derive(Clone, Debug)]
pub struct Program {
    code: Code,
    ty: ColumnType,
}

/// One node of a bound expression: the same shape as [`Term`], with indices for names.
#[derive(Clone, Debug)]
enum Code {
    Lit(Value),
    Column(usize),
    Param(usize),
    Unary(UnOp, Box<Code>),
    Binary(BinOp, Box<Code>, Box<Code>),
    Call(Func, Vec<Code>),
}

impl Expr {
    /// Resolve every name and check every type against `scope`.
    ///
    /// This is the function `dagpane check` runs before the app starts and the function the
    /// pipeline runs against the frame that actually arrived — the same code with a different
    /// scope, which is the only arrangement in which the two cannot disagree.
    ///
    /// # Errors
    ///
    /// [`ExprError::UnknownColumn`], [`ExprError::UnknownParam`], [`ExprError::Type`] or
    /// [`ExprError::AlwaysNull`].
    pub fn bind(&self, scope: &Scope) -> Result<Program, ExprError> {
        let (code, ty) = bind(&self.root, scope)?;
        Ok(Program {
            code,
            ty: ty.column_type().ok_or(ExprError::AlwaysNull)?,
        })
    }
}

impl Program {
    /// The type of the column this produces.
    pub fn output_type(&self) -> ColumnType {
        self.ty
    }

    /// Evaluate against one row.
    ///
    /// Total: there is no error return, because every way an expression can fail to produce
    /// a number — a null operand, a zero divisor, an `i64` that overflows, a NaN in a
    /// comparison — produces null instead. A row that cannot be computed is a gap in a
    /// column, not a broken pane.
    pub fn eval(&self, row: &dyn Row) -> Value {
        eval(&self.code, row)
    }
}

/// One row, as the numbers an expression needs from it.
///
/// Indices, not names: they are the positions [`Scope::with_columns`] and
/// [`Scope::with_param`] recorded.
pub trait Row {
    /// The value of column `at` in this row. Out of range is a caller bug — binding produced
    /// the index from the same scope this row is built for.
    fn column(&self, at: usize) -> Value;
    /// The value of parameter `at`. Constant across the rows of one evaluation.
    fn param(&self, at: usize) -> Value;
}

fn type_error(message: impl Into<String>) -> ExprError {
    ExprError::Type(message.into())
}

fn side(n: usize) -> &'static str {
    if n == 0 {
        "left"
    } else {
        "right"
    }
}

fn bind(term: &Term, scope: &Scope) -> Result<(Code, Ty), ExprError> {
    match term {
        Term::Lit(v) => Ok((
            Code::Lit(v.clone()),
            Ty::of_value(v).expect("a literal is never a list or a table"),
        )),
        Term::Column(name) => {
            let at = scope
                .columns
                .iter()
                .position(|(c, _)| c == name)
                .ok_or_else(|| ExprError::UnknownColumn {
                    name: name.clone(),
                    available: scope.columns.iter().map(|(c, _)| c.clone()).collect(),
                    nearest: nearest(name, scope.columns.iter().map(|(c, _)| c.as_str())),
                })?;
            Ok((Code::Column(at), Ty::of_column(scope.columns[at].1)))
        }
        Term::Param(name) => {
            let at = scope
                .params
                .iter()
                .position(|(p, _)| p == name)
                .ok_or_else(|| ExprError::UnknownParam {
                    name: name.clone(),
                    available: scope.params.iter().map(|(p, _)| p.clone()).collect(),
                    nearest: nearest(name, scope.params.iter().map(|(p, _)| p.as_str())),
                })?;
            Ok((Code::Param(at), scope.params[at].1))
        }
        Term::Unary(op, a) => {
            let (code, ty) = bind(a, scope)?;
            let out = match op {
                // Arithmetic keeps its operand's type, so a null stays a null all the way
                // up and is caught once, at the top, by `AlwaysNull`.
                UnOp::Neg => {
                    if !ty.numeric() {
                        return Err(type_error(format!("`-` needs a number, not {ty}")));
                    }
                    ty
                }
                // Logic and comparison always produce a bool, null operand or not — which is
                // what keeps `region == null` a usable bool column rather than a type error.
                UnOp::Not => {
                    if !matches!(ty, Ty::Bool | Ty::Null) {
                        return Err(type_error(format!("`not` needs a bool, not {ty}")));
                    }
                    Ty::Bool
                }
            };
            Ok((Code::Unary(*op, Box::new(code)), out))
        }
        Term::Binary(op, a, b) => {
            let (ca, ta) = bind(a, scope)?;
            let (cb, tb) = bind(b, scope)?;
            let out = binary_type(*op, ta, tb)?;
            Ok((Code::Binary(*op, Box::new(ca), Box::new(cb)), out))
        }
        Term::Call(func, args) => {
            let mut code = Vec::with_capacity(args.len());
            let mut types = Vec::with_capacity(args.len());
            for a in args {
                let (c, t) = bind(a, scope)?;
                code.push(c);
                types.push(t);
            }
            let out = call_type(*func, &types)?;
            Ok((Code::Call(*func, code), out))
        }
    }
}

fn binary_type(op: BinOp, a: Ty, b: Ty) -> Result<Ty, ExprError> {
    match op {
        BinOp::And | BinOp::Or => {
            for (n, t) in [a, b].into_iter().enumerate() {
                if !matches!(t, Ty::Bool | Ty::Null) {
                    return Err(type_error(format!(
                        "`{}` needs bools; its {} side is {t}",
                        op.symbol(),
                        side(n)
                    )));
                }
            }
            Ok(Ty::Bool)
        }
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            a.unify(b).ok_or_else(|| {
                type_error(format!("`{}` cannot compare {a} with {b}", op.symbol()))
            })?;
            Ok(Ty::Bool)
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Rem | BinOp::Div => {
            for (n, t) in [a, b].into_iter().enumerate() {
                if !t.numeric() {
                    let hint = if op == BinOp::Add && t == Ty::Text {
                        "; `concat(a, b)` joins text"
                    } else {
                        ""
                    };
                    return Err(type_error(format!(
                        "`{}` needs numbers; its {} side is {t}{hint}",
                        op.symbol(),
                        side(n)
                    )));
                }
            }
            // `/` is the one arithmetic operator whose output type does not follow its
            // inputs. See `BinOp::Div`.
            if op == BinOp::Div {
                Ok(Ty::Float)
            } else {
                Ok(a.unify(b).expect("two numerics always unify"))
            }
        }
    }
}

fn arity(func: Func, want: &str, ok: bool, got: usize) -> Result<(), ExprError> {
    if ok {
        Ok(())
    } else {
        Err(type_error(format!(
            "`{}` takes {want}; it was given {got}",
            func.name()
        )))
    }
}

fn call_type(func: Func, args: &[Ty]) -> Result<Ty, ExprError> {
    let n = args.len();
    let numeric = |t: Ty| -> Result<(), ExprError> {
        if t.numeric() {
            Ok(())
        } else {
            Err(type_error(format!(
                "`{}` needs numbers, not {t}",
                func.name()
            )))
        }
    };
    let text = |t: Ty| -> Result<(), ExprError> {
        if matches!(t, Ty::Text | Ty::Null) {
            Ok(())
        } else {
            Err(type_error(format!("`{}` needs text, not {t}", func.name())))
        }
    };
    match func {
        Func::Abs => {
            arity(func, "one argument", n == 1, n)?;
            numeric(args[0])?;
            Ok(args[0])
        }
        Func::Round | Func::Floor | Func::Ceil => {
            arity(func, "one argument", n == 1, n)?;
            numeric(args[0])?;
            Ok(Ty::Float)
        }
        Func::Min | Func::Max => {
            arity(func, "two arguments", n == 2, n)?;
            numeric(args[0])?;
            numeric(args[1])?;
            Ok(args[0].unify(args[1]).expect("two numerics always unify"))
        }
        Func::Lower | Func::Upper | Func::Trim => {
            arity(func, "one argument", n == 1, n)?;
            text(args[0])?;
            Ok(Ty::Text)
        }
        Func::Len => {
            arity(func, "one argument", n == 1, n)?;
            text(args[0])?;
            Ok(Ty::Int)
        }
        Func::Concat => {
            arity(func, "two or more arguments", n >= 2, n)?;
            args.iter().try_for_each(|t| text(*t))?;
            Ok(Ty::Text)
        }
        Func::Contains => {
            arity(func, "two arguments", n == 2, n)?;
            text(args[0])?;
            text(args[1])?;
            Ok(Ty::Bool)
        }
        Func::IsNull => {
            arity(func, "one argument", n == 1, n)?;
            // The one function with no type requirement at all: every type can be missing.
            Ok(Ty::Bool)
        }
        Func::Coalesce => {
            arity(func, "two or more arguments", n >= 2, n)?;
            args.iter().copied().try_fold(Ty::Null, |acc, t| {
                acc.unify(t).ok_or_else(|| {
                    type_error(format!(
                        "`coalesce` needs its arguments to share one type; it was given {acc} and {t}"
                    ))
                })
            })
        }
        Func::If => {
            arity(func, "three arguments", n == 3, n)?;
            if !matches!(args[0], Ty::Bool | Ty::Null) {
                return Err(type_error(format!(
                    "`if` needs a bool to test, not {}",
                    args[0]
                )));
            }
            args[1].unify(args[2]).ok_or_else(|| {
                type_error(format!(
                    "`if` needs its two results to share one type; they are {} and {}",
                    args[1], args[2]
                ))
            })
        }
    }
}

// ── evaluating ─────────────────────────────────────────────────────────────────────────

fn eval(code: &Code, row: &dyn Row) -> Value {
    match code {
        Code::Lit(v) => v.clone(),
        Code::Column(at) => row.column(*at),
        Code::Param(at) => row.param(*at),
        Code::Unary(UnOp::Neg, a) => match eval(a, row) {
            // `-i64::MIN` is not an `i64`. Null, not a panic and not a wrap: the module docs
            // say why an arithmetic result that does not exist is a missing value.
            Value::Int { v } => v.checked_neg().map(Value::int).unwrap_or(Value::Null),
            Value::Float { v } => Value::float(-v),
            _ => Value::Null,
        },
        Code::Unary(UnOp::Not, a) => match eval(a, row) {
            Value::Bool { v } => Value::bool(!v),
            _ => Value::Null,
        },
        Code::Binary(op, a, b) => binary(*op, eval(a, row), eval(b, row)),
        // The two functions that do not evaluate all of their arguments. Only their cost is
        // observable — nothing in this language has an effect — but on a million rows that
        // cost is the difference between one branch and both.
        Code::Call(Func::Coalesce, args) => args
            .iter()
            .map(|a| eval(a, row))
            .find(|v| !matches!(v, Value::Null))
            .unwrap_or(Value::Null),
        Code::Call(Func::If, args) => match eval(&args[0], row) {
            Value::Bool { v: true } => eval(&args[1], row),
            Value::Bool { v: false } => eval(&args[2], row),
            // A null condition is "we do not know", which is not "no".
            _ => Value::Null,
        },
        Code::Call(func, args) => call(
            *func,
            &args.iter().map(|a| eval(a, row)).collect::<Vec<_>>(),
        ),
    }
}

/// How two values order, or `None` when they do not compare at all — a null on either side,
/// a NaN, or two types the checker would not have let through.
fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int { v: x }, Value::Int { v: y }) => Some(x.cmp(y)),
        (Value::Text { v: x }, Value::Text { v: y }) => Some(x.as_str().cmp(y.as_str())),
        (Value::Bool { v: x }, Value::Bool { v: y }) => Some(x.cmp(y)),
        _ => a.as_float()?.partial_cmp(&b.as_float()?),
    }
}

fn holds(op: BinOp, ord: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match (op, ord) {
        (BinOp::Eq, Equal) => true,
        (BinOp::Ne, Equal) => false,
        (BinOp::Ne, _) => true,
        (BinOp::Lt, Less) => true,
        (BinOp::Le, Less | Equal) => true,
        (BinOp::Gt, Greater) => true,
        (BinOp::Ge, Greater | Equal) => true,
        _ => false,
    }
}

fn binary(op: BinOp, a: Value, b: Value) -> Value {
    match op {
        BinOp::And => match (&a, &b) {
            (Value::Bool { v: x }, Value::Bool { v: y }) => Value::bool(*x && *y),
            _ => Value::Null,
        },
        BinOp::Or => match (&a, &b) {
            (Value::Bool { v: x }, Value::Bool { v: y }) => Value::bool(*x || *y),
            _ => Value::Null,
        },
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(&a, &b)
            .map(|o| Value::bool(holds(op, o)))
            .unwrap_or(Value::Null),
        // Zero divides into nothing, and neither an infinity nor a NaN is a number a reader
        // of a dashboard can do anything with.
        BinOp::Div => divide(a.as_float(), b.as_float(), |x, y| x / y),
        BinOp::Rem => match (&a, &b) {
            (Value::Int { v: x }, Value::Int { v: y }) => {
                x.checked_rem(*y).map(Value::int).unwrap_or(Value::Null)
            }
            _ => divide(a.as_float(), b.as_float(), |x, y| x % y),
        },
        BinOp::Add | BinOp::Sub | BinOp::Mul => match (&a, &b) {
            (Value::Int { v: x }, Value::Int { v: y }) => match op {
                BinOp::Add => x.checked_add(*y),
                BinOp::Sub => x.checked_sub(*y),
                _ => x.checked_mul(*y),
            }
            .map(Value::int)
            .unwrap_or(Value::Null),
            _ => match (a.as_float(), b.as_float()) {
                (Some(x), Some(y)) => Value::float(match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    _ => x * y,
                }),
                _ => Value::Null,
            },
        },
    }
}

/// A float operation with a divisor, which is null when that divisor is zero.
fn divide(x: Option<f64>, y: Option<f64>, f: fn(f64, f64) -> f64) -> Value {
    match (x, y) {
        (Some(x), Some(y)) if y != 0.0 => Value::float(f(x, y)),
        _ => Value::Null,
    }
}

fn call(func: Func, args: &[Value]) -> Value {
    let float1 = |f: fn(f64) -> f64| {
        args[0]
            .as_float()
            .map(|x| Value::float(f(x)))
            .unwrap_or(Value::Null)
    };
    let text1 = |f: fn(&str) -> String| {
        args[0]
            .as_text()
            .map(|s| Value::text(f(s)))
            .unwrap_or(Value::Null)
    };
    match func {
        Func::Abs => match &args[0] {
            Value::Int { v } => v.checked_abs().map(Value::int).unwrap_or(Value::Null),
            Value::Float { v } => Value::float(v.abs()),
            _ => Value::Null,
        },
        Func::Round => float1(f64::round),
        Func::Floor => float1(f64::floor),
        Func::Ceil => float1(f64::ceil),
        Func::Min | Func::Max => {
            let want = if func == Func::Min {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
            // Through `compare`, not `f64::min`: that one ignores a lone NaN and would
            // return the other operand, which is a number the data does not contain.
            match compare(&args[0], &args[1]) {
                Some(o) => {
                    let pick = if o == want || o == std::cmp::Ordering::Equal {
                        0
                    } else {
                        1
                    };
                    args[pick].clone()
                }
                None => Value::Null,
            }
        }
        Func::Lower => text1(str::to_lowercase),
        Func::Upper => text1(str::to_uppercase),
        Func::Trim => text1(|s| s.trim().to_string()),
        Func::Len => args[0]
            .as_text()
            .map(|s| Value::int(s.chars().count() as i64))
            .unwrap_or(Value::Null),
        Func::IsNull => Value::bool(matches!(args[0], Value::Null)),
        Func::Contains => match (args[0].as_text(), args[1].as_text()) {
            (Some(hay), Some(needle)) => Value::bool(hay.contains(needle)),
            _ => Value::Null,
        },
        Func::Concat => {
            let mut out = String::new();
            for a in args {
                match a.as_text() {
                    Some(s) => out.push_str(s),
                    None => return Value::Null,
                }
            }
            Value::text(out)
        }
        Func::Coalesce | Func::If => {
            unreachable!("coalesce and if are evaluated lazily, above")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of literal values, indexed the way a bound program indexes one.
    struct Fixed {
        columns: Vec<Value>,
        params: Vec<Value>,
    }

    impl Row for Fixed {
        fn column(&self, at: usize) -> Value {
            self.columns[at].clone()
        }
        fn param(&self, at: usize) -> Value {
            self.params[at].clone()
        }
    }

    /// The schema every test below binds against, unless it builds its own.
    fn scope() -> Scope {
        Scope::new()
            .with_columns(vec![
                ("amount".to_string(), ColumnType::Float),
                ("qty".to_string(), ColumnType::Int),
                ("region".to_string(), ColumnType::Text),
                ("paid".to_string(), ColumnType::Bool),
            ])
            .with_param("rate", Ty::Float)
            .with_param("floor", Ty::Int)
    }

    fn row() -> Fixed {
        Fixed {
            columns: vec![
                Value::float(100.0),
                Value::int(3),
                Value::text("north"),
                Value::bool(true),
            ],
            params: vec![Value::float(0.1), Value::int(50)],
        }
    }

    /// Bind against the standard scope and evaluate against the standard row.
    fn run(text: &str) -> Value {
        Expr::parse(text)
            .expect("parses")
            .bind(&scope())
            .expect("binds")
            .eval(&row())
    }

    fn fails(text: &str) -> ExprError {
        Expr::parse(text)
            .and_then(|e| e.bind(&scope()).map(|_| ()))
            .expect_err("should not have been accepted")
    }

    fn ty(text: &str) -> ColumnType {
        Expr::parse(text)
            .expect("parses")
            .bind(&scope())
            .expect("binds")
            .output_type()
    }

    // ── the one rule ───────────────────────────────────────────────────────────────────

    #[test]
    fn a_bare_name_is_a_column_and_a_dollar_is_an_edge() {
        let e = Expr::parse("amount * (1 - $rate) + qty").unwrap();
        assert_eq!(e.columns(), vec!["amount".to_string(), "qty".to_string()]);
        assert_eq!(e.params(), vec!["rate".to_string()]);
    }

    #[test]
    fn the_edge_set_is_deduplicated_and_in_first_use_order() {
        let e = Expr::parse("$floor + $rate * $floor").unwrap();
        assert_eq!(e.params(), vec!["floor".to_string(), "rate".to_string()]);
    }

    #[test]
    fn nothing_but_a_dollar_produces_an_edge() {
        // The whole argument for the sigil: a compiler reading `params()` can see every edge
        // this expression declares without knowing what any other name means.
        let e = Expr::parse("coalesce(lower(region), 'none') == 'north'").unwrap();
        assert!(e.params().is_empty());
    }

    // ── names ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn an_unknown_column_names_the_columns_that_exist_and_suggests_one() {
        let e = fails("amont > 10");
        let ExprError::UnknownColumn {
            name,
            available,
            nearest,
        } = &e
        else {
            panic!("wrong error: {e:?}");
        };
        assert_eq!(name, "amont");
        assert_eq!(nearest.as_deref(), Some("amount"));
        assert_eq!(available.len(), 4);
        let message = e.to_string();
        assert!(message.contains("did you mean `amount`?"), "{message}");
        assert!(message.contains("`region`"), "{message}");
    }

    #[test]
    fn a_wrong_case_column_is_suggested_too() {
        // The most common version of this mistake, and the hardest to see by eye.
        let ExprError::UnknownColumn { nearest, .. } = fails("Amount > 10") else {
            panic!("wrong error");
        };
        assert_eq!(nearest.as_deref(), Some("amount"));
    }

    #[test]
    fn a_transposition_is_suggested_even_in_a_short_name() {
        // Plain Levenshtein charges 2 for a swapped pair, which is over a four-letter name's
        // budget — and a swapped pair is the commonest typo there is.
        let scope = Scope::new().with_columns(vec![("band".to_string(), ColumnType::Text)]);
        let ExprError::UnknownColumn { nearest, .. } =
            Expr::parse("bnad").unwrap().bind(&scope).unwrap_err()
        else {
            panic!("wrong error");
        };
        assert_eq!(nearest.as_deref(), Some("band"));
    }

    #[test]
    fn an_unrelated_name_suggests_nothing() {
        let ExprError::UnknownColumn { nearest, .. } = fails("turnover > 10") else {
            panic!("wrong error");
        };
        assert_eq!(nearest, None);
    }

    #[test]
    fn an_unknown_parameter_is_its_own_error() {
        let e = fails("amount * $discount");
        assert!(
            matches!(&e, ExprError::UnknownParam { name, .. } if name == "discount"),
            "{e:?}"
        );
        assert!(e.to_string().contains("`$discount`"), "{e}");
    }

    #[test]
    fn a_column_is_never_read_as_a_parameter_or_the_reverse() {
        // `amount` exists as a column and `rate` as a parameter. Swapping the sigil must not
        // quietly find the other one — that would be exactly the inference this design is
        // built to refuse.
        assert!(matches!(fails("$amount"), ExprError::UnknownParam { .. }));
        assert!(matches!(fails("rate"), ExprError::UnknownColumn { .. }));
    }

    #[test]
    fn a_backtick_name_holds_a_column_name_with_a_space_in_it() {
        let scope = Scope::new().with_columns(vec![("order date".to_string(), ColumnType::Text)]);
        let e = Expr::parse("len(`order date`)").unwrap();
        assert_eq!(e.columns(), vec!["order date".to_string()]);
        assert_eq!(e.bind(&scope).unwrap().output_type(), ColumnType::Int);
    }

    // ── syntax ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_single_equals_says_what_to_write_instead() {
        let ExprError::Syntax { message, .. } = Expr::parse("region = 'north'").unwrap_err() else {
            panic!("wrong error");
        };
        assert!(message.contains("`==`"), "{message}");
    }

    #[test]
    fn comparisons_do_not_chain() {
        let ExprError::Syntax { message, .. } = Expr::parse("1 < qty < 10").unwrap_err() else {
            panic!("wrong error");
        };
        assert!(message.contains("and"), "{message}");
    }

    #[test]
    fn a_misspelt_function_is_suggested() {
        let ExprError::Syntax { message, .. } = Expr::parse("lowar(region)").unwrap_err() else {
            panic!("wrong error");
        };
        assert!(message.contains("did you mean `lower`?"), "{message}");
    }

    #[test]
    fn an_unclosed_paren_points_at_the_end() {
        let ExprError::Syntax { at, .. } = Expr::parse("abs(amount").unwrap_err() else {
            panic!("wrong error");
        };
        assert_eq!(at, "abs(amount".len());
    }

    #[test]
    fn a_string_holds_a_quote_by_doubling_it() {
        let scope = Scope::new();
        let v = Expr::parse("'it''s'")
            .unwrap()
            .bind(&scope)
            .unwrap()
            .eval(&Fixed {
                columns: vec![],
                params: vec![],
            });
        assert_eq!(v.as_text(), Some("it's"));
    }

    #[test]
    fn an_integer_too_large_for_i64_says_so_rather_than_becoming_a_float() {
        let ExprError::Syntax { message, .. } = Expr::parse("99999999999999999999").unwrap_err()
        else {
            panic!("wrong error");
        };
        assert!(message.contains("64-bit"), "{message}");
    }

    #[test]
    fn precedence_is_the_usual_one() {
        assert_eq!(run("1 + 2 * 3").as_int(), Some(7));
        assert_eq!(run("(1 + 2) * 3").as_int(), Some(9));
        assert_eq!(run("not paid and paid").as_bool(), Some(false));
        assert_eq!(run("-qty + 5").as_int(), Some(2));
    }

    // ── types ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn arithmetic_keeps_int_and_widens_to_float() {
        assert_eq!(ty("qty + 1"), ColumnType::Int);
        assert_eq!(ty("qty + 1.5"), ColumnType::Float);
        assert_eq!(ty("amount - qty"), ColumnType::Float);
    }

    #[test]
    fn division_is_always_float() {
        // An int division that truncated would be a wrong number rendered confidently.
        assert_eq!(ty("qty / 2"), ColumnType::Float);
        assert_eq!(run("qty / 2").as_float(), Some(1.5));
    }

    #[test]
    fn remainder_keeps_its_ints() {
        assert_eq!(ty("qty % 2"), ColumnType::Int);
        assert_eq!(run("qty % 2").as_int(), Some(1));
    }

    #[test]
    fn plus_on_text_points_at_concat() {
        let e = fails("region + 'x'");
        assert!(e.to_string().contains("concat"), "{e}");
    }

    #[test]
    fn a_comparison_across_types_is_refused_rather_than_always_false() {
        // A filter's operand arrives at run time and an unmatched type there yields an empty
        // frame. An expression's types are known before a row is read, so this is a mistake
        // that can be named — and naming it is the point of the verb.
        let e = fails("region > qty");
        assert!(
            e.to_string().contains("cannot compare text with int"),
            "{e}"
        );
    }

    #[test]
    fn logic_needs_bools() {
        let e = fails("amount and paid");
        assert!(e.to_string().contains("left side is float"), "{e}");
    }

    #[test]
    fn if_needs_its_branches_to_agree() {
        let e = fails("if(paid, 1, 'no')");
        assert!(e.to_string().contains("int and text"), "{e}");
        assert_eq!(ty("if(paid, 1, 2.5)"), ColumnType::Float);
    }

    #[test]
    fn an_expression_that_is_always_null_has_no_column_type() {
        assert_eq!(fails("null"), ExprError::AlwaysNull);
        assert_eq!(fails("coalesce(null, null)"), ExprError::AlwaysNull);
        // But a null beside a type is that type, which is what keeps `coalesce` usable.
        assert_eq!(ty("coalesce(region, null)"), ColumnType::Text);
        assert_eq!(ty("amount + null"), ColumnType::Float);
    }

    #[test]
    fn a_comparison_against_null_is_still_a_bool_column() {
        assert_eq!(ty("region == null"), ColumnType::Bool);
    }

    #[test]
    fn arity_is_checked_by_name() {
        assert!(fails("abs(1, 2)").to_string().contains("one argument"));
        assert!(fails("concat('a')").to_string().contains("two or more"));
        assert!(fails("if(paid, 1)").to_string().contains("three"));
    }

    // ── nulls ──────────────────────────────────────────────────────────────────────────

    fn with_nulls(text: &str) -> Value {
        let scope = Scope::new()
            .with_columns(vec![
                ("n".to_string(), ColumnType::Int),
                ("t".to_string(), ColumnType::Text),
                ("b".to_string(), ColumnType::Bool),
            ])
            .with_param("p", Ty::Int);
        Expr::parse(text)
            .expect("parses")
            .bind(&scope)
            .expect("binds")
            .eval(&Fixed {
                columns: vec![Value::Null, Value::Null, Value::Null],
                params: vec![Value::Null],
            })
    }

    #[test]
    fn a_null_anywhere_makes_the_result_null() {
        for text in [
            "n + 1",
            "n * 2",
            "n / 2",
            "n % 2",
            "-n",
            "n > 1",
            "n == 1",
            "not b",
            "len(t)",
            "lower(t)",
            "abs(n)",
            "round(n)",
            "min(n, 1)",
            "concat(t, 'x')",
            "$p + 1",
        ] {
            assert!(matches!(with_nulls(text), Value::Null), "{text}");
        }
    }

    #[test]
    fn and_and_or_propagate_a_null_where_sql_would_not() {
        // SQL says `false and null` is false. This language says null, because one rule an
        // author can hold in their head beats a truth table they have to look up — and the
        // case where the two differ is the case where the reader wants to know.
        assert!(matches!(with_nulls("b and false"), Value::Null));
        assert!(matches!(with_nulls("b or true"), Value::Null));
    }

    #[test]
    fn coalesce_is_the_one_function_that_sees_a_null_and_keeps_going() {
        assert_eq!(with_nulls("coalesce(t, 'none')").as_text(), Some("none"));
        assert_eq!(with_nulls("coalesce(n, 0)").as_int(), Some(0));
    }

    #[test]
    fn a_null_condition_is_not_the_else_branch() {
        // "We do not know" is not "no".
        assert!(matches!(with_nulls("if(b, 1, 2)"), Value::Null));
    }

    // ── arithmetic that has no answer ──────────────────────────────────────────────────

    #[test]
    fn dividing_by_zero_is_null_not_an_infinity() {
        assert!(matches!(run("amount / 0"), Value::Null));
        assert!(matches!(run("qty / 0"), Value::Null));
        assert!(matches!(run("qty % 0"), Value::Null));
        assert!(matches!(run("amount % 0.0"), Value::Null));
    }

    #[test]
    fn an_integer_that_overflows_is_null_not_a_wrap() {
        let scope = Scope::new().with_columns(vec![("n".to_string(), ColumnType::Int)]);
        let at_the_edge = |text: &str, v: i64| {
            Expr::parse(text)
                .unwrap()
                .bind(&scope)
                .unwrap()
                .eval(&Fixed {
                    columns: vec![Value::int(v)],
                    params: vec![],
                })
        };
        assert!(matches!(at_the_edge("n + 1", i64::MAX), Value::Null));
        assert!(matches!(at_the_edge("n - 1", i64::MIN), Value::Null));
        assert!(matches!(at_the_edge("n * 2", i64::MAX), Value::Null));
        assert!(matches!(at_the_edge("-n", i64::MIN), Value::Null));
        assert!(matches!(at_the_edge("abs(n)", i64::MIN), Value::Null));
        // And the ordinary case still works, so the check is not swallowing everything.
        assert_eq!(at_the_edge("n + 1", 41).as_int(), Some(42));
    }

    #[test]
    fn every_comparison_against_a_nan_is_null() {
        let scope = Scope::new().with_columns(vec![("x".to_string(), ColumnType::Float)]);
        let nan = |text: &str| {
            Expr::parse(text)
                .unwrap()
                .bind(&scope)
                .unwrap()
                .eval(&Fixed {
                    columns: vec![Value::float(f64::NAN)],
                    params: vec![],
                })
        };
        for text in ["x > 1", "x == 1", "x != 1", "x <= 1"] {
            assert!(matches!(nan(text), Value::Null), "{text}");
        }
        // `f64::min` would have returned 1.0 here — a number the data does not contain.
        assert!(matches!(nan("min(x, 1.0)"), Value::Null));
    }

    // ── functions ──────────────────────────────────────────────────────────────────────

    #[test]
    fn len_counts_characters_not_bytes() {
        let scope = Scope::new().with_columns(vec![("t".to_string(), ColumnType::Text)]);
        let v = Expr::parse("len(t)")
            .unwrap()
            .bind(&scope)
            .unwrap()
            .eval(&Fixed {
                columns: vec![Value::text("café")],
                params: vec![],
            });
        assert_eq!(v.as_int(), Some(4));
    }

    #[test]
    fn the_numeric_functions_do_what_they_say() {
        assert_eq!(run("abs(0 - qty)").as_int(), Some(3));
        assert_eq!(run("round(amount / 3.0)").as_float(), Some(33.0));
        assert_eq!(run("floor(2.7)").as_float(), Some(2.0));
        assert_eq!(run("ceil(2.1)").as_float(), Some(3.0));
        assert_eq!(run("max(qty, 10)").as_int(), Some(10));
        assert_eq!(run("min(qty, 10)").as_int(), Some(3));
    }

    #[test]
    fn the_text_functions_do_what_they_say() {
        assert_eq!(run("upper(region)").as_text(), Some("NORTH"));
        assert_eq!(run("trim('  x  ')").as_text(), Some("x"));
        assert_eq!(
            run("concat(region, ' / ', 'east')").as_text(),
            Some("north / east")
        );
    }

    #[test]
    fn a_program_evaluates_the_same_expression_over_many_rows() {
        let program = Expr::parse("amount * (1 - $rate)")
            .unwrap()
            .bind(&scope())
            .unwrap();
        assert_eq!(program.output_type(), ColumnType::Float);
        for (amount, want) in [(100.0, 90.0), (200.0, 180.0), (0.0, 0.0)] {
            let row = Fixed {
                columns: vec![
                    Value::float(amount),
                    Value::int(1),
                    Value::text("n"),
                    Value::bool(true),
                ],
                params: vec![Value::float(0.1), Value::int(0)],
            };
            assert_eq!(program.eval(&row).as_float(), Some(want));
        }
    }

    #[test]
    fn is_null_is_how_you_ask_about_a_missing_value() {
        // Without it a null is something an expression can propagate but never ask about.
        assert_eq!(ty("is_null(region)"), ColumnType::Bool);
        assert_eq!(run("is_null(region)").as_bool(), Some(false));
        assert_eq!(with_nulls("is_null(t)").as_bool(), Some(true));
        assert_eq!(with_nulls("is_null(n)").as_bool(), Some(true));
        // It takes any type, which no other function does.
        assert_eq!(ty("is_null(paid)"), ColumnType::Bool);
        assert_eq!(ty("is_null(null)"), ColumnType::Bool);
    }

    #[test]
    fn contains_is_the_substring_test_and_propagates_a_null() {
        assert_eq!(run("contains(region, 'ort')").as_bool(), Some(true));
        assert_eq!(run("contains(region, 'south')").as_bool(), Some(false));
        assert!(matches!(with_nulls("contains(t, 'x')"), Value::Null));
        assert!(fails("contains(region, 1)")
            .to_string()
            .contains("needs text"));
    }

    #[test]
    fn an_expression_built_from_parts_binds_and_evaluates_like_a_parsed_one() {
        // What a front end that parsed something else produces. The two must be the same
        // expression, or a SQL cell and a `derive` would disagree about their own arithmetic.
        let built = Expr::binary(
            BinOp::Mul,
            Expr::column("amount"),
            Expr::binary(
                BinOp::Sub,
                Expr::literal(Value::int(1)),
                Expr::param("rate"),
            ),
        );
        let parsed = Expr::parse("amount * (1 - $rate)").unwrap();
        assert_eq!(built.columns(), parsed.columns());
        assert_eq!(built.params(), parsed.params());
        let (b, p) = (
            built.bind(&scope()).unwrap().eval(&row()),
            parsed.bind(&scope()).unwrap().eval(&row()),
        );
        assert_eq!(b.as_float(), p.as_float());
    }

    #[test]
    fn a_built_expression_renders_back_into_source_that_parses_to_itself() {
        // The text is what an error quotes back, so it has to be readable — and it has to be
        // the expression that was built, which is what re-parsing it checks.
        for built in [
            Expr::call(
                Func::If,
                vec![
                    Expr::call(Func::IsNull, vec![Expr::column("region")]),
                    Expr::literal(Value::text("it's none")),
                    Expr::column("region"),
                ],
            ),
            Expr::unary(UnOp::Not, Expr::column("paid")),
            Expr::binary(
                BinOp::Ge,
                Expr::column("amount"),
                Expr::literal(Value::float(2.0)),
            ),
        ] {
            let text = built.text().to_string();
            let reparsed =
                Expr::parse(&text).unwrap_or_else(|e| panic!("`{text}` does not parse back: {e}"));
            assert_eq!(reparsed.text(), text);
            assert_eq!(
                built.bind(&scope()).map(|p| p.output_type()).ok(),
                reparsed.bind(&scope()).map(|p| p.output_type()).ok(),
                "{text}"
            );
        }
    }

    #[test]
    fn a_built_name_that_is_not_an_identifier_comes_back_in_backticks() {
        let built = Expr::column("order date");
        assert_eq!(built.text(), "`order date`");
        assert_eq!(
            Expr::parse(built.text()).unwrap().columns(),
            vec!["order date"]
        );
        // And a float literal keeps its point, or it would read back as an int.
        assert_eq!(Expr::literal(Value::float(2.0)).text(), "2.0");
    }

    #[test]
    fn display_gives_back_what_was_written() {
        let text = "amount * (1 - $rate)";
        assert_eq!(Expr::parse(text).unwrap().to_string(), text);
    }
}
