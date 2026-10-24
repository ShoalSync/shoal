//! Runs simulation seeds (ADR 0036). `SHOAL_SIM_SEEDS` sets how many (8 unless set),
//! `SHOAL_SIM_FIRST` the first, `SHOAL_SIM_REPEAT` how many times each is tried until it
//! fails, `SHOAL_SIM_JOBS` how many at once and `SHOAL_SIM_DIR`
//! where their folders go. The first failing seed, or the first `SHOAL_SIM_SHRINK`, is
//! shortened to the fewest steps that still fail and printed, to keep in `tests/regressions/`
//! as a JSON list of steps. The simulation runs on Unix.
#![cfg(unix)]

use std::path::PathBuf;

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn base() -> PathBuf {
    std::env::var_os("SHOAL_SIM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn seeds_pass() {
    let first = setting("SHOAL_SIM_FIRST", 0);
    let count = setting("SHOAL_SIM_SEEDS", 8);
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u64);
    let jobs = setting("SHOAL_SIM_JOBS", (cores / 2).clamp(1, 8));
    let base = base();
    std::fs::create_dir_all(&base).unwrap();
    let shrink = setting("SHOAL_SIM_SHRINK", 1) as usize;
    let repeat = setting("SHOAL_SIM_REPEAT", 1) as usize;
    let found =
        shoal_sim::check_seeds(first..first + count, repeat, jobs as usize, &base, shrink).await;
    for f in &found {
        eprintln!("{f}");
    }
    assert!(found.is_empty(), "{} of {count} seeds failed", found.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn regressions_pass() {
    // `SHOAL_SIM_REGRESSIONS` names them when this test runs away from its source, as in CI.
    let dir = std::env::var_os("SHOAL_SIM_REGRESSIONS").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/regressions"),
        PathBuf::from,
    );
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let base = base();
    std::fs::create_dir_all(&base).unwrap();
    for file in files {
        let steps: Vec<shoal_sim::Op> =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        if let Err(failure) = shoal_sim::run(&steps, shoal_sim::DEVICES, &base).await {
            panic!("{} failed {failure}", file.display());
        }
    }
}

/// Shortens the steps of `SHOAL_SIM_SHRINK_SEED`, a seed known to fail now and then, and
/// prints them; does nothing unless it is set.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn shrink_seed() {
    let Some(seed) = std::env::var("SHOAL_SIM_SHRINK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    else {
        return;
    };
    let base = base();
    std::fs::create_dir_all(&base).unwrap();
    let steps = shoal_sim::generate(seed, shoal_sim::DEVICES, shoal_sim::STEPS);
    let steps = shoal_sim::shorten(steps, &base).await;
    eprintln!(
        "seed {seed} shortened to {} steps:\n{}",
        steps.len(),
        serde_json::to_string(&steps).unwrap()
    );
}

/// Prints the steps of `SHOAL_SIM_PRINT_SEED` as JSON; does nothing unless it is set.
#[test]
fn print_seed() {
    let Some(seed) = std::env::var("SHOAL_SIM_PRINT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    else {
        return;
    };
    let steps = shoal_sim::generate(seed, shoal_sim::DEVICES, shoal_sim::STEPS);
    println!("STEPS {}", serde_json::to_string(&steps).unwrap());
}
