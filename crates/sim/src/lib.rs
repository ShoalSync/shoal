//! Simulation testing (ADR 0036): several engines in one process on an in-memory network, each
//! with its own folder, driven through steps a seed chooses: files written, renamed and deleted
//! on any device, the network split and joined, devices restarted. Once the network heals and
//! the engines settle, every device must have the same files, no device may have lost content
//! its user did not remove, and no engine may have failed.
//!
//! Timing is real, so a seed does not always fail the same way: a failing seed is shortened to
//! the fewest steps that still fail in one of a few runs, and kept as a list of steps.

mod ops;
mod world;

use std::path::Path;
use std::sync::Arc;

pub use ops::{generate, Op, Size};
pub use world::{run, Failure};

/// Devices in each simulation.
pub const DEVICES: u8 = 3;
/// Steps each seed makes.
pub const STEPS: usize = 40;

/// A seed that failed: its steps, shortened, and why.
#[derive(Debug, Clone)]
pub struct Found {
    pub seed: u64,
    pub failure: Failure,
    /// The fewest steps found that still fail.
    pub steps: Vec<Op>,
}

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "seed {} failed {}", self.seed, self.failure)?;
        writeln!(f, "the {} steps that still fail:", self.steps.len())?;
        write!(
            f,
            "{}",
            serde_json::to_string_pretty(&self.steps).unwrap_or_default()
        )
    }
}

/// Runs `seeds`, each up to `repeat` times until it fails, with `jobs` at a time under `base`,
/// and returns those that failed, the first `shrink` of them shortened. Each failure is printed
/// as it is found.
pub async fn check_seeds(
    seeds: std::ops::Range<u64>,
    repeat: usize,
    jobs: usize,
    base: &Path,
    shrink: usize,
) -> Vec<Found> {
    let base = Arc::new(base.to_path_buf());
    let permits = Arc::new(tokio::sync::Semaphore::new(jobs.max(1)));
    let mut running = tokio::task::JoinSet::new();
    for seed in seeds {
        let (base, permits) = (base.clone(), permits.clone());
        running.spawn(async move {
            let _permit = permits.acquire_owned().await.ok()?;
            let steps = generate(seed, DEVICES, STEPS);
            for _ in 0..repeat.max(1) {
                if let Err(failure) = run(&steps, DEVICES, &base).await {
                    eprintln!(
                        "seed {seed} failed {failure}\nits devices' files, step by step:\n{}",
                        failure.history.join("\n")
                    );
                    return Some((seed, failure, steps));
                }
            }
            None
        });
    }
    let mut failed = Vec::new();
    while let Some(done) = running.join_next().await {
        if let Ok(Some(found)) = done {
            failed.push(found);
        }
    }
    failed.sort_by_key(|(seed, _, _)| *seed);
    let mut out = Vec::new();
    for (n, (seed, failure, steps)) in failed.into_iter().enumerate() {
        let steps = if n < shrink {
            shorten(steps, &base).await
        } else {
            steps
        };
        out.push(Found {
            seed,
            failure,
            steps,
        });
    }
    out
}

/// Runs `copies` of `steps` at once, as many seeds run together and some failures need the
/// load; true if any fails.
async fn fails(steps: &[Op], base: &Path, copies: usize) -> bool {
    let mut running = tokio::task::JoinSet::new();
    for _ in 0..copies {
        let (steps, base) = (steps.to_vec(), base.to_path_buf());
        running.spawn(async move { run(&steps, DEVICES, &base).await.is_err() });
    }
    let mut failed = false;
    while let Some(done) = running.join_next().await {
        failed |= done.unwrap_or(true);
    }
    failed
}

/// The fewest of `steps` found that still fail: chunks of steps are left out while the rest
/// still fails, halving the chunks until single steps.
pub async fn shorten(mut steps: Vec<Op>, base: &Path) -> Vec<Op> {
    let mut chunk = steps.len() / 2;
    while chunk > 0 {
        let mut at = 0;
        let mut removed = false;
        while at < steps.len() {
            let end = (at + chunk).min(steps.len());
            let candidate: Vec<Op> = steps[..at].iter().chain(&steps[end..]).cloned().collect();
            let copies = std::env::var("TIDEFELL_SIM_COPIES")
                .ok()
                .and_then(|c| c.parse().ok())
                .unwrap_or(8);
            if fails(&candidate, base, copies).await {
                steps = candidate;
                removed = true;
            } else {
                at += chunk;
            }
        }
        if !removed {
            chunk /= 2;
        }
    }
    steps
}
