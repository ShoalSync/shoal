//! Page templates.

use askama::Template;

#[derive(Template)]
#[template(path = "login.html")]
pub(crate) struct Login {
    pub(crate) error: Option<String>,
}

pub(crate) struct FolderRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) files: usize,
    pub(crate) recycled: usize,
    /// What the folder is doing, such as "syncing: 3 files, 2.0 MiB to fetch" (ADR 0030).
    pub(crate) state: String,
    /// Paths that failed.
    pub(crate) problems: usize,
}

#[derive(Template)]
#[template(path = "folders.html")]
pub(crate) struct Folders {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) share_links: bool,
    pub(crate) paused: bool,
    /// For administrators: the server crashed and nobody dismissed it yet (ADR 0030).
    pub(crate) crashed: bool,
    pub(crate) folders: Vec<FolderRow>,
}

pub(crate) struct FileRow {
    pub(crate) path: String,
    pub(crate) path_query: String,
    pub(crate) size: String,
    pub(crate) modified: String,
    pub(crate) on_disk: bool,
    pub(crate) conflicts: usize,
    pub(crate) only_here: bool,
    pub(crate) pinned: bool,
}

#[derive(Template)]
#[template(path = "folder.html")]
pub(crate) struct Folder {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) recycled: usize,
    pub(crate) share_links: bool,
    /// Syncing of this folder is paused; `device_paused` if the whole server is.
    pub(crate) paused: bool,
    pub(crate) device_paused: bool,
    /// A partial replica here, which can pin files (ADR 0029); their total size.
    pub(crate) partial: bool,
    pub(crate) pinned_size: String,
    /// Files with conflicting versions waiting for a decision (ADR 0029).
    pub(crate) conflicts: usize,
    /// What the folder is doing and its last scan (ADR 0030).
    pub(crate) state: String,
    /// Paths that failed, until they succeed.
    pub(crate) problems: Vec<ProblemRow>,
    pub(crate) notice: Option<String>,
    pub(crate) files: Vec<FileRow>,
}

pub(crate) struct ProblemRow {
    pub(crate) path: String,
    /// Such as "write: permission denied".
    pub(crate) what: String,
    pub(crate) message: String,
    pub(crate) since: String,
    pub(crate) attempts: u32,
}

pub(crate) struct PeerRow {
    pub(crate) name: String,
    pub(crate) device: String,
    pub(crate) connected: bool,
    /// Such as "connected direct, 12 ms".
    pub(crate) connection: String,
    /// When the session started, or when the device was last seen.
    pub(crate) since: String,
    pub(crate) error: Option<String>,
}

pub(crate) struct CrashNotice {
    pub(crate) time: String,
    pub(crate) message: String,
    pub(crate) file: String,
}

/// This server's status, for administrators (ADR 0030).
#[derive(Template)]
#[template(path = "status.html")]
pub(crate) struct Status {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) device: String,
    pub(crate) peers: Vec<PeerRow>,
    pub(crate) sent: String,
    pub(crate) received: String,
    pub(crate) send_rate: String,
    pub(crate) receive_rate: String,
    /// The checks of `tidefell doctor`, when asked for.
    pub(crate) checks: Option<Vec<CheckRow>>,
    pub(crate) crash: Option<CrashNotice>,
}

pub(crate) struct CheckRow {
    /// `pass`, `warning` or `failure`.
    pub(crate) level: &'static str,
    pub(crate) detail: String,
    pub(crate) advice: Option<String>,
}

pub(crate) struct RecycledRow {
    /// A version that lost a conflict, rather than a deleted file (ADR 0029).
    pub(crate) conflict: bool,
    pub(crate) entry: String,
    pub(crate) path: String,
    pub(crate) size: String,
    pub(crate) deleted: String,
    pub(crate) expires: String,
}

#[derive(Template)]
#[template(path = "recycle.html")]
pub(crate) struct Recycle {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) notice: Option<String>,
    pub(crate) items: Vec<RecycledRow>,
}

pub(crate) struct ConflictRow {
    pub(crate) current: bool,
    pub(crate) id: String,
    pub(crate) version_query: String,
    pub(crate) device: String,
    pub(crate) modified: String,
    pub(crate) size: String,
}

/// A file's conflicting versions (ADR 0029).
#[derive(Template)]
#[template(path = "conflict.html")]
pub(crate) struct Conflict {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) path_query: String,
    pub(crate) versions: Vec<ConflictRow>,
}

pub(crate) struct AccountRow {
    pub(crate) username: String,
    pub(crate) user: String,
    pub(crate) admin: bool,
}

#[derive(Template)]
#[template(path = "accounts.html")]
pub(crate) struct Accounts {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) notice: Option<String>,
    pub(crate) accounts: Vec<AccountRow>,
}

#[derive(Template)]
#[template(path = "error.html")]
pub(crate) struct Error {
    pub(crate) title: String,
    pub(crate) message: String,
}

#[derive(Template)]
#[template(path = "share_new.html")]
pub(crate) struct ShareNew {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) directory: bool,
    /// True if the new link is upload-only.
    pub(crate) upload_only: bool,
    pub(crate) error: Option<String>,
    /// The new link's URL, shown once.
    pub(crate) created: Option<String>,
}

pub(crate) struct ShareRow {
    pub(crate) id: String,
    pub(crate) folder: String,
    pub(crate) path: String,
    pub(crate) created_by: String,
    pub(crate) created: String,
    pub(crate) expires: String,
    /// Downloads so far, or for an upload-only link, files and bytes uploaded.
    pub(crate) downloads: String,
    pub(crate) password: bool,
    pub(crate) upload_only: bool,
}

#[derive(Template)]
#[template(path = "shares.html")]
pub(crate) struct Shares {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) notice: Option<String>,
    pub(crate) shares: Vec<ShareRow>,
}

pub(crate) struct LinkFile {
    pub(crate) name: String,
    pub(crate) query: String,
    pub(crate) size: String,
}

#[derive(Template)]
#[template(path = "link.html")]
pub(crate) struct Link {
    pub(crate) token: String,
    pub(crate) title: String,
    pub(crate) directory: bool,
    pub(crate) exhausted: bool,
    pub(crate) expires: Option<String>,
    pub(crate) files: Vec<LinkFile>,
}

/// What a visitor to an upload-only link sees: a form, and none of the directory's contents.
#[derive(Template)]
#[template(path = "link_upload.html")]
pub(crate) struct LinkUpload {
    pub(crate) token: String,
    pub(crate) title: String,
    pub(crate) exhausted: bool,
    pub(crate) expires: Option<String>,
    pub(crate) files_left: Option<u32>,
    pub(crate) bytes_left: Option<String>,
    /// Files the last form uploaded.
    pub(crate) uploaded: Option<u32>,
}

#[derive(Template)]
#[template(path = "link_password.html")]
pub(crate) struct LinkPassword {
    pub(crate) token: String,
    pub(crate) error: Option<String>,
}

pub(crate) struct VersionRow {
    pub(crate) id: String,
    pub(crate) current: bool,
    pub(crate) size: String,
    pub(crate) modified: String,
    pub(crate) kept: String,
    pub(crate) expires: String,
}

#[derive(Template)]
#[template(path = "history.html")]
pub(crate) struct History {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) notice: Option<String>,
    pub(crate) versions: Vec<VersionRow>,
}
