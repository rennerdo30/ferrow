//! Hosted-only default-column observations; never a local performance workload.
use ferrow_index::{ColumnSet, Index, IndexBuilder};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, count, search};
use std::time::Instant;

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err("expected destination index".into());
    }
    let n = std::env::var("FERROW_BENCH_ENTRIES")
        .ok()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(5_000_000);
    let b = IndexBuilder::with_columns(ColumnSet::DEFAULT);
    ferrow_fixture::generate_into(&b, n, 2026);
    let (mut index, freeze) = b.freeze_with_timings();
    println!(
        "FERROW-METADATA-ACTIVE columns=size,modified sort=precomputed-both-directions unknown=last"
    );
    let mut lazy = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        index.rebuild_metadata_orders();
        lazy.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let path = std::path::Path::new(&args[0]);
    index.save(path)?;
    let mut loads = Vec::new();
    for _ in 0..3 {
        let (loaded, t) = Index::load_with_timings(path)?;
        assert_eq!(loaded.columns_present(), ColumnSet::DEFAULT);
        loads.push(t.total.as_secs_f64() * 1000.0);
    }
    let heap = index.heap_bytes();
    let metadata = index.metadata_heap_bytes();
    println!(
        "FERROW-METADATA entries={n} seed=2026 core_heap_bytes={} default_heap_bytes={heap} default_bytes_per_entry={:.9} metadata_bytes_per_entry={:.9} load_ms_median={:.6} precomputed_build_ms={:.6} lazy_build_ms_median={:.6} lazy_saved_bytes={} freeze_ms={:.6}",
        heap - metadata,
        heap as f64 / n as f64,
        metadata as f64 / n as f64,
        median(loads),
        freeze.metadata_sort.as_secs_f64() * 1000.0,
        median(lazy),
        16 * n,
        freeze.total.as_secs_f64() * 1000.0
    );
    for text in [
        "a",
        "re",
        "report 2024",
        "!tmp log",
        "zzzz-no-hit-zzzz",
        "文書",
    ] {
        let query = Query::parse(text)?;
        let mut pages = Vec::new();
        let mut counts = Vec::new();
        let mut hits = 0;
        let mut total = 0;
        for _ in 0..5 {
            let t = Instant::now();
            let result = search(
                &index,
                &query,
                &SearchOptions {
                    sort: SortKey::Modified,
                    descending: true,
                    limit: Some(100),
                    ..Default::default()
                },
            );
            pages.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(!result.cancelled);
            assert_eq!(result.sort_unavailable, None);
            hits = result.hits.len();
            let t = Instant::now();
            let (n, cancelled) = count(&index, &query, &CancelToken::new());
            counts.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(!cancelled);
            total = n;
        }
        println!(
            "FERROW-MODIFIED-QUERY query={text:?} first_page_ms={:.6} page_hits={hits} full_count_ms={:.6} total_hits={total}",
            median(pages),
            median(counts)
        );
    }
    Ok(())
}
