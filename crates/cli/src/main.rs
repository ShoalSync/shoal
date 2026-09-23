//! `tidefell`: command-line control of a local Tidefell daemon.
//!
//! Talks to `tidefelld` over its control socket (`<data_dir>/control.sock`) through the
//! `tidefell-sdk` client only.

mod format;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use tidefell_sdk::types::{KnownPeer, NewFolder};
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
    /// Show this device.
    Status,
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
    /// Browse and restore deleted files.
    #[command(subcommand)]
    Recycle(RecycleCommand),
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
        Command::Status => print(json, &client.status().await.map_err(e)?, format::status),
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
        },
        Command::Files { folder } => print(json, &client.files(&folder).await.map_err(e)?, |f| {
            format::files(f)
        }),
        Command::Fetch { folder, path } => {
            print(json, &client.fetch(&folder, &path).await.map_err(e)?, |f| {
                f.local_path.clone()
            })
        }
        Command::Recycle(command) => match command {
            RecycleCommand::List { folder } => {
                print(json, &client.recycle_bin(&folder).await.map_err(e)?, |r| {
                    format::recycled(r)
                });
            }
            RecycleCommand::Restore { folder, what } => {
                let bin = client.recycle_bin(&folder).await.map_err(e)?;
                let item = bin
                    .iter()
                    .find(|r| r.entry == what)
                    .or_else(|| bin.iter().find(|r| r.path == what))
                    .ok_or_else(|| format!("{what:?} is not in the recycle bin of {folder}"))?;
                client.restore(&folder, &item.entry).await.map_err(e)?;
                print(json, item, |r| format!("restored {}", r.path));
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
            PeerCommand::Add { device, addrs } => {
                client
                    .add_peer(&KnownPeer {
                        device: device.clone(),
                        addrs,
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
