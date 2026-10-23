//! Members' priorities for what storage peers keep of an encrypted folder (ADR 0023's amendment
//! of 2026-10-22; `specs/storage-v1.md`).
//!
//! The rules are in `.tidefellpriorities` at the folder's root, a file synced as any other, so
//! every member device has the same. A member device that holds the key works out each current
//! entry's priority from them, and gives the folder's storage peers the lot whenever it differs
//! from what it last gave them. One without the file on disk, as a partial replica may be, fetches
//! it into memory.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tidefell_proto::storage::{Hint, DEFAULT_PRIORITY, MAX_HINTS};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, FolderId};
use tracing::{debug, info, warn};

use crate::engine::Inner;
use crate::{materialize, Result};

/// The file at a folder's root that ranks its files for storage peers.
pub(crate) const PRIORITIES_FILE: &str = ".tidefellpriorities";

/// A folder's rules: each a priority and the pattern it gives it to, the last match winning.
pub(crate) struct Rules(Vec<(u8, String, Gitignore)>);

/// Checks that `pattern` can be a rule's: one line, a valid pattern as in `.tidefellignore`.
pub(crate) fn check_rule(pattern: &str) -> std::result::Result<(), String> {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern.starts_with('#') || pattern.contains(['\n', '\r']) {
        return Err(format!("{pattern:?} is not a pattern"));
    }
    let mut builder = GitignoreBuilder::new("");
    builder
        .add_line(None, pattern)
        .map_err(|e| format!("{pattern:?} is not a valid pattern: {e}"))?;
    builder
        .build()
        .map(drop)
        .map_err(|e| format!("{pattern:?} is not a valid pattern: {e}"))
}

/// `text`, a priorities file, with its rule for `pattern` replaced by one at `priority`, or
/// removed for `None`. Comments and other rules stay as they are.
pub(crate) fn set_rule(text: &str, pattern: &str, priority: Option<u8>) -> String {
    let pattern = pattern.trim();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| {
            let line = line.trim();
            line.starts_with('#')
                || line
                    .split_once(char::is_whitespace)
                    .is_none_or(|(_, p)| p.trim() != pattern)
        })
        .map(str::to_owned)
        .collect();
    if let Some(priority) = priority {
        lines.push(format!("{priority} {pattern}"));
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The folder's rules file as this device has it: from disk, or fetched where it is not on
/// disk, as on a partial replica; none if the folder has no such file. `cache` keeps what was
/// fetched by version, so each is fetched once.
pub(crate) async fn text(
    inner: &Arc<Inner>,
    node: &Arc<tidefell_net::Node>,
    folder: FolderId,
    cache: Option<&mut HashMap<FolderId, (ContentHash, String)>>,
) -> Result<Option<String>> {
    let handle = inner.folder(&folder)?;
    let (root, content) = {
        let state = handle.lock();
        let content = state
            .meta
            .find_path(PRIORITIES_FILE)
            .and_then(|v| match v.kind {
                EntryKind::File { content, .. } => Some(*content),
                _ => None,
            });
        (state.config.path.clone(), content)
    };
    if let Ok(text) = std::fs::read_to_string(root.join(PRIORITIES_FILE)) {
        return Ok(Some(text));
    }
    let Some(content) = content else {
        return Ok(None);
    };
    if let Some((cached, text)) = cache.as_ref().and_then(|c| c.get(&folder)) {
        if *cached == content {
            return Ok(Some(text.clone()));
        }
    }
    let mut bytes = Vec::new();
    materialize::fetch_into(
        inner,
        node,
        folder,
        &handle,
        PRIORITIES_FILE,
        Some(content),
        &mut |_, data| {
            bytes.extend_from_slice(data);
            Ok(())
        },
    )
    .await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if let Some(cache) = cache {
        cache.insert(folder, (content, text.clone()));
    }
    Ok(Some(text))
}

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
            let pattern = pattern.trim();
            let mut builder = GitignoreBuilder::new(root);
            let built = match check_rule(pattern) {
                Ok(()) => match builder.add_line(None, pattern) {
                    Ok(_) => builder.build().ok(),
                    Err(_) => None,
                },
                Err(_) => None,
            };
            match built {
                Some(matcher) => rules.push((priority, pattern.to_owned(), matcher)),
                None => warn!(line, "skipping a priority rule whose pattern is not valid"),
            }
        }
        Self(rules)
    }

    /// The rules, in order: each priority and pattern.
    pub(crate) fn listed(&self) -> impl Iterator<Item = (u8, &str)> {
        self.0.iter().map(|(p, pattern, _)| (*p, pattern.as_str()))
    }

    /// The priority of the folder-relative `path`.
    pub(crate) fn priority(&self, path: &str) -> u8 {
        self.ranking(path, false).0
    }

    /// The priority of the folder-relative `path`, a directory if `directory`, and the index of
    /// the rule that gives it, if one does.
    pub(crate) fn ranking(&self, path: &str, directory: bool) -> (u8, Option<usize>) {
        self.0
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, _, m))| m.matched_path_or_any_parents(path, directory).is_ignore())
            .map_or((DEFAULT_PRIORITY, None), |(i, (p, _, _))| (*p, Some(i)))
    }
}

