//! Conversions between engine views and API types, and parsing of identifiers.

use std::str::FromStr;

use tidefell_engine::{
    Event, FileInfo, FolderInfo, PeerAddr, PeerInfo, RecycledInfo, ReplicaMode, Retention,
};
use tidefell_sdk::types as api;

use crate::handlers::ApiFailure;

pub(crate) fn parse<T: FromStr>(what: &str, value: &str) -> Result<T, ApiFailure> {
    value.parse().map_err(|_| {
        ApiFailure::bad_request(format!("{what} {value:?} is not 64 lowercase hex digits"))
    })
}

pub(crate) fn folder(f: FolderInfo) -> api::Folder {
    let (mode, budget_bytes) = match f.mode {
        ReplicaMode::Full => (api::Mode::Full, None),
        ReplicaMode::Partial { budget_bytes } => (api::Mode::Partial, Some(budget_bytes)),
    };
    api::Folder {
        id: f.id.to_string(),
        path: f.path.display().to_string(),
        mode,
        budget_bytes,
        members: f.members.iter().map(ToString::to_string).collect(),
        retention_days: match f.retention {
            Retention::Days(days) => Some(days),
            Retention::Forever => None,
        },
        configured: f.configured,
    }
}

pub(crate) fn file(f: FileInfo) -> api::File {
    api::File {
        entry: f.entry.to_string(),
        path: f.path,
        size: f.size,
        modified_ms: f.modified_ms,
        executable: f.executable,
        on_disk: f.on_disk,
        conflicts: f.conflicts,
        author: f.author.to_string(),
    }
}

pub(crate) fn recycled(r: RecycledInfo) -> api::Recycled {
    api::Recycled {
        entry: r.entry.to_string(),
        path: r.path,
        size: r.size,
        deleted_at_ms: r.deleted_at_ms,
        expires_at_ms: r.expires_at_ms,
    }
}

pub(crate) fn peer(p: PeerInfo) -> api::Peer {
    api::Peer {
        device: p.device.to_string(),
        user: p.user.to_string(),
        name: p.name,
        shared_folders: p.shared_folders.iter().map(ToString::to_string).collect(),
    }
}

pub(crate) fn known_peer(p: PeerAddr) -> api::KnownPeer {
    api::KnownPeer {
        device: p.device.to_string(),
        addrs: p.addrs.iter().map(ToString::to_string).collect(),
        relay: p.relay.map(|r| r.to_string()),
    }
}

pub(crate) fn event(e: Event) -> api::Event {
    match e {
        Event::FolderAdded(f) => api::Event::FolderAdded {
            folder: f.to_string(),
        },
        Event::FolderRemoved(f) => api::Event::FolderRemoved {
            folder: f.to_string(),
        },
        Event::FolderChanged(f) => api::Event::FolderChanged {
            folder: f.to_string(),
        },
        Event::FileUpdated { folder, path } => api::Event::FileUpdated {
            folder: folder.to_string(),
            path,
        },
        Event::FileRemoved { folder, path } => api::Event::FileRemoved {
            folder: folder.to_string(),
            path,
        },
    }
}
