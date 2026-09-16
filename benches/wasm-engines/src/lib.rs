//! Exercise each candidate enough that the linker cannot dead-strip it.
//!
//! A `cargo check` answers "does it compile for wasm32". It does not answer "how big is
//! the artifact", and an empty `cdylib` that merely *depends* on a crate links almost
//! none of it. So each probe below builds a frame, filters it, sorts it and takes a
//! slice — the operations dagpane's seven verbs reduce to — and returns a number that
//! depends on the result, so nothing can be optimised away.

#[cfg(feature = "arrow-min")]
#[no_mangle]
pub extern "C" fn probe_arrow() -> i64 {
    use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    let n = 1000i64;
    let ids: Int64Array = (0..n).map(Some).collect();
    let regions: StringArray = (0..n).map(|i| Some(if i % 2 == 0 { "north" } else { "south" })).collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("region", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(regions)]).unwrap();

    // filter — the `filter` verb
    let mask: arrow_array::BooleanArray = (0..n).map(|i| Some(i % 3 == 0)).collect();
    let filtered = arrow_select::filter::filter_record_batch(&batch, &mask).unwrap();

    // sort — the `sort` verb
    let col = filtered.column(0);
    let indices = arrow_ord::sort::sort_to_indices(col, None, None).unwrap();
    let taken = arrow_select::take::take(col, &indices, None).unwrap();

    // head(n) — the `limit` verb
    let head = taken.slice(0, 10.min(taken.len()));
    head.len() as i64 + filtered.num_rows() as i64
}

#[cfg(feature = "arrow-parquet")]
#[no_mangle]
pub extern "C" fn probe_parquet() -> i64 {
    // Reading is what matters: a source is read, never written, by this engine.
    use parquet::file::reader::SerializedFileReader;
    use std::io::Cursor;
    // Deliberately invalid bytes: the point is to link the reader, not to read a file.
    let bytes: Vec<u8> = b"PAR1not-a-real-file".to_vec();
    match SerializedFileReader::new(bytes::Bytes::from(bytes)) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

#[cfg(feature = "datafusion")]
#[no_mangle]
pub extern "C" fn probe_datafusion() -> i64 {
    use datafusion::prelude::SessionContext;
    let ctx = SessionContext::new();
    // Constructing the context links the planner, the optimiser and the execution engine.
    std::hint::black_box(&ctx);
    1
}

#[cfg(feature = "polars")]
#[no_mangle]
pub extern "C" fn probe_polars() -> i64 {
    use polars::prelude::*;
    let df = df!["id" => &[1i64, 2, 3], "region" => &["north", "south", "north"]].unwrap();
    let out = df.lazy().filter(col("id").gt(lit(1))).collect().unwrap();
    out.height() as i64
}

#[cfg(feature = "polars-parquet")]
#[no_mangle]
pub extern "C" fn probe_polars_parquet() -> i64 {
    use polars_pq::prelude::*;
    let df = df!["id" => &[1i64, 2, 3]].unwrap();
    let mut buf = std::io::Cursor::new(Vec::new());
    // Linking the Parquet writer is the point: it is what pulls the compression codecs
    // ROADMAP §2 says do not cross-compile.
    match ParquetWriter::new(&mut buf).finish(&mut df.clone()) {
        Ok(_) => buf.into_inner().len() as i64,
        Err(_) => -1,
    }
}
