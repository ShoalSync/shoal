//! Starting and supervising `tidefelld` (ADR 0042): the tray is what starts it again when it
//! stops, as a service manager would.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use tidefell_sdk::types::{Folder, Status};
use tidefell_sdk::Client;
use tokio::sync::mpsc;

use crate::view::Daemon;

/// The environment `tidefelld` runs best with: its allocator reads it as the process loads
/// (ADR 0035). The same as the engine's `ALLOCATOR_SETTINGS`, which a test checks, without
/// linking the engine into the tray.
pub const ALLOCATOR_SETTINGS: [(&str, &str); 2] = [
    ("MIMALLOC_PURGE_DELAY", "0"),
    ("MIMALLOC_ARENA_EAGER_COMMIT", "0"),
];

/// How often the daemon is asked how it is.
const POLL: Duration = Duration::from_secs(2);

/// How long a request may take before the daemon counts as not answering.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Failures within this long of each other count together.
const FAILURE_WINDOW: Duration = Duration::from_secs(120);

/// After this many failures in [`FAILURE_WINDOW`], the daemon is not started again until asked.
const MAX_FAILURES: usize = 5;

/// How long a stop may take before the tray gives up waiting.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the daemon and its files are.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `tidefelld` itself.
    pub daemon: PathBuf,
    /// Its data directory.
    pub data_dir: PathBuf,
    /// Its configuration file.
    pub config: PathBuf,
}

impl Paths {
    /// The installed layout: `tidefelld` beside the tray, the data directory where the CLI
    /// looks for it (`$TIDEFELL_DATA_DIR`, else `%LOCALAPPDATA%\Tidefell`), and the
    /// configuration file in it.
    pub fn installed() -> Self {
        let data_dir = std::env::var_os("TIDEFELL_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join("Tidefell")
            });
        let daemon = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned))
            .unwrap_or_default()
            .join(format!("tidefelld{}", std::env::consts::EXE_SUFFIX));
        Self {
            daemon,
            config: data_dir.join("tidefelld.toml"),
            data_dir,
        }
    }

    /// Where the daemon's standard error goes: what it says before its logs start, as when its
    /// configuration is not valid.
    pub fn stderr(&self) -> PathBuf {
        self.data_dir.join("tidefelld.stderr.log")
    }
}

/// What the tray asks of the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Pause syncing on the whole device.
    Pause,
    /// Resume it.
    Resume,
    /// Start the daemon again after it failed too often.
    Restart,
    /// Stop the daemon and the supervisor.
    Quit,
}

/// What the supervisor last saw.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Where the daemon is.
    pub daemon: Daemon,
    /// Its status, while it answers.
    pub status: Option<Status>,
    /// Its folders, while it answers.
    pub folders: Vec<Folder>,
    /// Why the last thing asked of it failed, if it did, once.
    pub error: Option<String>,
}

