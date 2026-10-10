//! `tidefell doctor` (ADR 0030): checks that report pass, warning or failure, with advice.

use std::path::Path;

use tidefell_net::PathKind;

use super::Engine;
use crate::views::{Check, CheckLevel};

/// A peer's clock this far off is worth a warning: conflicts are decided by modification time.
const CLOCK_WARN_MS: u64 = 2 * 60 * 1000;
/// Less free space than this fails a check; less than the second, or than 5%, warns.
const SPACE_FAIL: u64 = 100 << 20;
const SPACE_WARN: u64 = 1 << 30;

/// How far `name`'s clock is from this device's, `offset` milliseconds ahead. The offset comes
/// from the peer, so any value, however absurd, gives a check.
fn clock_check(name: &str, offset: i64) -> Check {
    let distance = offset.unsigned_abs();
    let detail = if distance < 1000 {
        format!("{name}'s clock agrees with this device's")
    } else {
        let way = if offset > 0 { "ahead of" } else { "behind" };
        format!("{name}'s clock is {}s {way} this device's", distance / 1000)
    };
    if distance > CLOCK_WARN_MS {
        check(
            "clock",
            CheckLevel::Warning,
            detail,
            Some(
                "Set both devices to take the time from the network. Conflicting edits are \
                 decided by modification time, so a wrong clock can pick the older edit.",
            ),
        )
    } else {
        check("clock", CheckLevel::Pass, detail, None)
    }
}

fn check(name: &str, level: CheckLevel, detail: impl Into<String>, advice: Option<&str>) -> Check {
    Check {
        name: name.to_owned(),
        level,
        detail: detail.into(),
        advice: advice.map(str::to_owned),
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Free and total bytes on the volume holding `path`, where the platform can tell.
fn free_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let stat = rustix::fs::statvfs(path).ok()?;
        let block = stat.f_frsize.max(1);
        Some((stat.f_bavail * block, stat.f_blocks * block))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = path;
        None
    }
}

fn space_check(name: &str, path: &Path) -> Option<Check> {
    let (free, total) = free_space(path)?;
    let detail = format!(
        "{} free of {} at {}",
        size(free),
        size(total),
        path.display()
    );
    let advice = Some("Free some space: files that cannot be written are not synced.");
    Some(if free < SPACE_FAIL {
        check(name, CheckLevel::Failure, detail, advice)
    } else if free < SPACE_WARN || free.saturating_mul(20) < total {
        check(name, CheckLevel::Warning, detail, advice)
    } else {
        check(name, CheckLevel::Pass, detail, None)
    })
}

/// Creates and removes a file in `dir`, a reserved directory no scan looks at: can this device
/// write there?
fn probe(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!(
        "doctor-{:016x}",
        u64::from_le_bytes(crate::random_bytes().map_err(std::io::Error::other)?)
    ));
    std::fs::write(&file, b"")?;
    std::fs::remove_file(&file)
}

/// The volume `path` is on, where the platform can tell.
fn volume_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Whether this device may write in `dir`, without writing anything there. A read-only disk
/// says so too.
fn writable(dir: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).map_err(std::io::Error::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        if std::fs::metadata(dir)?.permissions().readonly() {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }
}

/// The system's limit on watched directories, where there is one to read.
fn watch_limit() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()?
        .trim()
        .parse()
        .ok()
}

