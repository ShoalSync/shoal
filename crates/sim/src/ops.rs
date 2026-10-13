//! What a simulation does, and how a seed chooses it.

use serde::{Deserialize, Serialize};

/// Folders paths are made in: few, so devices clash on them.
pub const DIRS: [&str; 4] = ["", "docs/", "docs/old/", "Photos/"];
/// File names: few, so devices edit the same files, and two that differ only in case.
pub const NAMES: [&str; 5] = ["a.txt", "b.txt", "c.bin", "A.txt", "notes.md"];

/// One step of a simulation. Files and devices are picked by number, wrapped to what exists
/// when the step runs, so a list of steps stays valid when steps are removed from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Writes content at a path on a device, scanning after if `scan`. Content is never
    /// written twice: it is made from the step's position.
    Write {
        device: u8,
        path: u8,
        size: Size,
        scan: bool,
    },
    /// Renames or moves one of a device's files to a path, replacing what is there.
    Rename { device: u8, file: u8, to: u8 },
    /// Deletes one of a device's files.
    Delete { device: u8, file: u8 },
    /// Deletes one of a device's folders and everything in it.
    DeleteDir { device: u8, dir: u8 },
    /// Scans a device's folder.
    Scan { device: u8 },
    /// Splits the network: each device gets the side its bit says.
    Partition { sides: u8 },
    /// Joins the network again.
    Heal,
    /// Stops a device's engine and starts it again.
    Restart { device: u8 },
    /// Lets the engines work for a while.
    Wait { ms: u16 },
    /// Sets a device's clock this many minutes from the true time: skewed a little, or jumped
    /// by hours or days, short of the folder's retention.
    Clock { device: u8, minutes: i16 },
    /// Once the devices agree, stops a device, loses its folder's database and starts it again,
    /// rebuilding the folder (ADR 0034).
    LoseMetadata { device: u8 },
    /// Fills a device's disk, or frees it again (ADR 0037).
    Disk { device: u8, full: bool },
    /// Makes a device send its changes only, or sync both ways again (ADR 0025).
    SendOnly { device: u8, on: bool },
    /// Adds a user to a device's members of the folder, or removes them.
    Member { device: u8, add: bool },
}

/// How much a write writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    /// A few bytes.
    Tiny,
    /// One chunk.
    Small,
    /// Several chunks.
    Large,
}

impl Size {
    pub(crate) fn bytes(self) -> usize {
        match self {
            Self::Tiny => 16,
            Self::Small => 20_000,
            Self::Large => 700_000,
        }
    }
}

/// A path from a step's number.
pub(crate) fn path(n: u8) -> String {
    let n = usize::from(n);
    format!(
        "{}{}",
        DIRS[n % DIRS.len()],
        NAMES[(n / DIRS.len()) % NAMES.len()]
    )
}

/// A small, fast generator, so a seed means the same steps everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

/// The steps seed `seed` makes for `devices` devices.
pub fn generate(seed: u64, devices: u8, steps: usize) -> Vec<Op> {
    let mut rng = Rng::new(seed);
    let mut ops = Vec::with_capacity(steps);
    let mut partitioned = false;
    for _ in 0..steps {
        let device = rng.below(u64::from(devices)) as u8;
        let op = match rng.below(100) {
            0..=34 => Op::Write {
                device,
                path: rng.byte(),
                size: match rng.below(10) {
                    0..=5 => Size::Tiny,
                    6..=8 => Size::Small,
                    _ => Size::Large,
                },
                scan: rng.below(4) != 0,
            },
            35..=44 => Op::Rename {
                device,
                file: rng.byte(),
                to: rng.byte(),
            },
            45..=52 => Op::Delete {
                device,
                file: rng.byte(),
            },
            53..=54 => Op::DeleteDir {
                device,
                dir: rng.byte(),
            },
            55..=64 => Op::Scan { device },
            65..=72 if !partitioned => {
                partitioned = true;
                // At least one device on each side.
                let all = (1u16 << devices) - 1;
                let sides = (rng.below(u64::from(all) - 1) + 1) as u8;
                Op::Partition { sides }
            }
            65..=72 => {
                partitioned = false;
                Op::Heal
            }
            73..=75 => Op::Restart { device },
            76..=78 => Op::Clock {
                device,
                minutes: match rng.below(6) {
                    0 => 0,
                    1 => 3,
                    2 => -7,
                    3 => 90,
                    4 => -1_440,
                    _ => 2_880,
                },
            },
            79 => Op::LoseMetadata { device },
            80..=81 => Op::Disk {
                device,
                full: rng.below(2) == 0,
            },
            82..=83 => Op::SendOnly {
                device,
                on: rng.below(2) == 0,
            },
            84 => Op::Member {
                device,
                add: rng.below(2) == 0,
            },
            _ => Op::Wait {
                ms: (rng.below(300) + 20) as u16,
            },
        };
        ops.push(op);
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_always_makes_the_same_steps() {
        assert_eq!(generate(7, 3, 40), generate(7, 3, 40));
        assert_ne!(generate(7, 3, 40), generate(8, 3, 40));
    }

    #[test]
    fn paths_cover_every_folder_and_name() {
        let paths: std::collections::BTreeSet<String> = (0..=255).map(path).collect();
        assert_eq!(paths.len(), DIRS.len() * NAMES.len());
    }
}
