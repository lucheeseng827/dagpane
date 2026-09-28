//! Resolution and lowering: a parsed [`Select`] into the [`Step`]s it means.
//!
//! Two jobs, and the order matters. **Resolve** first — every table alias to the cell behind
//! it, every qualified column to the alias it names, every select-list item to a grouped
//! column or an aggregate — and refuse what does not resolve. **Lower** second, into the same
//! `Step` a `[[cell.step]]` becomes.
//!
//! Nothing here evaluates anything, and nothing here decides a semantics. Every rule about
//! nulls, types, key duplication and column collisions belongs to the verb the statement
//! lowered to, which is what makes a SQL cell unable to behave differently from the pipeline
//! it compiles to.

use std::collections::BTreeMap;

use dagpane_core::expr::{BinOp, Expr, Func};
use dagpane_core::transform::{Agg, AggSpec, Comparison, GroupBy, How, Join};
use dagpane_core::{ColumnType, Value};

use super::parse::{parse, Item, JoinClause, Ref, Select, Sql, Table};
use super::SqlError;
use crate::manifest::{Rhs, Step};

fn meaning(at: usize, message: impl Into<String>) -> SqlError {
    SqlError::Meaning {
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

/// "What columns does this cell produce?", answered by whoever is compiling.
///
/// Borrowed rather than owned because the compiler's answer changes as it works down the
/// file: a cell declared later has none yet, and that is the whole reason `select *` over one
/// is refused.
pub(crate) type SchemaOf<'a> = &'a dyn Fn(&str) -> Option<Vec<(String, ColumnType)>>;

/// A lowered statement: the cell its pipeline reads, and the steps it runs.
#[derive(Clone, Debug)]
pub(crate) struct Lowered {
    /// What the cell's `from` is.
    pub from: String,
    /// The pipeline, in order.
    pub steps: Vec<Step>,
}

/// Parse and lower one statement.
///
/// `schema_of` answers "what columns does this cell produce?", and is consulted for exactly
/// one thing: expanding `select *`. Everything else lowers without knowing a schema, which is
/// what keeps a SQL cell compilable in the same places a hand-written pipeline is — and a `*`
/// over a cell whose columns are not knowable before the app runs is refused by name rather
/// than guessed at.
///
/// # Errors
///
/// [`SqlError`], carrying an offset into `sql`.
pub(crate) fn lower(sql: &str, schema_of: SchemaOf<'_>) -> Result<Lowered, SqlError> {
    let select = parse(sql)?;
    Lowering { schema_of, next: 0 }.run(select)
}

struct Lowering<'a> {
    schema_of: SchemaOf<'a>,
    next: usize,
}

impl Lowering<'_> {
    /// A name for a column the author did not write. Two underscores, so it cannot be
    /// confused with one they did — and every one of them is projected away before the
    /// pipeline ends.
    fn temp(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("__{kind}{}", self.next)
    }

    fn run(&mut self, select: Select) -> Result<Lowered, SqlError> {
        let aliases = self.aliases(&select.from, select.join.as_ref())?;
        let mut steps: Vec<Step> = Vec::new();

        // ── the join ───────────────────────────────────────────────────────────────────
        if let Some(join) = &select.join {
            steps.push(self.join(join, &select.from, &aliases)?);
        }

        // ── the where ──────────────────────────────────────────────────────────────────
        if let Some(predicate) = &select.filter {
            let mut conjuncts = Vec::new();
            flatten(predicate, &mut conjuncts);
            for conjunct in conjuncts {
                steps.extend(self.conjunct(conjunct, &aliases)?);
            }
        }

        // ── the select list, and the aggregates inside it ──────────────────────────────
        let grouped: Vec<String> = select
            .group_by
            .iter()
            .map(|r| self.column(r, &aliases))
            .collect::<Result<_, _>>()?;

        let mut items = self.expand(&select, &aliases)?;
        let mut aggs: Vec<AggSpec> = Vec::new();
        let mut before: Vec<Step> = Vec::new();
        for item in &mut items {
            item.value = self.extract(&item.value, &aliases, &mut aggs, &mut before)?;
        }

        if aggs.is_empty() && !grouped.is_empty() {
            // `group by` with nothing aggregated is a distinct, and the verb says so more
            // plainly than SQL does.
            aggs.push(AggSpec {
                column: String::new(),
                agg: Agg::Count,
                as_name: self.temp("count"),
            });
        }
        let grouping = !aggs.is_empty();

        // Every column an item reads has to survive the grouping, or the statement is asking
        // for a value that no longer exists by the time it is read. Checked here, before the
        // renaming below moves an aggregate's output column to the name the author gave it.
        if grouping {
            for item in &items {
                if let Some(bad) = reads_outside(&item.value, &grouped, &aggs) {
                    return Err(meaning(
                        item.at,
                        format!(
                            "`{bad}` is neither grouped nor aggregated, so there is no one \
                             value of it per row of the result"
                        ),
                    ));
                }
            }
        }

        // ── the output columns, and the steps that produce them ────────────────────────
        //
        // Before the grouping is pushed, because an item that is *exactly* one aggregate
        // names that aggregate's output column directly rather than deriving a copy of it —
        // `sum(amount) as revenue` is one `group_by` output called `revenue`, not one called
        // `__agg1` and a derive beside it.
        let mut outputs: Vec<String> = Vec::with_capacity(items.len());
        let mut after: Vec<Step> = Vec::new();
        for item in &items {
            if let Sql::Ref(r) = &item.value {
                if let Some(agg) = aggs.iter_mut().find(|a| a.as_name == r.name) {
                    let Some(alias) = &item.alias else {
                        return Err(meaning(
                            item.at,
                            "an aggregate is computed rather than read, so it needs a name: \
                             write `… as name`",
                        ));
                    };
                    agg.as_name = alias.clone();
                    outputs.push(alias.clone());
                    continue;
                }
            }
            match (&item.value, &item.alias) {
                // A column the author wrote keeps its own name, and needs no step at all.
                (Sql::Ref(r), None) => outputs.push(self.column(r, &aliases)?),
                (Sql::Ref(r), Some(alias)) if self.column(r, &aliases)? == *alias => {
                    outputs.push(alias.clone())
                }
                (value, Some(alias)) => {
                    after.push(Step::Derive {
                        name: alias.clone(),
                        expr: self.expr(value, &aliases)?,
                    });
                    outputs.push(alias.clone());
                }
                (_, None) => {
                    return Err(meaning(
                        item.at,
                        "this is computed rather than read, so it needs a name: write \
                         `… as name`",
                    ))
                }
            }
        }

        steps.extend(before);
        if grouping {
            steps.push(Step::Group(GroupBy {
                by: grouped.clone(),
                aggs,
            }));
        }
        steps.extend(after);

        // ── order by, then the projection, then limit ──────────────────────────────────
        if let Some((column, descending)) = &select.order_by {
            steps.push(Step::Sort {
                column: self.column(column, &aliases)?,
                descending: *descending,
            });
        }

        // The projection is skipped only when it would be the identity: no temporaries to
        // drop, no reordering to do, nothing renamed.
        let identity = !grouping
            && steps.iter().all(|s| !matches!(s, Step::Derive { .. }))
            && select.star
            && items.is_empty();
        if !identity {
            steps.push(Step::Select(outputs));
        }

        if let Some(n) = select.limit {
            steps.push(Step::Limit(n));
        }

        Ok(Lowered {
            from: select.from.cell.clone(),
            steps,
        })
    }

    /// The alias each table answers to, and the cell behind it.
    fn aliases(
        &self,
        from: &Table,
        join: Option<&JoinClause>,
    ) -> Result<BTreeMap<String, String>, SqlError> {
        let mut out = BTreeMap::new();
        out.insert(from.alias.clone(), from.cell.clone());
        if let Some(join) = join {
            if out.contains_key(&join.table.alias) {
                return Err(meaning(
                    join.table.at,
                    format!(
                        "both tables here answer to `{}`; give one of them an alias with `as`",
                        join.table.alias
                    ),
                ));
            }
            out.insert(join.table.alias.clone(), join.table.cell.clone());
        }
        Ok(out)
    }

    fn join(
        &mut self,
        join: &JoinClause,
        from: &Table,
        aliases: &BTreeMap<String, String>,
    ) -> Result<Step, SqlError> {
        let mut left_on = Vec::new();
        let mut right_on = Vec::new();
        for (a, b) in &join.on {
            let (Some(qa), Some(qb)) = (&a.qualifier, &b.qualifier) else {
                return Err(meaning(
                    a.at,
                    "qualify both sides of a join's `on` — `a.key = b.key` — so that which \
                     column belongs to which table is something the statement says rather \
                     than something the compiler works out",
                ));
            };
            for q in [qa, qb] {
                if !aliases.contains_key(q) {
                    return Err(meaning(a.at, unknown_alias(q, aliases)));
                }
            }
            if qa == &from.alias && qb == &join.table.alias {
                left_on.push(a.name.clone());
                right_on.push(b.name.clone());
            } else if qb == &from.alias && qa == &join.table.alias {
                left_on.push(b.name.clone());
                right_on.push(a.name.clone());
            } else {
                return Err(meaning(
                    a.at,
                    "a join's `on` matches one table against the other; both sides of this \
                     one name the same table",
                ));
            }
        }
        Ok(Step::Join {
            with: join.table.cell.clone(),
            spec: Join {
                left_on,
                right_on,
                how: join.how,
                suffix: String::new(),
                // Not negotiable from here: a SQL cell's join is a lookup. Fanning one out is
                // how a total silently doubles, and the dialect has no word for meaning it —
                // aggregate the right-hand side, or write the step by hand.
                multiple: false,
            },
        })
    }

    /// One conjunct of a `where`, as a step.
    ///
    /// A comparison between a column and a constant is a `filter` and nothing else, which is
    /// the shape almost every `where` actually has. Anything else becomes a derived boolean
    /// and a filter on it — one extra column, projected away by the select list, and the only
    /// price for a predicate the verb cannot express directly.
    fn conjunct(
        &mut self,
        sql: &Sql,
        aliases: &BTreeMap<String, String>,
    ) -> Result<Vec<Step>, SqlError> {
        if let Sql::Binary(op, left, right) = sql {
            if let Some(comparison) = comparison(*op) {
                let simple = match (left.as_ref(), right.as_ref()) {
                    (Sql::Ref(r), rhs) => self.rhs(rhs).map(|v| (r, comparison, v)),
                    // Flipped, so `100 <= amount` filters as `amount >= 100`.
                    (lhs, Sql::Ref(r)) => self.rhs(lhs).map(|v| (r, flip(comparison), v)),
                    _ => None,
                };
                if let Some((r, op, operand)) = simple {
                    return Ok(vec![Step::Filter {
                        column: self.column(r, aliases)?,
                        op,
                        operand,
                        skip_when: None,
                    }]);
                }
            }
        }
        // `contains(col, 'text')` is the one function call that is also a comparison the verb
        // knows, so it filters without a column of its own.
        if let Sql::Call(Func::Contains, args) = sql {
            if let (Some(Sql::Ref(r)), Some(Sql::Lit(needle @ Value::Text { .. }))) =
                (args.first(), args.get(1))
            {
                return Ok(vec![Step::Filter {
                    column: self.column(r, aliases)?,
                    op: Comparison::Contains,
                    operand: Rhs::Literal(needle.clone()),
                    skip_when: None,
                }]);
            }
        }
        let name = self.temp("where");
        Ok(vec![
            Step::Derive {
                name: name.clone(),
                expr: self.expr(sql, aliases)?,
            },
            Step::Filter {
                column: name,
                op: Comparison::Eq,
                operand: Rhs::Literal(Value::bool(true)),
                skip_when: None,
            },
        ])
    }

    fn rhs(&self, sql: &Sql) -> Option<Rhs> {
        match sql {
            Sql::Lit(v) => Some(Rhs::Literal(v.clone())),
            Sql::Param(name) => Some(Rhs::Param(name.clone())),
            _ => None,
        }
    }

    /// `select *` as the columns it stands for.
    fn expand(
        &mut self,
        select: &Select,
        aliases: &BTreeMap<String, String>,
    ) -> Result<Vec<Item>, SqlError> {
        if !select.star {
            return Ok(select.items.clone());
        }
        if !select.group_by.is_empty() {
            return Err(unsupported(
                select.star_at,
                "`select *` and `group by` do not go together: a group has one row for many, \
                 so name the columns you want per group",
            ));
        }
        let mut columns = self
            .schema(&select.from.cell)
            .ok_or_else(|| unsupported(select.star_at, star_needs_a_schema(&select.from.cell)))?;
        if let Some(join) = &select.join {
            if matches!(join.how, How::Inner | How::Left) {
                let right = self.schema(&join.table.cell).ok_or_else(|| {
                    unsupported(select.star_at, star_needs_a_schema(&join.table.cell))
                })?;
                let Step::Join { spec, .. } = self.join(join, &select.from, aliases)? else {
                    unreachable!("join lowers to a join")
                };
                // The same `join_schema` the verb uses, so `*` after a join is exactly the
                // columns the join will produce — and a collision is refused there, once.
                columns = dagpane_core::transform::join_schema(&columns, &right, &spec)
                    .map_err(|e| meaning(join.at, e))?;
            }
        }
        let mut items: Vec<Item> = columns
            .into_iter()
            .map(|(name, _)| Item {
                value: Sql::Ref(Ref {
                    qualifier: None,
                    name,
                    at: select.star_at,
                }),
                alias: None,
                at: select.star_at,
            })
            .collect();
        items.extend(select.items.iter().cloned());
        Ok(items)
    }

    fn schema(&self, cell: &str) -> Option<Vec<(String, ColumnType)>> {
        (self.schema_of)(cell)
    }

    /// Pull every aggregate out of an expression, leaving a reference to the column the
    /// group-by will produce. `sum(a)/sum(b) as ratio` becomes two aggregates and one derive
    /// over their results, which is what SQL means by it.
    fn extract(
        &mut self,
        sql: &Sql,
        aliases: &BTreeMap<String, String>,
        aggs: &mut Vec<AggSpec>,
        before: &mut Vec<Step>,
    ) -> Result<Sql, SqlError> {
        Ok(match sql {
            Sql::Agg { func, arg, at } => {
                let column = match arg {
                    None => String::new(),
                    Some(inner) => match inner.as_ref() {
                        Sql::Ref(r) => self.column(r, aliases)?,
                        // An aggregate over an expression: compute the column first, then
                        // aggregate it. The derive runs before the grouping, so it is a
                        // per-row step over the input, which is what SQL means too.
                        other => {
                            let name = self.temp("arg");
                            before.push(Step::Derive {
                                name: name.clone(),
                                expr: self.expr(other, aliases)?,
                            });
                            name
                        }
                    },
                };
                if matches!(func, Agg::Count) && !column.is_empty() {
                    return Err(unsupported(
                        *at,
                        "`count(<expression>)` counts non-null values; this dialect's `count` \
                         counts rows, so it is written `count(*)`",
                    ));
                }
                let as_name = self.temp("agg");
                aggs.push(AggSpec {
                    column,
                    agg: *func,
                    as_name: as_name.clone(),
                });
                Sql::Ref(Ref {
                    qualifier: None,
                    name: as_name,
                    at: *at,
                })
            }
            Sql::Unary(op, a) => Sql::Unary(*op, Box::new(self.extract(a, aliases, aggs, before)?)),
            Sql::Binary(op, a, b) => Sql::Binary(
                *op,
                Box::new(self.extract(a, aliases, aggs, before)?),
                Box::new(self.extract(b, aliases, aggs, before)?),
            ),
            Sql::Call(func, args) => Sql::Call(
                *func,
                args.iter()
                    .map(|a| self.extract(a, aliases, aggs, before))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }

    /// A resolved column name. After a join the columns are flat, so a qualifier's only job
    /// is to say which table the author meant — and one that names no table is an error.
    fn column(&self, r: &Ref, aliases: &BTreeMap<String, String>) -> Result<String, SqlError> {
        if let Some(q) = &r.qualifier {
            if !aliases.contains_key(q) {
                return Err(meaning(r.at, unknown_alias(q, aliases)));
            }
        }
        Ok(r.name.clone())
    }

    /// A resolved SQL expression as a dagpane expression.
    fn expr(&self, sql: &Sql, aliases: &BTreeMap<String, String>) -> Result<Expr, SqlError> {
        Ok(match sql {
            Sql::Lit(v) => Expr::literal(v.clone()),
            Sql::Ref(r) => Expr::column(self.column(r, aliases)?),
            Sql::Param(name) => Expr::param(name),
            Sql::Unary(op, a) => Expr::unary(*op, self.expr(a, aliases)?),
            Sql::Binary(op, a, b) => {
                Expr::binary(*op, self.expr(a, aliases)?, self.expr(b, aliases)?)
            }
            Sql::Call(func, args) => Expr::call(
                *func,
                args.iter()
                    .map(|a| self.expr(a, aliases))
                    .collect::<Result<_, _>>()?,
            ),
            Sql::Agg { at, .. } => {
                return Err(meaning(
                    *at,
                    "an aggregate belongs in the select list; it has no meaning here, where \
                     there is one row rather than a group",
                ))
            }
        })
    }
}

fn unknown_alias(name: &str, aliases: &BTreeMap<String, String>) -> String {
    let known = aliases
        .keys()
        .map(|a| format!("`{a}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("`{name}` is not a table in this statement; it has {known}")
}

fn star_needs_a_schema(cell: &str) -> String {
    format!(
        "`select *` needs to know what `{cell}` produces, and that is not knowable before \
         this app runs — name the columns instead"
    )
}

/// Split a conjunction into its parts, so `a and b and c` becomes three filters rather than
/// one derived boolean.
fn flatten<'a>(sql: &'a Sql, out: &mut Vec<&'a Sql>) {
    match sql {
        Sql::Binary(BinOp::And, a, b) => {
            flatten(a, out);
            flatten(b, out);
        }
        other => out.push(other),
    }
}

fn comparison(op: BinOp) -> Option<Comparison> {
    Some(match op {
        BinOp::Eq => Comparison::Eq,
        BinOp::Ne => Comparison::Ne,
        BinOp::Lt => Comparison::Lt,
        BinOp::Le => Comparison::Le,
        BinOp::Gt => Comparison::Gt,
        BinOp::Ge => Comparison::Ge,
        _ => return None,
    })
}

fn flip(op: Comparison) -> Comparison {
    match op {
        Comparison::Lt => Comparison::Gt,
        Comparison::Le => Comparison::Ge,
        Comparison::Gt => Comparison::Lt,
        Comparison::Ge => Comparison::Le,
        other => other,
    }
}

/// A column an expression reads that the grouping does not produce, if there is one.
fn reads_outside(sql: &Sql, grouped: &[String], aggs: &[AggSpec]) -> Option<String> {
    match sql {
        Sql::Ref(r) => {
            let produced = grouped.contains(&r.name) || aggs.iter().any(|a| a.as_name == r.name);
            (!produced).then(|| r.name.clone())
        }
        Sql::Unary(_, a) => reads_outside(a, grouped, aggs),
        Sql::Binary(_, a, b) => {
            reads_outside(a, grouped, aggs).or_else(|| reads_outside(b, grouped, aggs))
        }
        Sql::Call(_, args) => args.iter().find_map(|a| reads_outside(a, grouped, aggs)),
        Sql::Lit(_) | Sql::Param(_) | Sql::Agg { .. } => None,
    }
}