impl Engine {
    /// Checks this device's clock against its peers', its free space, its folders, file
    /// watching, its connectivity and its state (ADR 0030). The Android app adds its own.
    pub fn doctor(&self) -> Vec<Check> {
        let mut checks = Vec::new();

        // The engine started, so every folder's state loaded.
        let folders = self.folders();
        checks.push(check(
            "state",
            CheckLevel::Pass,
            format!("engine state of {} folder(s) loaded", folders.len()),
            None,
        ));

        for peer in self.peer_states().into_iter().filter(|p| p.connected) {
            let name = peer.name.clone().unwrap_or_else(|| peer.device.to_string());
            if let Some(offset) = peer.clock_offset_ms {
                checks.push(clock_check(&name, offset));
            }
        }

        // One check per volume is enough; folders often share one.
        let mut volumes = Vec::new();
        let mut space = |path: &Path, checks: &mut Vec<Check>| {
            let volume = volume_of(path);
            if volume.is_none() || !volumes.contains(&volume) {
                volumes.push(volume);
                checks.extend(space_check("space", path));
            }
        };
        space(self.data_dir(), &mut checks);
        for folder in &folders {
            space(&folder.path, &mut checks);
            let path = folder.path.display();
            if let Some(reason) = &folder.unavailable {
                checks.push(check(
                    "folder",
                    CheckLevel::Failure,
                    format!("{path} is unavailable: {reason}"),
                    Some("Mount the drive or restore the folder; it is not synced meanwhile."),
                ));
                continue;
            }
            let reserved = folder.path.join(crate::scan::RESERVED_DIR).join("tmp");
            // Nothing is written in the folder itself, where a scan could pick it up and sync it.
            let access = std::fs::read_dir(&folder.path)
                .map(drop)
                .and_then(|()| writable(&folder.path))
                .and_then(|()| probe(&reserved));
            checks.push(match access {
                Ok(()) => check(
                    "folder",
                    CheckLevel::Pass,
                    format!("{path} can be read and written"),
                    None,
                ),
                Err(e) => check(
                    "folder",
                    CheckLevel::Failure,
                    format!("{path} cannot be read and written: {e}"),
                    Some("Give the user that runs Tidefell read and write access to the folder."),
                ),
            });
            if let Some(reason) = &folder.watch_error {
                let limit = watch_limit()
                    .map(|n| format!(" (the system allows {n} watched directories)"))
                    .unwrap_or_default();
                checks.push(check(
                    "watch",
                    CheckLevel::Warning,
                    format!("{path} is not watched for changes: {reason}{limit}"),
                    Some(
                        "Changes are picked up at the next scan instead of at once. If the \
                         system's limit was reached, raise fs.inotify.max_user_watches.",
                    ),
                ));
            }
        }

        let connectivity = self.node.connectivity();
        for relay in &connectivity.relays {
            checks.push(if relay.connected {
                check(
                    "relay",
                    CheckLevel::Pass,
                    format!("connected to relay {}", relay.url),
                    None,
                )
            } else {
                check(
                    "relay",
                    CheckLevel::Warning,
                    format!(
                        "not connected to relay {}{}",
                        relay.url,
                        relay
                            .error
                            .as_ref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    ),
                    Some(
                        "Devices that cannot connect directly meet through the relay. Check \
                         the internet connection and the relay's address.",
                    ),
                )
            });
        }
        // The network report probes the relays; with them turned off it has nothing to go on.
        let report = connectivity
            .report
            .filter(|_| !connectivity.relays.is_empty());
        if let Some(report) = report {
            let udp = match (report.udp_v4, report.udp_v6) {
                (true, true) => "UDP works over IPv4 and IPv6",
                (true, false) => "UDP works over IPv4",
                (false, true) => "UDP works over IPv6",
                (false, false) => "UDP does not get through",
            };
            let nat = match report.mapping_varies {
                Some(true) => "; the NAT gives each destination a different port",
                _ => "",
            };
            let detail = format!("{udp}{nat}");
            checks.push(if !report.udp_v4 && !report.udp_v6 {
                check(
                    "network",
                    CheckLevel::Warning,
                    detail,
                    Some("Direct connections need UDP; until it gets through, devices meet through the relay."),
                )
            } else if report.mapping_varies == Some(true) {
                check(
                    "network",
                    CheckLevel::Warning,
                    detail,
                    Some("Direct connections through this NAT often fail; devices then use the relay, which is slower."),
                )
            } else {
                check("network", CheckLevel::Pass, detail, None)
            });
            if report.captive_portal == Some(true) {
                checks.push(check(
                    "network",
                    CheckLevel::Failure,
                    "a captive portal blocks the network",
                    Some("Sign in to the network, as in a browser, and check again."),
                ));
            }
        }
        // Connected devices and how, for context.
        let direct = self
            .peer_states()
            .iter()
            .filter(|p| p.path == Some(PathKind::Direct))
            .count();
        let relayed = self
            .peer_states()
            .iter()
            .filter(|p| p.path == Some(PathKind::Relay))
            .count();
        if direct + relayed > 0 {
            checks.push(check(
                "peers",
                CheckLevel::Pass,
                format!("{direct} device(s) connected directly, {relayed} through a relay"),
                None,
            ));
        }
        checks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_clock_offset_gives_a_check() {
        assert_eq!(clock_check("laptop", 300).level, CheckLevel::Pass);
        let ahead = clock_check("laptop", 5 * 60 * 1000);
        assert_eq!(ahead.level, CheckLevel::Warning);
        assert_eq!(
            ahead.detail,
            "laptop's clock is 300s ahead of this device's"
        );
        for extreme in [i64::MIN, i64::MAX] {
            assert_eq!(clock_check("laptop", extreme).level, CheckLevel::Warning);
        }
    }
}
