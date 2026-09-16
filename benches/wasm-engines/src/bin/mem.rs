//! How the three representations hold the SAME column: 1M rows of a four-value
//! categorical, which is what `region` and `channel` are in the bundled example.
use arrow_array::{Array, DictionaryArray, Int32Array, StringArray};
use std::sync::Arc;

fn main() {
    const N: usize = 1_000_000;
    let vals = ["north", "south", "east", "west"];

    let today: Vec<Option<String>> = (0..N).map(|i| Some(vals[i % 4].to_string())).collect();
    let today_bytes: usize = today.iter().map(|s| {
        std::mem::size_of::<Option<String>>() + s.as_ref().map_or(0, |v| v.capacity())
    }).sum();

    let arrow: StringArray = (0..N).map(|i| Some(vals[i % 4])).collect();
    let arrow_bytes = arrow.get_array_memory_size();

    let keys: Int32Array = (0..N).map(|i| Some((i % 4) as i32)).collect();
    let dict_values = StringArray::from(vals.to_vec());
    let dict: DictionaryArray<arrow_array::types::Int32Type> =
        DictionaryArray::try_new(keys, Arc::new(dict_values)).unwrap();
    let dict_bytes = dict.get_array_memory_size();

    let mb = |b: usize| b as f64 / 1_048_576.0;
    println!("one 1M-row categorical column, four distinct values:");
    println!("  Vec<Option<String>>  (today)       {:>8.1} MB", mb(today_bytes));
    println!("  arrow StringArray                  {:>8.1} MB   {:.1}x smaller",
             mb(arrow_bytes), today_bytes as f64 / arrow_bytes as f64);
    println!("  arrow DictionaryArray              {:>8.1} MB   {:.1}x smaller",
             mb(dict_bytes), today_bytes as f64 / dict_bytes as f64);
    std::hint::black_box((today.len(), arrow.len(), dict.len()));
}
