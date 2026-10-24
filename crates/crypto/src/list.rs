//! List blobs: an encrypted file's chunk IDs beyond what its version lists itself, in the clear so
//! storage peers can keep them (`specs/encryption-v1.md`).

use shoal_proto::{ChunkId, ListId};

use crate::{Error, Result};

/// The most chunk IDs in one list blob, and in one version's own list.
pub const MAX_LIST: usize = 4096;

/// The list blob of `ids` and its ID.
pub fn list_blob(ids: &[ChunkId]) -> Result<(ListId, Vec<u8>)> {
    if ids.len() > MAX_LIST {
        return Err(Error::TooLarge("chunk list"));
    }
    let blob = postcard::to_stdvec(ids).map_err(|_| Error::Malformed("list blob"))?;
    Ok((ListId(*blake3::hash(&blob).as_bytes()), blob))
}

/// The chunk IDs in a list blob fetched as `id`, once it is checked to be that blob.
pub fn read_list_blob(id: &ListId, blob: &[u8]) -> Result<Vec<ChunkId>> {
    if blake3::hash(blob).as_bytes() != id.as_bytes() {
        return Err(Error::Mismatch("list blob"));
    }
    let (ids, rest): (Vec<ChunkId>, _) =
        postcard::take_from_bytes(blob).map_err(|_| Error::Malformed("list blob"))?;
    if !rest.is_empty() {
        return Err(Error::Malformed("list blob"));
    }
    if ids.len() > MAX_LIST {
        return Err(Error::TooLarge("chunk list"));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_round_trip_and_are_checked() {
        let ids = vec![ChunkId([1; 32]), ChunkId([2; 32])];
        let (id, blob) = list_blob(&ids).unwrap();
        assert_eq!(blob.len(), 1 + 64);
        assert_eq!(read_list_blob(&id, &blob).unwrap(), ids);
        assert_eq!(
            read_list_blob(&ListId([0; 32]), &blob),
            Err(Error::Mismatch("list blob"))
        );
        let longer = [blob.as_slice(), &[0]].concat();
        let longer_id = ListId(*blake3::hash(&longer).as_bytes());
        assert_eq!(
            read_list_blob(&longer_id, &longer),
            Err(Error::Malformed("list blob"))
        );
        assert!(list_blob(&vec![ChunkId([0; 32]); MAX_LIST]).is_ok());
        assert_eq!(
            list_blob(&vec![ChunkId([0; 32]); MAX_LIST + 1]),
            Err(Error::TooLarge("chunk list"))
        );
    }
}
