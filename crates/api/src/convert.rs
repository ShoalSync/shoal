//! Conversions between engine views and API types, and parsing of identifiers.

use std::str::FromStr;

use tidefell_engine::{
    CopyInfo, Direction, Event, FileInfo, FolderInfo, LocalDeletes, PeerAddr, PeerInfo,
    RecycledInfo, ReplicaMode, SyncOptions,
};
use tidefell_sdk::types as api;

use crate::handlers::ApiFailure;

pub(crate) fn parse<T: FromStr>(what: &str, value: &str) -> Result<T, ApiFailure> {
    value.parse().map_err(|_| {
        ApiFailure::bad_request(format!("{what} {value:?} is not 64 lowercase hex digits"))
    })
}

pub(crate) fn folder(f: FolderInfo) -> api::Folder {
    let mode = match f.mode {
        ReplicaMode::Full => api::Mode::Full,
        ReplicaMode::Partial { .. } => api::Mode::Partial,
    };
    api::Folder {
        id: f.id.to_string(),
        path: f.path.display().to_string(),
        mode,
        budget_bytes: f.mode.budget_bytes(),
        members: f.members.iter().map(ToString::to_string).collect(),
        retention_days: f.retention.days(),
        sync: sync_options(f.sync),
        recycle_bin: api::RecycleBinSummary {
            items: f.recycle_bin.items as u64,
            bytes: f.recycle_bin.bytes,
            next_expiry_ms: f.recycle_bin.next_expiry_ms,
        },
        configured: f.configured,
        only_here: f.only_here as u64,
        watch_error: f.watch_error,
    }
}

pub(crate) fn sync_options(s: SyncOptions) -> api::SyncOptions {
    api::SyncOptions {
        direction: match s.direction {
            Direction::Both => api::Direction::Both,
            Direction::SendOnly => api::Direction::SendOnly,
        },
        local_deletes: match s.local_deletes {
            LocalDeletes::Everywhere => api::LocalDeletes::Everywhere,
            LocalDeletes::ThisDevice => api::LocalDeletes::ThisDevice,
        },
    }
}

pub(crate) fn engine_sync_options(s: api::SyncOptions) -> SyncOptions {
    SyncOptions {
        direction: match s.direction {
            api::Direction::Both => Direction::Both,
            api::Direction::SendOnly => Direction::SendOnly,
        },
        local_deletes: match s.local_deletes {
            api::LocalDeletes::Everywhere => LocalDeletes::Everywhere,
            api::LocalDeletes::ThisDevice => LocalDeletes::ThisDevice,
        },
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
        confirmed_copies: f.confirmed_copies as u64,
        only_here: f.only_here,
    }
}

pub(crate) fn copy(c: CopyInfo) -> api::Copy {
    api::Copy {
        device: c.device.to_string(),
        name: c.name,
        confirmed_ms: c.confirmed_ms,
    }
}

pub(crate) fn recycled(r: RecycledInfo) -> api::Recycled {
    api::Recycled {
        entry: r.entry.to_string(),
        path: r.path,
        size: r.size,
        deleted_at_ms: r.deleted_at_ms,
        expires_at_ms: r.expires_at_ms,
        here_only: r.here_only,
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
        Event::Crashed(message) => api::Event::Crashed { message },
    }
}
