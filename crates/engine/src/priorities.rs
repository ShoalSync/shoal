//! Members' priorities for what storage peers keep of an encrypted folder (ADR 0023's amendment
//! of 2026-10-22; `specs/storage-v1.md`).
//!
//! The rules are in `.tidefellpriorities` at the folder's root, a file synced as any other, so
//! every member device has the same. A member device that holds the key works out each current
//! entry's priority from them, and gives the folder's storage peers the lot whenever it differs
//! from what it last gave them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tidefell_proto::storage::{Hint, DEFAULT_PRIORITY, MAX_HINTS};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{DeviceId, FolderId};
use tracing::{debug, info, warn};

use crate::engine::Inner;

/// The file at a folder's root that ranks its files for storage peers.
pub(crate) const PRIORITIES_FILE: &str = ".tidefellpriorities";

/// A folder's rules: each a priority and the patterns it gives it to, the last match winning.
pub(crate) struct Rules(Vec<(u8, Gitignore)>);

impl Rules {
    /// Reads `<priority> <pattern>` lines; blank lines and those starting with `#` are none,
    /// and a line that is not a rule is skipped.
    pub(crate) fn parse(root: &Path, text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((priority, pattern)) = line.split_once(char::is_whitespace) else {
                warn!(line, "skipping a priority rule with no pattern");
                continue;
            };
            let Ok(priority) = priority.parse::<u8>() else {
                warn!(
                    line,
                    "skipping a priority rule whose priority is not 0 to 255"
                );
                continue;
            };
            let mut builder = GitignoreBuilder::new(root);
            let built = match builder.add_line(None, pattern.trim()) {
                Ok(_) => builder.build().ok(),
                Err(_) => None,
            };
            match built {
                Some(matcher) => rules.push((priority, matcher)),
                None => warn!(line, "skipping a priority rule whose pattern is not valid"),
            }
        }
        Self(rules)
    }

    /// The priority of the folder-relative `path`.
    pub(crate) fn priority(&self, path: &str) -> u8 {
        self.0
            .iter()
            .rev()
            .find(|(_, m)| m.matched_path_or_any_parents(path, false).is_ignore())
            .map_or(DEFAULT_PRIORITY, |(p, _)| *p)
    }
}

/// What a member device last gave each storage peer of each folder: a digest of the hints, and
/// whether the peer does not speak the protocol.
#[derive(Default)]
pub(crate) struct Given(HashMap<(FolderId, DeviceId), Option<blake3::Hash>>);

/// Gives the storage peers of each encrypted folder this device is a member of the priorities
/// of its current files, if they differ from what it last gave them.
pub(crate) async fn give(inner: &Arc<Inner>, node: &Arc<tidefell_net::Node>, given: &mut Given) {
    for handle in inner.handles() {
        let (folder, root, peers, hints) = {
            let state = handle.lock();
            let Some(encryption) = state.config.encryption.as_ref() else {
                continue;
            };
            if state.config.is_stored() || encryption.storage_peers.is_empty() {
                continue;
            }
            // Read from disk: a device without the file on it leaves it to one with it.
            let Ok(text) = std::fs::read_to_string(state.config.path.join(PRIORITIES_FILE)) else {
                continue;
            };
            let rules = Rules::parse(&state.config.path, &text);
            let mut hints: Vec<Hint> = state
                .meta
                .entries()
                .filter(|v| matches!(v.kind, EntryKind::File { .. }))
                .map(|v| Hint {
                    entry: v.entry,
                    priority: rules.priority(v.path),
                })
                .collect();
            hints.sort_by_key(|h| h.entry);
            (
                state.config.id,
                state.config.path.clone(),
                encryption.storage_peers.clone(),
                hints,
            )
        };
        let digest = {
            let mut hasher = blake3::Hasher::new();
            for hint in &hints {
                hasher.update(hint.entry.as_bytes());
                hasher.update(&[hint.priority]);
            }
            hasher.finalize()
        };
        for peer in peers {
            // Given already, or it does not speak the protocol; and only to a peer connected.
            match given.0.get(&(folder, peer)) {
                Some(None) => continue,
                Some(Some(last)) if *last == digest => continue,
                _ => {}
            }
            if node.encrypted_peer(&peer).is_none() {
                continue;
            }
            let Ok(endpoint) = tidefell_net::EndpointId::from_bytes(peer.as_bytes()) else {
                continue;
            };
            let issued_ms = inner.now_ms();
            let mut sent = Ok(());
            for page in hints.chunks(MAX_HINTS) {
                sent = node
                    .send_priorities(endpoint, folder, issued_ms, page.to_vec())
                    .await;
                if sent.is_err() {
                    break;
                }
            }
            match sent {
                Ok(()) => {
                    info!(%folder, peer = %peer, files = hints.len(), "gave a storage peer the folder's priorities");
                    given.0.insert((folder, peer), Some(digest));
                }
                Err(tidefell_net::Error::Rejected(e)) => {
                    debug!(%folder, peer = %peer, error = %e, "a storage peer refused the priorities");
                    given.0.insert((folder, peer), Some(digest));
                }
                Err(e) if unspoken(&e) => {
                    debug!(%folder, peer = %peer, "the storage peer does not take priorities");
                    given.0.insert((folder, peer), None);
                }
                Err(e) => {
                    debug!(%folder, peer = %peer, root = %root.display(), error = %e, "cannot give the priorities yet");
                }
            }
        }
    }
}

/// Whether `error` says the peer does not speak storage/1: it refused the protocol.
fn unspoken(error: &tidefell_net::Error) -> bool {
    let error = error.to_string().to_lowercase();
    ["alpn", "application protocol", "unknown protocol"]
        .iter()
        .any(|says| error.contains(says))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_matching_rule_ranks_a_file() {
        let rules = Rules::parse(
            Path::new("/f"),
            "# what to keep first\n200 Photos/\n0 *.iso\n\n50 Photos/raw/\nnot a rule\n300 x\n",
        );
        assert_eq!(rules.priority("Photos/a.jpg"), 200);
        assert_eq!(rules.priority("Photos/raw/b.cr2"), 50);
        assert_eq!(rules.priority("disc.iso"), 0);
        assert_eq!(rules.priority("Photos/disc.iso"), 0);
        assert_eq!(rules.priority("notes.md"), DEFAULT_PRIORITY);
        // `300 x` is not a priority: no rule.
        assert_eq!(rules.priority("x"), DEFAULT_PRIORITY);
    }
}
