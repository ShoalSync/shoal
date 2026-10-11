//! `tidefell`: command-line control of a local Tidefell daemon.
//!
//! Talks to `tidefelld` over its control socket (`<data_dir>/control.sock`) through the
//! `tidefell-sdk` client only.

mod format;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use tidefell_sdk::types::{Direction, KnownPeer, LocalDeletes, NewFolder, SyncOptions};
use tidefell_sdk::Client;

#[derive(Parser)]
#[command(name = "tidefell", version, about = "Control a Tidefell peer")]
struct Cli {
    /// The daemon's data directory; its control socket is `<data_dir>/control.sock`.
    /// Defaults to $TIDEFELL_DATA_DIR, then ~/.local/share/tidefell.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Control socket path, overriding --data-dir.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this device and its folders' recycle bins.
    Status,
    /// Show where the daemon logs, or its recent warnings and errors.
    Logs {
        /// The most recent warnings and errors, oldest first.
        #[arg(long)]
        problems: bool,
    },
    /// List the crashes recorded on this device, or write one up to report it.
    Crashes {
        /// Mark them as seen, so `status` stops reporting them.
        #[arg(long)]
        dismiss: bool,
        /// Write a crash up as a new issue for the project's tracker: the newest one not
        /// dismissed, the newest one, or `ID`. Nothing is sent.
        #[arg(long, conflicts_with = "dismiss")]
        report: bool,
        /// With `--report`: the crash's ID, as `crashes --json` lists it.
        #[arg(requires = "report")]
        id: Option<String>,
    },
    /// Pause syncing of a folder, or of the whole device. Folders still scan; nothing is sent or
    /// received until you resume, across restarts.
    Pause {
        /// Folder ID; the whole device if omitted.
        folder: Option<String>,
    },
    /// Resume syncing of a folder, or of the whole device.
    Resume {
        /// Folder ID; the whole device if omitted.
        folder: Option<String>,
    },
    /// Show the upload and download limits, or change them until the daemon restarts.
    Limits {
        /// Most KiB/s to send; 0 for no limit.
        #[arg(long)]
        upload: Option<u64>,
        /// Most KiB/s to receive; 0 for no limit.
        #[arg(long)]
        download: Option<u64>,
        /// Also limit traffic to devices on the local network.
        #[arg(long)]
        limit_local: Option<bool>,
    },
    /// Show the daemon's log level, or change it until the daemon restarts.
    LogLevel {
        /// A level filter, such as `debug` or `info,tidefell_net=debug`.
        level: Option<String>,
    },
    /// Manage folders.
    #[command(subcommand)]
    Folder(FolderCommand),
    /// List the live files in a folder.
    Files {
        /// Folder ID.
        folder: String,
    },
    /// Make a file available on this device's disk, fetching it from peers if it is a
    /// placeholder, and print where it is.
    Fetch {
        /// Folder ID.
        folder: String,
        /// Path within the folder.
        path: String,
    },
    /// Remove a file from this device only. It moves to this device's recycle bin, stays on
    /// every other device, and is not downloaded again until fetched or restored.
    RemoveHere {
        /// Folder ID.
        folder: String,
        /// Path within the folder.
        path: String,
    },
    /// Rename or move a file, or a directory and everything in it, on every device: for
    /// example one whose name a device's drive cannot store.
    Rename {
        /// Folder ID.
        folder: String,
        /// Its path within the folder.
        from: String,
        /// Its new path within the folder.
        to: String,
    },
    /// Keep a file or directory on this partial replica: fetched now, never evicted, and not
    /// counted against the budget.
    Pin {
        /// Folder ID.
        folder: String,
        /// Path within the folder: a file, or a directory and everything below it.
        path: String,
    },
    /// Remove a pin; what it covered may then be evicted.
    Unpin {
        /// Folder ID.
        folder: String,
        /// The pinned path.
        path: String,
    },
    /// List the paths pinned on this device.
    Pins {
        /// Folder ID.
        folder: String,
    },
    /// Browse and restore deleted files.
    #[command(subcommand)]
    Recycle(RecycleCommand),
    /// List a file's versions: conflicting ones and previous ones kept on this device.
    History {
        /// Folder ID.
        folder: String,
        /// Path within the folder.
        path: String,
        /// Make this version (`previous:…`, from the list) the file's content again.
        #[arg(long, conflicts_with = "open")]
        restore: Option<String>,
        /// Write a read-only copy of this version (`previous:…` or `conflict:…`) and print
        /// where.
        #[arg(long)]
        open: Option<String>,
    },
    /// Check this device's clock, space, folders, watching and connectivity, with advice.
    Doctor,
    /// What this device applied to a folder, newest first: who changed which file, and whether
    /// the change came from another device.
    Activity {
        /// Folder ID.
        folder: String,
        /// Most changes to list.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Write a diagnostics bundle to attach to a report: a zip file describing this device,
    /// with paths and names hashed. Look inside before sending it.
    Diagnostics {
        /// Keep real paths and folder names.
        #[arg(long)]
        include_paths: bool,
        /// Where to write it; `tidefell-diagnostics-<time>.zip` here if left out.
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
    },
    /// List paths that failed to scan, fetch, write or delete, in one folder or all.
    Problems {
        /// Folder ID; all folders if left out.
        folder: Option<String>,
    },
    /// List files with conflicting versions waiting for a decision, in one folder or all.
    Conflicts {
        /// Folder ID; all folders if left out.
        folder: Option<String>,
    },
    /// Decide a conflict: keep one version, or keep both.
    Resolve {
        /// Folder ID.
        folder: String,
        /// Path within the folder.
        path: String,
        /// The version to keep (`current` or `conflict:…`, from `tidefell conflicts`). The
        /// others go to every device's recycle bin.
        #[arg(
            long,
            required_unless_present = "keep_both",
            conflicts_with = "keep_both"
        )]
        keep: Option<String>,
        /// Keep the current version as the file, and each other version as a file of its own
        /// beside it.
        #[arg(long)]
        keep_both: bool,
    },
    /// Add a new device to your user from the pairing code it shows. The daemon must hold
    /// your user key.
    Pair {
        /// The code the new device shows (TFP1-…).
        code: String,
        /// The new device's name; defaults to the name it suggests.
        #[arg(long)]
        name: Option<String>,
        /// Approve without asking; still prints the confirmation code.
        #[arg(long)]
        yes: bool,
    },
    /// Manage peers.
    #[command(subcommand)]
    Peer(PeerCommand),
    /// Follow changes as they happen.
    Events,
}

