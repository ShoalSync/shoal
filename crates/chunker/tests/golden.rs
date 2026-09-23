//! Pins chunk boundaries for the default parameters. Different boundaries would not break
//! interoperability (manifests list chunks explicitly) but would defeat deduplication against
//! existing data, so a change here needs a deliberate decision.

use tidefell_chunker::{manifest_of, Params};

/// Deterministic pseudo-random bytes (xorshift64).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn default_params_boundaries_are_stable() {
    let manifest = manifest_of(&noise(4 * 1024 * 1024), Params::DEFAULT);
    let lengths: Vec<u32> = manifest.chunks.iter().map(|c| c.len).collect();
    let expected: &[u32] = &EXPECTED;
    assert_eq!(lengths, expected, "chunk boundaries changed");
}

const EXPECTED: [u32; 13] = [
    477054, 563279, 238528, 305109, 392241, 414373, 84148, 171831, 499346, 366457, 365408, 204590,
    111940,
];
