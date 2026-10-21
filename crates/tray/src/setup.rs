//! Setting up this device, before the daemon can run: as the first of your devices, with a new
//! user key, or as a new one joining them by pairing (ADR 0015). Both run `tidefelld`, which
//! makes the data directory, and then write the configuration.

use std::fs;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};

use crate::supervisor::Paths;

/// The configuration a device set up from the tray starts with: its data directory, and the
/// defaults for the rest. Folders are added with the CLI or the web peer.
pub fn config_text(paths: &Paths) -> String {
    // A TOML literal string keeps a Windows path's backslashes.
    format!(
        "# Written by the Tidefell tray app. See the configuration guide for the rest.\n\
         data_dir = '{}'\n",
        paths.data_dir.display()
    )
}

fn write_config(paths: &Paths) -> Result<(), String> {
    fs::write(&paths.config, config_text(paths))
        .map_err(|e| format!("cannot write {}: {e}", paths.config.display()))
}

#[cfg_attr(not(windows), allow(unused_mut))]
fn command(paths: &Paths) -> Command {
    let mut command = Command::new(&paths.daemon);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// The name this device suggests for itself: the computer's.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Windows".to_owned())
}

/// Sets this device up as the first of your devices: a new device key, a new user key in the
/// data directory (`user.key`, to back up), and a certificate from it.
pub fn first_device(paths: &Paths, name: &str) -> Result<(), String> {
    // `init` would certify the device again; once is setting it up.
    if paths.data_dir.join("device.cert").exists() {
        return Err(format!(
            "{} already has a certificate; this device is set up",
            paths.data_dir.display()
        ));
    }
    let out = command(paths)
        .args(["init", "--name", name, "--data-dir"])
        .arg(&paths.data_dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start {}: {e}", paths.daemon.display()))?;
    if !out.status.success() {
        return Err(said(&out.stderr));
    }
    write_config(paths)
}

/// What setting up by pairing shows, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Joining {
    /// The code to give a device that holds your user key.
    Code(String),
    /// That device answered: it should show this confirmation code too.
    Confirm(String),
    /// Paired, and set up.
    Joined,
    /// Not paired, and why.
    Failed(String),
}

/// One line `tidefelld pair` prints, as it matters here.
pub fn parse_line(line: &str) -> Option<Joining> {
    let (name, value) = line.split_once(':')?;
    let value = value.trim().to_owned();
    match name {
        "code" => Some(Joining::Code(value)),
        "confirmation" => Some(Joining::Confirm(value)),
        _ => None,
    }
}

/// Setting this device up by pairing: `tidefelld pair` running, waiting for a device with your
/// user key to approve it. Dropping it stops waiting.
pub struct Join {
    child: Child,
}

impl Join {
    /// Starts pairing as `name`; `events` is given what to show, from a thread of its own,
    /// ending with [`Joining::Joined`] or [`Joining::Failed`].
    pub fn start(
        paths: &Paths,
        name: &str,
        events: impl Fn(Joining) + Send + 'static,
    ) -> Result<Self, String> {
        Self::start_with(paths, name, &[], events)
    }

    /// [`Join::start`], passing `tidefelld pair` the further `args`, as `--no-relay`.
    pub fn start_with(
        paths: &Paths,
        name: &str,
        args: &[&str],
        events: impl Fn(Joining) + Send + 'static,
    ) -> Result<Self, String> {
        let mut child = command(paths)
            .args(["pair", "--name", name, "--data-dir"])
            .arg(&paths.data_dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", paths.daemon.display()))?;
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let paths = paths.clone();
        std::thread::spawn(move || {
            // Once paired, it says whose device this now is, then exits.
            let mut paired = false;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                paired |= line.starts_with("user:");
                if let Some(event) = parse_line(&line) {
                    events(event);
                }
            }
            let mut said_last = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stderr, &mut said_last);
            events(if paired {
                match write_config(&paths) {
                    Ok(()) => Joining::Joined,
                    Err(e) => Joining::Failed(e),
                }
            } else {
                Joining::Failed(said(&said_last))
            });
        });
        Ok(Self { child })
    }
}

impl Drop for Join {
    fn drop(&mut self) {
        // Waiting for an approval holds nothing that needs a clean stop.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What `tidefelld` said as it failed, in its standard error.
fn said(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("tidefelld: "))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .map_or_else(|| "it stopped".to_owned(), |l| l.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_pairing_prints() {
        assert_eq!(
            parse_line("code: 4-river-lamp-07"),
            Some(Joining::Code("4-river-lamp-07".into()))
        );
        assert_eq!(
            parse_line("confirmation: 123456"),
            Some(Joining::Confirm("123456".into()))
        );
        assert_eq!(parse_line("device: abcd"), None);
        assert_eq!(parse_line("nothing here"), None);
    }

    #[test]
    fn a_failure_says_why() {
        assert_eq!(
            said(b"tidefelld: the pairing code expired\n"),
            "the pairing code expired"
        );
        assert_eq!(said(b"something else\n\n"), "something else");
        assert_eq!(said(b""), "it stopped");
    }
}