/// Keeps `tidefelld` running until told to quit: one already answering on the control pipe,
/// as one started by hand, is used as it is; otherwise one is started with
/// [`ALLOCATOR_SETTINGS`] in its environment, and started again when it stops, waiting longer
/// each time, until it has failed [`MAX_FAILURES`] times in two minutes. `publish` is given
/// each [`Snapshot`]. Quitting stops the daemon through its API, so it disconnects its sync
/// roots before it exits, never by killing it.
pub async fn supervise(
    paths: Paths,
    mut commands: mpsc::Receiver<Command>,
    publish: impl Fn(Snapshot),
) {
    let client = Client::local(Client::socket_path(&paths.data_dir));
    let mut child: Option<Child> = None;
    let mut failures: VecDeque<Instant> = VecDeque::new();
    let mut failed: Option<String> = None;
    let mut next_start = Instant::now();
    let mut error = None;
    loop {
        // A child that exited, unasked, failed.
        if let Some(exit) = child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            child = None;
            let now = Instant::now();
            failures.push_back(now);
            while failures
                .front()
                .is_some_and(|t| now.duration_since(*t) > FAILURE_WINDOW)
            {
                failures.pop_front();
            }
            let reason =
                last_line(&paths.stderr()).unwrap_or_else(|| format!("it exited ({exit})"));
            if failures.len() >= MAX_FAILURES {
                failed = Some(reason);
            } else {
                next_start = now + Duration::from_secs(1 << failures.len().min(5));
            }
        }
        let status = request(client.status()).await;
        let daemon = match (&status, &failed) {
            (Some(_), _) => {
                failed = None;
                Daemon::Running
            }
            (None, Some(reason)) => Daemon::Failed(reason.clone()),
            (None, None) if !paths.config.exists() => Daemon::NotSetUp,
            (None, None) => {
                if child.is_none() && Instant::now() >= next_start {
                    match start(&paths) {
                        Ok(started) => child = Some(started),
                        Err(e) => failed = Some(e),
                    }
                }
                match &failed {
                    Some(reason) => Daemon::Failed(reason.clone()),
                    None => Daemon::Starting,
                }
            }
        };
        let folders = match status {
            Some(_) => request(client.folders()).await.unwrap_or_default(),
            None => Vec::new(),
        };
        publish(Snapshot {
            daemon,
            status,
            folders,
            error: error.take(),
        });

        let command = tokio::select! {
            command = commands.recv() => command.unwrap_or(Command::Quit),
            () = tokio::time::sleep(POLL) => continue,
        };
        error = match command {
            Command::Pause => ask(client.pause(None)).await,
            Command::Resume => ask(client.resume(None)).await,
            Command::Restart => {
                failed = None;
                failures.clear();
                next_start = Instant::now();
                None
            }
            Command::Quit => {
                publish(Snapshot {
                    daemon: Daemon::Stopping,
                    status: None,
                    folders: Vec::new(),
                    error: None,
                });
                stop(&client, child).await;
                return;
            }
        };
    }
}

/// `call`'s result, or `None` if it failed or took too long.
async fn request<T, E: std::fmt::Display>(
    call: impl std::future::Future<Output = Result<T, E>>,
) -> Option<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, call)
        .await
        .ok()
        .and_then(Result::ok)
}

/// Why `call` failed, if it did or took too long.
async fn ask<T, E: std::fmt::Display>(
    call: impl std::future::Future<Output = Result<T, E>>,
) -> Option<String> {
    match tokio::time::timeout(REQUEST_TIMEOUT, call).await {
        Err(_) => Some("Tidefell did not answer".to_owned()),
        Ok(Err(e)) => Some(e.to_string()),
        Ok(Ok(_)) => None,
    }
}

/// Starts the daemon, its standard error to [`Paths::stderr`].
fn start(paths: &Paths) -> Result<Child, String> {
    fs::create_dir_all(&paths.data_dir)
        .map_err(|e| format!("cannot make {}: {e}", paths.data_dir.display()))?;
    let stderr = fs::File::create(paths.stderr())
        .map_err(|e| format!("cannot write {}: {e}", paths.stderr().display()))?;
    let mut command = Process::new(&paths.daemon);
    command
        .arg("run")
        .arg("--config")
        .arg(&paths.config)
        .envs(ALLOCATOR_SETTINGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window of its own.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", paths.daemon.display()))
}

/// Asks the daemon to stop and waits for it to, the child if the tray started it, or until it
/// no longer answers.
async fn stop(client: &Client, mut child: Option<Child>) {
    let asked = request(client.shutdown()).await.is_some();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        let gone = match child.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_some(),
            None => !asked || request(client.status()).await.is_none(),
        };
        if gone {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What the daemon said as it failed, in `path`: the error `main` prints, else its last line.
fn last_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let line = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("tidefelld: "))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))?
        .trim();
    Some(line.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allocator_settings_are_the_engines() {
        assert_eq!(ALLOCATOR_SETTINGS, tidefell_engine::ALLOCATOR_SETTINGS);
    }

    #[test]
    fn the_reason_is_the_last_thing_said() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stderr");
        assert_eq!(last_line(&path), None);
        fs::write(&path, "starting\nsomething went wrong\n\n").unwrap();
        assert_eq!(last_line(&path).as_deref(), Some("something went wrong"));
        fs::write(
            &path,
            "tidefelld: config.toml: TOML parse error\n  |\n1 | x\n",
        )
        .unwrap();
        assert_eq!(
            last_line(&path).as_deref(),
            Some("config.toml: TOML parse error")
        );
    }
}
