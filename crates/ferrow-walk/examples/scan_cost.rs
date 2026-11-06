//! Hosted-only same-tree none/default column scan-cost observation.
use ferrow_index::ColumnSet;
use ferrow_walk::{WalkOptions, scan};
use std::path::PathBuf;

struct Tree(PathBuf);
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("expected new fixture directory")?,
    );
    std::fs::create_dir(&root)?;
    let tree = Tree(root);
    for d in 0..100 {
        let dir = tree.0.join(format!("dir-{d}"));
        std::fs::create_dir(&dir)?;
        for i in 0..20 {
            std::fs::write(dir.join(format!("file-{i}.txt")), b"x")?;
        }
    }
    let mut none = Vec::new();
    let mut default = Vec::new();
    // Alternate the two arms to reduce warm-cache order bias.
    for run in 0..5 {
        let arms = if run % 2 == 0 {
            [ColumnSet::NONE, ColumnSet::DEFAULT]
        } else {
            [ColumnSet::DEFAULT, ColumnSet::NONE]
        };
        for columns in arms {
            let (index, report) = scan(&WalkOptions {
                roots: vec![tree.0.clone()],
                columns,
                ..Default::default()
            });
            if report.errors != 0 || index.len() != 2101 || index.columns_present() != columns {
                return Err(format!("invalid scan evidence: {report:?}").into());
            }
            let samples = if columns == ColumnSet::NONE {
                &mut none
            } else {
                &mut default
            };
            samples.push(report.elapsed.as_secs_f64() * 1000.0);
        }
    }
    none.sort_by(f64::total_cmp);
    default.sort_by(f64::total_cmp);
    println!(
        "FERROW-SCAN entries=2101 errors=0 none_ms_median={:.6} default_ms_median={:.6}",
        none[2], default[2]
    );
    Ok(())
}