/// What a member device last gave each storage peer of each folder: a digest of the hints, or
/// none where the peer does not speak the protocol; and the rules it fetched, by version.
#[derive(Default)]
pub(crate) struct Given {
    given: HashMap<(FolderId, DeviceId), Option<blake3::Hash>>,
    fetched: HashMap<FolderId, (ContentHash, String)>,
}

/// Gives the storage peers of each encrypted folder this device is a member of the priorities
/// of its current files, if they differ from what it last gave them.
pub(crate) async fn give(inner: &Arc<Inner>, node: &Arc<tidefell_net::Node>, given: &mut Given) {
    for handle in inner.handles() {
        let folder = {
            let state = handle.lock();
            let Some(encryption) = state.config.encryption.as_ref() else {
                continue;
            };
            if state.config.is_stored() || encryption.storage_peers.is_empty() {
                continue;
            }
            state.config.id
        };
        let text = match text(inner, node, folder, Some(&mut given.fetched)).await {
            Ok(Some(text)) => text,
            // No rules, or none any more: every file is at the default.
            Ok(None) => String::new(),
            Err(e) => {
                debug!(%folder, error = %e, "cannot read the priorities yet");
                continue;
            }
        };
        let (root, peers, hints) = {
            let state = handle.lock();
            let Some(encryption) = state.config.encryption.as_ref() else {
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
            match given.given.get(&(folder, peer)) {
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
                    given.given.insert((folder, peer), Some(digest));
                }
                Err(tidefell_net::Error::Rejected(e)) => {
                    debug!(%folder, peer = %peer, error = %e, "a storage peer refused the priorities");
                    given.given.insert((folder, peer), Some(digest));
                }
                Err(e) if unspoken(&e) => {
                    debug!(%folder, peer = %peer, "the storage peer does not take priorities");
                    given.given.insert((folder, peer), None);
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

    #[test]
    fn a_rule_replaces_the_one_for_its_pattern() {
        let text = "# what to keep\n200 Photos/\n0 *.iso\n";
        assert_eq!(
            set_rule(text, "Photos/", Some(150)),
            "# what to keep\n0 *.iso\n150 Photos/\n"
        );
        assert_eq!(
            set_rule(text, "*.iso", None),
            "# what to keep\n200 Photos/\n"
        );
        assert_eq!(set_rule("", "docs/", Some(10)), "10 docs/\n");
        let rules = Rules::parse(Path::new("/f"), &set_rule(text, "Photos/raw/", Some(50)));
        assert_eq!(
            rules.listed().collect::<Vec<_>>(),
            [(200, "Photos/"), (0, "*.iso"), (50, "Photos/raw/")]
        );
        assert_eq!(rules.ranking("Photos/raw", true), (50, Some(2)));
        assert_eq!(rules.ranking("Photos", true), (200, Some(0)));
        assert_eq!(rules.ranking("notes.md", false), (DEFAULT_PRIORITY, None));
    }

    #[test]
    fn an_anchored_rule_ranks_its_directory() {
        let rules = Rules::parse(Path::new("/f"), "200 /Photos/\n5 /a\\*.txt\n");
        assert_eq!(rules.ranking("Photos", true), (200, Some(0)));
        assert_eq!(rules.ranking("Photos/a.jpg", false), (200, Some(0)));
        assert_eq!(rules.ranking("x/Photos", true), (DEFAULT_PRIORITY, None));
        assert_eq!(rules.ranking("a*.txt", false), (5, Some(1)));
        assert_eq!(rules.ranking("ab.txt", false), (DEFAULT_PRIORITY, None));
    }

    #[test]
    fn a_rule_is_one_valid_pattern() {
        assert!(check_rule("Photos/").is_ok());
        assert!(check_rule("*.iso").is_ok());
        assert!(check_rule("").is_err());
        assert!(check_rule("# comment").is_err());
        assert!(check_rule("a\n200 b").is_err());
        assert!(check_rule("[z-a]").is_err());
    }
}