#[derive(Subcommand)]
enum FolderCommand {
    /// List folders.
    List,
    /// Create a new folder at PATH and print its ID; share the ID with your other devices.
    Create {
        /// Where the folder lives on this device.
        path: PathBuf,
        #[command(flatten)]
        options: FolderOptions,
    },
    /// Join a folder created on another device.
    Join {
        /// The folder ID.
        id: String,
        /// Where the folder lives on this device.
        path: PathBuf,
        #[command(flatten)]
        options: FolderOptions,
    },
    /// Stop syncing a folder. Files on disk are kept.
    Remove {
        /// Folder ID.
        id: String,
    },
    /// Let another user's devices sync a folder.
    Share {
        /// Folder ID.
        id: String,
        /// The user's ID.
        user: String,
    },
    /// Stop another user's devices syncing a folder.
    Unshare {
        /// Folder ID.
        id: String,
        /// The user's ID.
        user: String,
    },
    /// Scan a folder for local changes now.
    Scan {
        /// Folder ID.
        id: String,
    },
    /// Change how a folder syncs on this device. Other devices keep their own setting.
    Sync {
        /// Folder ID.
        id: String,
        /// Whether other devices' changes are written here.
        #[arg(long, value_enum)]
        direction: Option<DirectionArg>,
        /// What deleting a file here, outside Tidefell, does.
        #[arg(long, value_enum)]
        local_deletes: Option<LocalDeletesArg>,
    },
    /// Change how long this device keeps a folder's deleted files. Other devices keep their
    /// own setting.
    Retention {
        /// Folder ID.
        id: String,
        /// Days to keep deleted files, or "forever".
        #[arg(value_parser = parse_retention)]
        days: Retention,
        /// Change it without asking when deleted files would be purged.
        #[arg(long)]
        yes: bool,
    },
}

