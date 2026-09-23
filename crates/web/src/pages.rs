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
