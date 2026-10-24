//! A time of loss (ADR 0040): which of a revoked device's versions a replica received before the
//! user lost it, and which after. Each replica judges by when it received each version, by its own
//! clock (ADR 0040, "Times come from trusted clocks"), and the revoking machine previews the same
//! judgement from its own record.

use shoal_meta::Retention;

use crate::db::Rebuild;

const DAY_MS: i64 = 86_400_000;

/// How far back a time of loss reaches in a folder that keeps deleted files forever.
const FOREVER_REACH_DAYS: i64 = 30;

/// A time of loss as a folder applies it: no earlier than `from_ms` less the folder's
/// retention, 30 days for one that keeps deleted files forever, so everything withdrawn is
/// still in the recycle bin (ADR 0040's guard against a revocation meant to do harm). `from_ms`
/// is when this replica learnt of the revocation, by its own clock, not the time the revocation
/// states, which whoever signed it chose; the preview uses the time now.
pub(crate) fn effective_lost(lost_ms: i64, from_ms: i64, retention: Retention) -> i64 {
    let days = retention.days().map_or(FOREVER_REACH_DAYS, i64::from);
    lost_ms.max(from_ms.saturating_sub(days.saturating_mul(DAY_MS)))
}

/// When a version was received, against a time of loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Received {
    /// Before the loss: taken over like any version of a revoked device.
    Before,
    /// At or after it: withdrawn.
    After,
    /// Not known: left alone, for other replicas' judgement to settle.
    Unknown,
}

/// Judges a version received at `received_ms`, none if this replica recorded no time for it,
/// against the time of loss `lost_ms`. A version without a time was received before the loss
/// only if this replica recorded times from before it (`receipts_since`) and was not rebuilt
/// since: versions received again while rebuilding have none (ADR 0034).
pub(crate) fn judge(
    received_ms: Option<i64>,
    lost_ms: i64,
    receipts_since: Option<i64>,
    rebuild: Option<&Rebuild>,
) -> Received {
    match received_ms {
        Some(at) if at < lost_ms => Received::Before,
        Some(_) => Received::After,
        None => {
            let recorded_before = receipts_since.is_some_and(|since| since <= lost_ms);
            let rebuilt_before = rebuild.is_none_or(|r| r.finished_ms.is_some_and(|f| f < lost_ms));
            if recorded_before && rebuilt_before {
                Received::Before
            } else {
                Received::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_of_loss_reaches_back_at_most_the_retention() {
        let revoked = 100 * DAY_MS;
        assert_eq!(
            effective_lost(95 * DAY_MS, revoked, Retention::Days(30)),
            95 * DAY_MS
        );
        assert_eq!(effective_lost(0, revoked, Retention::Days(30)), 70 * DAY_MS);
        assert_eq!(effective_lost(0, revoked, Retention::Forever), 70 * DAY_MS);
        assert_eq!(effective_lost(0, revoked, Retention::Days(7)), 93 * DAY_MS);
    }

    #[test]
    fn versions_are_judged_by_when_they_were_received() {
        let rebuilt = |finished_ms| Rebuild {
            started_ms: 0,
            reason: String::new(),
            restored: true,
            finished_ms,
        };
        assert_eq!(judge(Some(9), 10, Some(0), None), Received::Before);
        assert_eq!(judge(Some(10), 10, Some(0), None), Received::After);
        // No time: before only if times were recorded from before the loss, and no rebuild
        // since.
        assert_eq!(judge(None, 10, Some(5), None), Received::Before);
        assert_eq!(judge(None, 10, Some(15), None), Received::Unknown);
        assert_eq!(judge(None, 10, None, None), Received::Unknown);
        assert_eq!(
            judge(None, 10, Some(5), Some(&rebuilt(Some(8)))),
            Received::Before
        );
        assert_eq!(
            judge(None, 10, Some(5), Some(&rebuilt(Some(12)))),
            Received::Unknown
        );
        assert_eq!(
            judge(None, 10, Some(5), Some(&rebuilt(None))),
            Received::Unknown
        );
    }
}