/// A retention period given on the command line.
#[derive(Clone, Copy)]
enum Retention {
    Days(u32),
    Forever,
}

fn parse_retention(value: &str) -> Result<Retention, String> {
    match value {
        "forever" | "0" => Ok(Retention::Forever),
        days => days
            .parse()
            .map(Retention::Days)
            .map_err(|_| format!("{days:?} is not a number of days or \"forever\"")),
    }
}

#[derive(clap::Args)]
struct FolderOptions {
    /// Users allowed to sync the folder (repeatable). Defaults to your user.
    #[arg(long = "member")]
    members: Vec<String>,
    /// Days to keep deleted files; 0 keeps them forever.
    #[arg(long, default_value_t = 30)]
    retention_days: u32,
    /// Keep only BYTES of other devices' files on disk: the rest are placeholders, fetched
    /// with `tidefell fetch` (a partial replica).
    #[arg(long, value_name = "BYTES")]
    partial: Option<u64>,
    /// Whether other devices' changes are written here.
    #[arg(long, value_enum, default_value_t = DirectionArg::Both)]
    direction: DirectionArg,
    /// What deleting a file here, outside Tidefell, does.
    #[arg(long, value_enum, default_value_t = LocalDeletesArg::Everywhere)]
    local_deletes: LocalDeletesArg,
}

/// `--direction` on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum DirectionArg {
    /// Send local changes and write other devices' changes here.
    Both,
    /// Send local changes only.
    SendOnly,
}

impl From<DirectionArg> for Direction {
    fn from(d: DirectionArg) -> Self {
        match d {
            DirectionArg::Both => Direction::Both,
            DirectionArg::SendOnly => Direction::SendOnly,
        }
    }
}

/// `--local-deletes` on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum LocalDeletesArg {
    /// Deleting a file here deletes it on every device.
    Everywhere,
    /// Deleting a file here drops it from this device only.
    ThisDevice,
}

impl From<LocalDeletesArg> for LocalDeletes {
    fn from(d: LocalDeletesArg) -> Self {
        match d {
            LocalDeletesArg::Everywhere => LocalDeletes::Everywhere,
            LocalDeletesArg::ThisDevice => LocalDeletes::ThisDevice,
        }
    }
}

#[derive(Subcommand)]
enum RecycleCommand {
    /// List deleted files, newest first.
    List {
        /// Folder ID.
        folder: String,
    },
    /// Restore a deleted file by entry ID or by path (the most recent deletion wins).
    Restore {
        /// Folder ID.
        folder: String,
        /// Entry ID or path.
        what: String,
    },
    /// Permanently delete files from this device's recycle bin: one, by entry ID or path, or
    /// all of them. Other devices keep their copies.
    Empty {
        /// Folder ID.
        folder: String,
        /// Entry ID or path; omit to empty the whole recycle bin.
        what: Option<String>,
        /// Delete without asking.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum PeerCommand {
    /// List connected peers and peers this device keeps connected.
    List,
    /// Keep a peer connected.
    Add {
        /// The peer's device ID.
        device: String,
        /// Direct addresses, e.g. 192.168.1.20:7777 (repeatable).
        #[arg(long = "addr")]
        addrs: Vec<String>,
        /// A relay the peer is reachable through, e.g. https://relay.example.org.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Stop keeping a peer connected.
    Remove {
        /// The peer's device ID.
        device: String,
    },
}

fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TIDEFELL_DATA_DIR") {
        return dir.into();
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/tidefell")
}

fn print<T: Serialize>(json: bool, value: &T, text: impl FnOnce(&T) -> String) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).expect("API types serialise")
        );
    } else {
        let text = text(value);
        if !text.is_empty() {
            println!("{text}");
        }
    }
}

