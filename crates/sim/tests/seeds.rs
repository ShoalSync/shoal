//! Runs simulation seeds (ADR 0036). `TIDEFELL_SIM_SEEDS` sets how many (8 unless set),
//! `TIDEFELL_SIM_FIRST` the first, `TIDEFELL_SIM_REPEAT` how many times each is tried until it
//! fails, `TIDEFELL_SIM_JOBS` how many at once and `TIDEFELL_SIM_DIR`
//! where their folders go. The first failing seed, or the first `TIDEFELL_SIM_SHRINK`, is
//! shortened to the fewest steps that still fail and printed, to keep in `tests/regressions/`
//! as a JSON list of steps.

use std::path::PathBuf;

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn base() -> PathBuf {
    std::env::var_os("TIDEFELL_SIM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn seeds_pass() {
    let first = setting("TIDEFELL_SIM_FIRST", 0);
    let count = setting("TIDEFELL_SIM_SEEDS", 8);
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u64);
    let jobs = setting("TIDEFELL_SIM_JOBS", (cores / 2).clamp(1, 8));
    let base = base();
    std::fs::create_dir_all(&base).unwrap();
    let shrink = setting("TIDEFELL_SIM_SHRINK", 1) as usize;
    let repeat = setting("TIDEFELL_SIM_REPEAT", 1) as usize;
    let found =
        tidefell_sim::check_seeds(first..first + count, repeat, jobs as usize, &base, shrink).await;
    for f in &found {
        eprintln!("{f}");
    }
    assert!(found.is_empty(), "{} of {count} seeds failed", found.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn regressions_pass() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/regressions");
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
        let steps: Vec<tidefell_sim::Op> =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        if let Err(failure) = tidefell_sim::run(&steps, tidefell_sim::DEVICES, &base).await {
            panic!("{} failed {failure}", file.display());
        }
    }
}

/// Shortens the steps of `TIDEFELL_SIM_SHRINK_SEED`, a seed known to fail now and then, and
/// prints them; does nothing unless it is set.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn shrink_seed() {
    let Some(seed) = std::env::var("TIDEFELL_SIM_SHRINK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    else {
        return;
    };
    let base = base();
    std::fs::create_dir_all(&base).unwrap();
    let steps = tidefell_sim::generate(seed, tidefell_sim::DEVICES, tidefell_sim::STEPS);
    let steps = tidefell_sim::shorten(steps, &base).await;
    eprintln!(
        "seed {seed} shortened to {} steps:\n{}",
        steps.len(),
        serde_json::to_string(&steps).unwrap()
    );
}

/// Prints the steps of `TIDEFELL_SIM_PRINT_SEED` as JSON; does nothing unless it is set.
#[test]
fn print_seed() {
    let Some(seed) = std::env::var("TIDEFELL_SIM_PRINT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    else {
        return;
    };
    let steps = tidefell_sim::generate(seed, tidefell_sim::DEVICES, tidefell_sim::STEPS);
    println!("STEPS {}", serde_json::to_string(&steps).unwrap());
}
