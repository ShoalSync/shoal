//! Free-space floors (ADR 0037): below a volume's floor, nothing more is downloaded to it, so a
//! full disk pauses downloads instead of failing file by file and never stops metadata from
//! being saved.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

/// The floor when none is set: 2% of the volume, or this, whichever is smaller.
const DEFAULT_FLOOR_MAX: u64 = 1 << 30;

/// Free and total bytes on the volume holding `path`, where the platform can tell.
pub(crate) fn free(path: &Path) -> Option<(u64, u64)> {
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

/// The volume holding `path`, to tell volumes apart, where the platform can tell.
pub(crate) fn volume(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        path.metadata().ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The floor of a volume of `total` bytes: `configured`, or 2% of it up to 1 GiB.
pub(crate) fn floor(total: u64, configured: Option<u64>) -> u64 {
    configured.unwrap_or_else(|| (total / 50).min(DEFAULT_FLOOR_MAX))
}

/// Why a write does not fit: the volume, and its free space against its floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Short {
    pub(crate) free: u64,
    pub(crate) floor: u64,
    pub(crate) needed: u64,
}

impl std::fmt::Display for Short {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "waiting for space: {} free, and {} more must stay free",
            size(self.free),
            size(self.floor)
        )?;
        if self.needed > 0 {
            write!(f, " besides this file's {}", size(self.needed))?;
        }
        Ok(())
    }
}

/// Which volumes are below their floor, so they resume only once well above it.
#[derive(Debug, Default)]
pub(crate) struct Floors {
    /// Bytes to keep free on each volume, or the default.
    configured: Mutex<Option<u64>>,
    low: Mutex<HashSet<u64>>,
}

impl Floors {
    pub(crate) fn new(configured: Option<u64>) -> Self {
        Self {
            configured: Mutex::new(configured),
            low: Mutex::default(),
        }
    }

    /// Whether `bytes` more fit on the volume holding `path` above its floor. A volume found
    /// below its floor stays low until it is back above the floor by 10% (ADR 0037).
    pub(crate) fn room(&self, path: &Path, bytes: u64) -> Result<(), Short> {
        let (Some((free, total)), Some(volume)) = (free(path), volume(path)) else {
            return Ok(());
        };
        let floor = floor(total, self.configured());
        let mut low = self.low.lock().unwrap();
        let needed_floor = if low.contains(&volume) {
            floor + floor / 10
        } else {
            floor
        };
        if free >= needed_floor.saturating_add(bytes) {
            low.remove(&volume);
            Ok(())
        } else {
            low.insert(volume);
            Err(Short {
                free,
                floor,
                needed: bytes,
            })
        }
    }

    /// Why the volume holding `path` takes no downloads, if it is below its floor.
    pub(crate) fn low(&self, path: &Path) -> Option<Short> {
        let volume = volume(path)?;
        if !self.low.lock().unwrap().contains(&volume) {
            return None;
        }
        let (free, total) = free(path)?;
        Some(Short {
            free,
            floor: floor(total, self.configured()),
            needed: 0,
        })
    }

    /// The bytes to keep free set in the configuration, if any.
    pub(crate) fn configured(&self) -> Option<u64> {
        *self.configured.lock().unwrap()
    }

    /// Keeps `bytes` free on every volume from now on, or the default.
    pub(crate) fn set(&self, bytes: Option<u64>) {
        *self.configured.lock().unwrap() = bytes;
    }
}

/// `bytes` in binary units, as the doctor writes them.
pub(crate) fn size(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_floor_is_two_percent_up_to_a_gibibyte() {
        assert_eq!(floor(10 << 30, None), (10 << 30) / 50);
        assert_eq!(floor(1 << 40, None), 1 << 30);
        assert_eq!(floor(1 << 40, Some(5)), 5);
    }

    #[test]
    fn a_low_volume_resumes_only_well_above_its_floor() {
        let dir = tempfile::tempdir().unwrap();
        let Some((free, _)) = free(dir.path()) else {
            return;
        };
        if free < 2 << 30 {
            return;
        }
        // A floor 64 MiB under what is free, so other writes meanwhile do not matter: a file
        // that would cross it waits.
        let floors = Floors::new(Some(free - (64 << 20)));
        assert!(floors.room(dir.path(), 10).is_ok());
        let short = floors.room(dir.path(), 1 << 30).unwrap_err();
        assert!(short.to_string().starts_with("waiting for space"));
        assert!(floors.low(dir.path()).is_some());
        // Now low, it needs 10% more than the floor, over 100 MiB, which a small file does not
        // clear.
        assert!(floors.room(dir.path(), 10).is_err());
        // With plenty of room, it resumes.
        let roomy = Floors::new(Some(0));
        assert!(roomy.room(dir.path(), 10).is_ok());
        assert!(roomy.low(dir.path()).is_none());
    }
}