/// Asks a yes/no question on the terminal; anything but yes is no.
fn confirm(question: &str) -> Result<bool, String> {
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Finds a recycle-bin item by entry ID, or by path (the most recent deletion wins).
fn find_recycled<'a>(
    bin: &'a [tidefell_sdk::types::Recycled],
    folder: &str,
    what: &str,
) -> Result<&'a tidefell_sdk::types::Recycled, String> {
    bin.iter()
        .find(|r| r.entry == what)
        .or_else(|| bin.iter().find(|r| r.path == what))
        .ok_or_else(|| format!("{what:?} is not in the recycle bin of {folder}"))
}

fn new_folder(
    id: Option<String>,
    path: PathBuf,
    options: FolderOptions,
) -> Result<NewFolder, String> {
    let path = std::path::absolute(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(NewFolder {
        id,
        path: path.display().to_string(),
        members: options.members,
        retention_days: options.retention_days,
        mode: options.partial.map(|_| tidefell_sdk::types::Mode::Partial),
        budget_bytes: options.partial,
        scan_interval_secs: None,
        sync: SyncOptions {
            direction: options.direction.into(),
            local_deletes: options.local_deletes.into(),
        },
    })
}

async fn run(cli: Cli) -> Result<(), String> {
    let socket = cli.socket.clone().unwrap_or_else(|| {
        Client::socket_path(&cli.data_dir.clone().unwrap_or_else(default_data_dir))
    });
    let client = Client::unix(&socket);
    let json = cli.json;
    let e = |e: tidefell_sdk::Error| e.to_string();
    match cli.command {
        Command::Status => {
            #[derive(Serialize)]
            struct StatusView {
                #[serde(flatten)]
                status: tidefell_sdk::types::Status,
                folders: Vec<tidefell_sdk::types::Folder>,
            }
            let view = StatusView {
                status: client.status().await.map_err(e)?,
                folders: client.folders().await.map_err(e)?,
            };
            print(json, &view, |v| format::status(&v.status, &v.folders));
        }
        Command::Logs { problems: true } => {
            print(json, &client.problems().await.map_err(e)?, |p| {
                format::problems(p)
            });
        }
        Command::Logs { problems: false } => {
            print(json, &client.log_settings().await.map_err(e)?, |l| {
                format::log_settings(l)
            });
        }
        Command::Crashes { dismiss: true, .. } => {
            client.dismiss_crashes().await.map_err(e)?;
        }
        Command::Crashes {
            report: true, id, ..
        } => {
            let id = match id {
                Some(id) => id,
                None => {
                    let crashes = client.crashes().await.map_err(e)?;
                    crashes
                        .iter()
                        .rfind(|c| !c.seen)
                        .or(crashes.last())
                        .ok_or("no crashes recorded")?
                        .id
                        .clone()
                }
            };
            let report = client.crash_report(&id).await.map_err(e)?;
            print(json, &report, format::crash_report);
        }
        Command::Crashes { .. } => {
            print(json, &client.crashes().await.map_err(e)?, |c| {
                format::crashes(c)
            });
        }
        Command::History {
            folder,
            path,
            open: Some(version),
            ..
        } => {
            let opened = client
                .open_version(&folder, &path, &version)
                .await
                .map_err(e)?;
            print(json, &opened, |o| o.file.clone());
        }
        Command::Diagnostics {
            include_paths,
            output,
        } => {
            let zip = client.diagnostics(include_paths).await.map_err(e)?;
            let output = output.unwrap_or_else(|| {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                std::path::PathBuf::from(format!("tidefell-diagnostics-{secs}.zip"))
            });
            std::fs::write(&output, &zip).map_err(|e| format!("{}: {e}", output.display()))?;
            let entries = format::zip_entries(&zip);
            print(json, &entries, |entries| {
                format!(
                    "wrote {}\n{}\npaths and names are {}; IDs are shortened and addresses \
                     reduced to their kind. Look inside before attaching it to a report.",
                    output.display(),
                    entries
                        .iter()
                        .map(|(name, size)| format!("  {name}  {}", format::size(*size)))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    if include_paths { "included" } else { "hashed" }
                )
            });
        }
        Command::Activity { folder, limit } => {
            let activity = client.activity(&folder, limit).await.map_err(e)?;
            print(json, &activity, |a| format::activity(a));
        }
        Command::Doctor => {
            let checks = client.doctor().await.map_err(e)?;
            print(json, &checks, |c| format::checks(c));
        }
        Command::Problems { folder } => {
            let folders = match folder {
                Some(folder) => vec![folder],
                None => client
                    .folders()
                    .await
                    .map_err(e)?
                    .into_iter()
                    .map(|f| f.id)
                    .collect(),
            };
            let mut all = Vec::new();
            for folder in folders {
                let problems = client.folder_problems(&folder).await.map_err(e)?;
                all.push((folder, problems));
            }
            print(json, &all, |all| {
                all.iter()
                    .map(|(folder, problems)| {
                        format!("{folder}\n{}", format::path_problems(problems))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            });
        }
        Command::Conflicts { folder } => {
            let folders = match folder {
                Some(folder) => vec![folder],
                None => client
                    .folders()
                    .await
                    .map_err(e)?
                    .into_iter()
                    .map(|f| f.id)
                    .collect(),
            };
            let mut conflicts = Vec::new();
            for folder in folders {
                for file in client.files(&folder).await.map_err(e)? {
                    if file.conflicts > 0 {
                        let versions = client.versions(&folder, &file.path).await.map_err(e)?;
                        conflicts.push(format::Conflict {
                            folder: folder.clone(),
                            path: file.path,
                            versions,
                        });
                    }
                }
            }
            print(json, &conflicts, |c| format::conflicts(c));
        }
        Command::Resolve {
            folder,
            path,
            keep,
            keep_both,
        } => {
            let keep = if keep_both {
                "both".to_owned()
            } else {
                keep.unwrap_or_default()
            };
            client.resolve(&folder, &path, &keep).await.map_err(e)?;
        }
        Command::History {
            folder,
            path,
            restore: Some(version),
            ..
        } => {
            client
                .restore_version(&folder, &path, &version)
                .await
                .map_err(e)?;
        }
        Command::History {
            folder,
            path,
            restore: None,
            open: None,
        } => {
            print(
                json,
                &client.versions(&folder, &path).await.map_err(e)?,
                |v| format::versions(v),
            );
        }
        Command::Pause { folder } => client.pause(folder.as_deref()).await.map_err(e)?,
        Command::Resume { folder } => client.resume(folder.as_deref()).await.map_err(e)?,
        Command::Limits {
            upload,
            download,
            limit_local,
        } => {
            let mut limits = client.limits().await.map_err(e)?;
            let rate = |kib: u64| (kib > 0).then_some(kib);
            if upload.is_some() || download.is_some() || limit_local.is_some() {
                if let Some(kib) = upload {
                    limits.upload_kib_per_sec = rate(kib);
                }
                if let Some(kib) = download {
                    limits.download_kib_per_sec = rate(kib);
                }
                if let Some(on) = limit_local {
                    limits.limit_local = on;
                }
                limits = client.set_limits(&limits).await.map_err(e)?;
            }
            print(json, &limits, format::limits);
        }
        Command::LogLevel { level } => {
            let settings = match level {
                Some(level) => client.set_log_level(&level).await,
                None => client.log_settings().await,
            }
            .map_err(e)?;
            print(json, &settings, |l| l.level.clone());
        }
        Command::Folder(command) => match command {
            FolderCommand::List => print(json, &client.folders().await.map_err(e)?, |f| {
                format::folders(f)
            }),
            FolderCommand::Create { path, options } => {
                let folder = client
                    .add_folder(&new_folder(None, path, options)?)
                    .await
                    .map_err(e)?;
                print(json, &folder, format::created);
            }
            FolderCommand::Join { id, path, options } => {
                let folder = client
                    .add_folder(&new_folder(Some(id), path, options)?)
                    .await
                    .map_err(e)?;
                print(json, &folder, format::joined);
            }
            FolderCommand::Remove { id } => {
                client.remove_folder(&id).await.map_err(e)?;
                print(json, &(), |_| {
                    format!("stopped syncing {id}; files were kept")
                });
            }
            FolderCommand::Share { id, user } => {
                let folder = client.add_member(&id, &user).await.map_err(e)?;
                print(json, &folder, |_| format!("{user} can now sync {id}"));
            }
            FolderCommand::Unshare { id, user } => {
                let folder = client.remove_member(&id, &user).await.map_err(e)?;
                print(json, &folder, |_| format!("{user} can no longer sync {id}"));
            }
            FolderCommand::Scan { id } => {
                client.scan(&id).await.map_err(e)?;
                print(json, &(), |_| String::new());
            }
            FolderCommand::Sync {
                id,
                direction,
                local_deletes,
            } => {
                let current = client
                    .folders()
                    .await
                    .map_err(e)?
                    .into_iter()
                    .find(|f| f.id == id)
                    .ok_or_else(|| format!("no folder {id}"))?;
                let sync = SyncOptions {
                    direction: direction.map_or(current.sync.direction, Into::into),
                    local_deletes: local_deletes.map_or(current.sync.local_deletes, Into::into),
                };
                let folder = client.set_sync(&id, sync).await.map_err(e)?;
                print(json, &folder, |f| format!("{} {}", f.id, format::sync(f)));
            }
            FolderCommand::Retention { id, days, yes } => {
                let retention_days = match days {
                    Retention::Days(days) => Some(days),
                    Retention::Forever => None,
                };
                if let Some(days) = retention_days {
                    let cutoff = now_ms() - i64::from(days) * 86_400_000;
                    let bin = client.recycle_bin(&id).await.map_err(e)?;
                    let expiring = bin.iter().filter(|r| r.deleted_at_ms <= cutoff).count();
                    if expiring > 0 {
                        eprintln!(
                            "{expiring} deleted file(s) are older than {days} days and will be \
                             purged from this device within the hour."
                        );
                        if !yes && !confirm("Change retention?")? {
                            return Err("retention not changed".into());
                        }
                    }
                }
                let folder = client.set_retention(&id, retention_days).await.map_err(e)?;
                print(json, &folder, |f| {
                    format!("{} now keeps deleted files {}", f.id, format::retention(f))
                });
            }
        },
        Command::Files { folder } => print(json, &client.files(&folder).await.map_err(e)?, |f| {
            format::files(f)
        }),
        Command::Fetch { folder, path } => {
            print(json, &client.fetch(&folder, &path).await.map_err(e)?, |f| {
                f.local_path.clone()
            })
        }
        Command::Rename { folder, from, to } => {
            client.rename(&folder, &from, &to).await.map_err(e)?;
            print(json, &(), |_| {
                format!("renamed {from} to {to} on every device")
            });
        }
        Command::Pin { folder, path } => {
            client.pin(&folder, &path).await.map_err(e)?;
            print(json, &(), |_| {
                format!("pinned {path}; it is fetched and kept on this device")
            });
        }
        Command::Unpin { folder, path } => {
            client.unpin(&folder, &path).await.map_err(e)?;
        }
        Command::Pins { folder } => {
            print(json, &client.pins(&folder).await.map_err(e)?, |p| {
                if p.is_empty() {
                    "nothing pinned".into()
                } else {
                    p.join("\n")
                }
            });
        }
        Command::RemoveHere { folder, path } => {
            client.remove_here(&folder, &path).await.map_err(e)?;
            print(json, &(), |_| {
                format!(
                    "removed {path} from this device; restore it with `tidefell recycle restore`"
                )
            });
        }
        Command::Pair { code, name, yes } => {
            let pairing = client.pair(&code).await.map_err(e)?;
            let name = name.unwrap_or_else(|| pairing.suggested_name.clone());
            eprintln!(
                "Pairing with \"{}\" (device {}).\nConfirmation code: {} {}",
                pairing.suggested_name,
                pairing.device,
                &pairing.confirmation_code[..3],
                &pairing.confirmation_code[3..],
            );
            let approved = yes
                || confirm(&format!(
                    "Does the new device show the same code? Add it as \"{name}\"?"
                ))?;
            if approved {
                client
                    .approve_pairing(&pairing.id, Some(&name))
                    .await
                    .map_err(e)?;
                print(json, &pairing, |_| format!("added {name} to your devices"));
            } else {
                client.decline_pairing(&pairing.id).await.map_err(e)?;
                return Err("pairing declined".into());
            }
        }
        Command::Recycle(command) => match command {
            RecycleCommand::List { folder } => {
                print(json, &client.recycle_bin(&folder).await.map_err(e)?, |r| {
                    format::recycled(r)
                });
            }
            RecycleCommand::Restore { folder, what } => {
                let bin = client.recycle_bin(&folder).await.map_err(e)?;
                let item = find_recycled(&bin, &folder, &what)?;
                client.restore(&folder, &item.entry).await.map_err(e)?;
                print(json, item, |r| format!("restored {}", r.path));
            }
            RecycleCommand::Empty { folder, what, yes } => {
                let bin = client.recycle_bin(&folder).await.map_err(e)?;
                let removed = match what {
                    Some(what) => {
                        let item = find_recycled(&bin, &folder, &what)?;
                        if !yes
                            && !confirm(&format!(
                                "Permanently delete {} from this device's recycle bin?",
                                item.path
                            ))?
                        {
                            return Err("nothing deleted".into());
                        }
                        vec![client.discard(&folder, &item.entry).await.map_err(e)?]
                    }
                    None if bin.is_empty() => Vec::new(),
                    None => {
                        let bytes = bin.iter().map(|r| r.size).sum();
                        if !yes
                            && !confirm(&format!(
                                "Permanently delete {} file(s), {}, from this device's recycle \
                                 bin?",
                                bin.len(),
                                format::size(bytes)
                            ))?
                        {
                            return Err("nothing deleted".into());
                        }
                        client.empty_recycle_bin(&folder).await.map_err(e)?
                    }
                };
                print(json, &removed, |r| format::emptied(r));
            }
        },
        Command::Peer(command) => match command {
            PeerCommand::List => {
                let connected = client.peers().await.map_err(e)?;
                let known = client.known_peers().await.map_err(e)?;
                #[derive(Serialize)]
                struct Peers {
                    connected: Vec<tidefell_sdk::types::Peer>,
                    known: Vec<KnownPeer>,
                }
                print(json, &Peers { connected, known }, |p| {
                    format::peers(&p.connected, &p.known)
                });
            }
            PeerCommand::Add {
                device,
                addrs,
                relay,
            } => {
                client
                    .add_peer(&KnownPeer {
                        device: device.clone(),
                        addrs,
                        relay,
                    })
                    .await
                    .map_err(e)?;
                print(json, &(), |_| format!("keeping {device} connected"));
            }
            PeerCommand::Remove { device } => {
                client.remove_peer(&device).await.map_err(e)?;
                print(json, &(), |_| format!("forgot {device}"));
            }
        },
        Command::Events => {
            client
                .events(|event| {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&event).expect("API types serialise")
                        );
                    } else {
                        println!("{}", format::event(&event));
                    }
                    true
                })
                .await
                .map_err(e)?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tidefell: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tidefell: {e}");
            ExitCode::FAILURE
        }
    }
}
