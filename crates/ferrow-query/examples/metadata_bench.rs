//! Hosted-only default-column observations; never a local performance workload.
use ferrow_index::{ColumnSet, Index, IndexBuilder};
use ferrow_query::{CancelToken, Query, SearchOptions, SortKey, count, search};
use std::time::Instant;
#[path = "support/sort_prototype.rs"]
mod sort_prototype;

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
    let (index, freeze) = b.freeze_with_timings();
    println!(
        "FERROW-METADATA-ACTIVE columns=size,modified values=packed40-and-seconds32 sort=lazy-dedicated-worker ids=packed24 unknown=last"
    );
    let path = std::path::Path::new(&args[0]);
    index.save(path)?;
    let core_path = path.with_extension("core.bin");
    let core = IndexBuilder::new();
    ferrow_fixture::generate_into(&core, n, 2026);
    let core = core.freeze();
    core.save(&core_path)?;
    let cold = index.heap_bytes();
    for file in [&core_path, path] {
        let loaded = Index::load(file)?;
        assert!(
            loaded
                .sort_ticket(SortKey::Modified)
                .is_none_or(|t| t.status() == ferrow_index::SortStatus::Unprepared)
        );
    }
    let mut core_load = Vec::new();
    let mut loads = Vec::new();
    for run in 0..3 {
        for metadata in if run % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let (loaded, t) = Index::load_with_timings(if metadata { path } else { &core_path })?;
            assert_eq!(loaded.len(), n);
            if metadata {
                assert_eq!(
                    loaded.sort_ticket(SortKey::Modified).unwrap().status(),
                    ferrow_index::SortStatus::Unprepared
                );
                loads.push(t.total.as_secs_f64() * 1000.0);
            } else {
                core_load.push(t.total.as_secs_f64() * 1000.0);
            }
        }
    }
    fn prepare(index: &Index) {
        for key in [SortKey::Size, SortKey::Modified] {
            index.request_sort(key);
        }
        for key in [SortKey::Size, SortKey::Modified] {
            assert_eq!(
                index
                    .sort_ticket(key)
                    .unwrap()
                    .wait_timeout(std::time::Duration::from_secs(30)),
                ferrow_index::SortStatus::Ready
            );
        }
    }
    let mut lazy = Vec::new();
    for _ in 0..3 {
        let loaded = Index::load(path)?;
        let t = Instant::now();
        prepare(&loaded);
        lazy.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    prepare(&index);
    let cache_path = path.with_extension("sort-prototype.bin");
    std::fs::write(&cache_path, sort_prototype::encode(&index))?;
    let mut persisted = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let loaded = Index::load(path)?;
        sort_prototype::validate(&loaded, &cache_path)?;
        persisted.push(started.elapsed().as_secs_f64() * 1000.0);
        assert!(loaded.metadata_order(SortKey::Modified, false).is_none());
    }
    let heap = index.heap_bytes();
    let metadata = index.metadata_heap_bytes();
    assert_eq!(heap - metadata, core.heap_bytes());
    println!(
        "FERROW-METADATA entries={n} seed=2026 core_heap_bytes={} default_heap_bytes={heap} cold_default_heap_bytes={cold} default_bytes_per_entry={:.9} metadata_bytes_per_entry={:.9} load_ms_median={:.6} core_load_ms_median={:.6} sort_during_load=0 lazy_build_ms_median={:.6} persisted_load_ms_median={:.6} lazy_saved_bytes={} freeze_ms={:.6}",
        heap - metadata,
        heap as f64 / n as f64,
        metadata as f64 / n as f64,
        median(loads),
        median(core_load),
        median(lazy),
        median(persisted),
        heap - cold,
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
