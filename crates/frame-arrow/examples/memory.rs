//! What the two representations cost on the bundled example's column shape.
//!
//!   cargo run --release -p dagpane-frame-arrow --example memory [rows]
//!
//! Six columns exactly as `examples/sales.csv` has them: two categoricals with a handful of
//! distinct values, one high-cardinality id, one date-as-text, two numerics. The point is
//! that the win is not uniform — it comes almost entirely from the two categoricals, which
//! is why the encoding decision is per column rather than per frame.

use dagpane_core::value::{Column, ColumnData};
use dagpane_frame_arrow::{ArrowFrame, Encoding};

fn sales_columns(n: usize) -> Vec<Column> {
    let regions = ["north", "south", "east", "west"];
    let channels = ["partner", "direct", "web", "retail"];
    vec![
        Column::int("order_id", (0..n).map(|i| Some(i as i64)).collect()),
        Column::text(
            "day",
            (0..n)
                .map(|i| Some(format!("2026-08-{:02}", i % 28 + 1)))
                .collect(),
        ),
        Column::text(
            "region",
            (0..n).map(|i| Some(regions[i % 4].to_string())).collect(),
        ),
        Column::text(
            "channel",
            (0..n).map(|i| Some(channels[i % 4].to_string())).collect(),
        ),
        Column::int("units", (0..n).map(|i| Some((i % 50) as i64 + 1)).collect()),
        Column::float(
            "amount",
            (0..n).map(|i| Some(i as f64 % 994.0 + 5.0)).collect(),
        ),
    ]
}

/// What `Vec<Option<T>>` actually occupies: the enum slot per element, plus each string's
/// own heap buffer.
fn table_bytes(columns: &[Column]) -> usize {
    columns
        .iter()
        .map(|c| match &c.data {
            ColumnData::Int(v) => v.len() * std::mem::size_of::<Option<i64>>(),
            ColumnData::Float(v) => v.len() * std::mem::size_of::<Option<f64>>(),
            ColumnData::Bool(v) => v.len() * std::mem::size_of::<Option<bool>>(),
            ColumnData::Text(v) => v
                .iter()
                .map(|s| {
                    std::mem::size_of::<Option<String>>() + s.as_ref().map_or(0, |x| x.capacity())
                })
                .sum(),
        })
        .sum()
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let columns = sales_columns(n);

    let today = table_bytes(&columns);
    let plain = ArrowFrame::from_columns_with(&columns, Encoding::Plain).memory_size();
    let auto = ArrowFrame::from_columns(&columns);
    let encoded = auto.memory_size();

    let mb = |b: usize| b as f64 / 1_048_576.0;
    println!("the bundled example's six columns, {n} rows\n");
    println!("  Vec<Option<T>>        (today)      {:>8.1} MB", mb(today));
    println!(
        "  arrow, plain strings               {:>8.1} MB   {:.1}x",
        mb(plain),
        today as f64 / plain as f64
    );
    println!(
        "  arrow, encoding chosen per column  {:>8.1} MB   {:.1}x",
        mb(encoded),
        today as f64 / encoded as f64
    );
    println!();
    // Straight off the columns. Building a `Table` here to reach `schema()` would clone the
    // whole dataset — about 139 MB at the default million rows — inside the one example whose
    // subject is memory. The figures above are already measured, so it would not corrupt them;
    // it would just be an embarrassing way to read six strings.
    for (col, enc) in columns.iter().zip(auto.encodings()) {
        if let Some(e) = enc {
            let name = &col.name;
            println!("  {name:<10} text stored {e:?}");
        }
    }
}
