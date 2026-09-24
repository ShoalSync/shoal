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
}

#[derive(Template)]
#[template(path = "folders.html")]
pub(crate) struct Folders {
    pub(crate) username: String,
    pub(crate) admin: bool,
    pub(crate) csrf: String,
    pub(crate) share_links: bool,
    pub(crate) folders: Vec<FolderRow>,
}

pub(crate) struct FileRow {
    pub(crate) path: String,
    pub(crate) path_query: String,
    pub(crate) size: String,
    pub(crate) modified: String,
    pub(crate) on_disk: bool,
    pub(crate) conflicts: usize,
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
    pub(crate) notice: Option<String>,
    pub(crate) files: Vec<FileRow>,
}

pub(crate) struct RecycledRow {
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
    pub(crate) downloads: String,
    pub(crate) password: bool,
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

#[derive(Template)]
#[template(path = "link_password.html")]
pub(crate) struct LinkPassword {
    pub(crate) token: String,
    pub(crate) error: Option<String>,
}
