//! Availability bitmaps: which chunks of a manifest a replica holds.

/// One bit per chunk of a manifest, in manifest order. Bit `i` of byte `i / 8` is chunk `i`
/// (least significant bit first).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Availability {
    bits: Vec<u8>,
    len: usize,
}

impl Availability {
    /// A bitmap of `len` chunks, none available.
    pub fn empty(len: usize) -> Self {
        Self {
            bits: vec![0; len.div_ceil(8)],
            len,
        }
    }

    /// A bitmap of `len` chunks where chunk `i` is available if `f(i)`.
    pub fn from_fn(len: usize, mut f: impl FnMut(usize) -> bool) -> Self {
        let mut out = Self::empty(len);
        for i in 0..len {
            if f(i) {
                out.set(i, true);
            }
        }
        out
    }

    /// Number of chunks the bitmap covers.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if the bitmap covers no chunks.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether chunk `i` is available.
    ///
    /// # Panics
    ///
    /// If `i >= len()`.
    pub fn get(&self, i: usize) -> bool {
        assert!(i < self.len, "chunk index {i} out of range {}", self.len);
        self.bits[i / 8] & (1 << (i % 8)) != 0
    }

    /// Marks chunk `i` available or not.
    ///
    /// # Panics
    ///
    /// If `i >= len()`.
    pub fn set(&mut self, i: usize, available: bool) {
        assert!(i < self.len, "chunk index {i} out of range {}", self.len);
        if available {
            self.bits[i / 8] |= 1 << (i % 8);
        } else {
            self.bits[i / 8] &= !(1 << (i % 8));
        }
    }

    /// Number of available chunks.
    pub fn count(&self) -> usize {
        self.bits.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// True if every chunk is available.
    pub fn is_complete(&self) -> bool {
        self.count() == self.len
    }

    /// Indices of missing chunks.
    pub fn missing(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|&i| !self.get(i))
    }

    /// The raw bitmap bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_count() {
        let mut a = Availability::empty(10);
        assert_eq!(a.as_bytes().len(), 2);
        a.set(0, true);
        a.set(9, true);
        assert!(a.get(0) && a.get(9) && !a.get(5));
        assert_eq!(a.count(), 2);
        assert_eq!(a.missing().count(), 8);
        assert_eq!(a.as_bytes(), [0b0000_0001, 0b0000_0010]);
        a.set(0, false);
        assert_eq!(a.count(), 1);
    }

    #[test]
    fn empty_bitmap_is_complete() {
        assert!(Availability::empty(0).is_complete());
    }
}
