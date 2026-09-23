use proptest::prelude::*;
use tidefell_chunker::{chunk_reader, hash_chunk, hash_content, Params};

const SMALL: Params = Params {
    min: 64,
    avg: 256,
    max: 1024,
};

proptest! {
    #[test]
    fn manifest_describes_input(data in proptest::collection::vec(any::<u8>(), 0..20_000)) {
        let mut out = Vec::new();
        let manifest = chunk_reader(&data[..], SMALL, |r, bytes| {
            assert_eq!(r.hash, hash_chunk(bytes));
            out.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        prop_assert_eq!(&out, &data);
        prop_assert_eq!(manifest.content, hash_content(&data));
        prop_assert!(manifest.chunks.iter().all(|c| c.len <= SMALL.max));
    }
}
