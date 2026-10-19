//! Which devices this one knows, and whether an author is vouched for (ADR 0024): a version is
//! applied only if its author holds a certificate from a user who is a member of the folder now,
//! and that user has not revoked the device (ADR 0040).

use std::collections::{BTreeMap, BTreeSet};

use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::{DeviceId, FolderId, Signed, UserId};

#[derive(Debug, Default)]
pub(crate) struct Vouching {
    /// The newest certificate seen for each device, as issued, and what it says: each is kept
    /// only once it verifies, and not checked again each time an author is judged.
    certificates: BTreeMap<DeviceId, (Signed<DeviceCertificate>, DeviceCertificate)>,
    /// Authors noted as not vouched for, once each, so the log says so once.
    noted: BTreeSet<(FolderId, DeviceId)>,
    /// Counts the certificates and revocations learnt, so what was worked out from them can tell
    /// it is stale.
    generation: u64,
    /// Device revocations, by user and device (ADR 0040): the first seen of each is kept. The
    /// set only grows.
    revocations: BTreeMap<(UserId, DeviceId), Signed<DeviceRevocation>>,
    /// When each revoked device's revocation was learnt by this run, this device's time: those
    /// kept from before it count from its start. A replica withdraws what it cannot take over
    /// only after an index exchange since then (ADR 0040).
    /// The time by this device's clock, for the retention guard, and by the monotonic clock,
    /// for the exchange a withdrawal waits for: a clock set back must not make it wait forever.
    learnt: BTreeMap<(UserId, DeviceId), (i64, std::time::Instant)>,
    /// Authors a peer that sent their versions answered it cannot vouch for: those versions
    /// can be taken over. Not kept: asked again after a restart.
    unvouchable: BTreeSet<DeviceId>,
}

/// Whether revocation `a` is kept over `b`, of the same device: the earlier time of loss, a time
/// over none, then the earlier revocation, then the lesser signature, so every replica keeps the
/// same one whatever order they come in.
fn precedes(a: &Signed<DeviceRevocation>, b: &Signed<DeviceRevocation>) -> bool {
    let (Ok(x), Ok(y)) = (a.verify_revocation(), b.verify_revocation()) else {
        return false;
    };
    let order = |r: &DeviceRevocation| (r.lost_ms.is_none(), r.lost_ms, r.revoked_ms);
    (order(&x), a.signature) < (order(&y), b.signature)
}

/// Most revocations kept: users revoke few devices, and a peer cannot make this device keep
/// an unbounded list.
const MAX_REVOCATIONS: usize = 4096;

/// Why an author is not vouched for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unvouched {
    /// No certificate for the author is known: the peer that sent its versions could not vouch
    /// for it either.
    NoCertificate,
    /// The author's certificate is from a user who is not a member of the folder now.
    NotAMember,
    /// Its user revoked it (ADR 0040).
    Revoked,
}

impl Vouching {
    pub(crate) fn new(
        certificates: impl IntoIterator<Item = Signed<DeviceCertificate>>,
        revocations: impl IntoIterator<Item = Signed<DeviceRevocation>>,
        now_ms: i64,
    ) -> Self {
        let mut this = Self::default();
        for certificate in certificates {
            this.learn(certificate);
        }
        // Kept before, so relevant then.
        for revocation in revocations {
            this.learn_revocation(revocation, |_| true, now_ms);
        }
        this
    }

    /// Keeps `certificate` if it verifies and is newer than the one held for its device.
    /// Returns true if something changed.
    pub(crate) fn learn(&mut self, certificate: Signed<DeviceCertificate>) -> bool {
        let Ok(body) = certificate.verify_certificate() else {
            return false;
        };
        let newer = self
            .certificates
            .get(&body.device)
            .is_none_or(|(_, held)| body.issued_ms > held.issued_ms);
        if !newer {
            return false;
        }
        self.certificates
            .insert(body.device, (certificate, body.clone()));
        self.generation += 1;
        true
    }

    /// Changes each time a certificate or a revocation is learnt.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Keeps `revocation` if it verifies, is new, and is by a user this device has to do with:
    /// one `relevant` says is (its own user, or a member of one of its folders) or the user of
    /// a certificate it holds. Returns true if it was kept.
    pub(crate) fn learn_revocation(
        &mut self,
        revocation: Signed<DeviceRevocation>,
        relevant: impl Fn(&UserId) -> bool,
        now_ms: i64,
    ) -> bool {
        let Ok(body) = revocation.verify_revocation() else {
            return false;
        };
        let key = (body.user, body.device);
        if let Some(held) = self.revocations.get(&key) {
            // Two revocations of one device, made apart on machines that both hold the user key:
            // every replica keeps the same one, the earliest time of loss (ADR 0040).
            let replaces = precedes(&revocation, held);
            if !replaces {
                return false;
            }
        } else if self.revocations.len() >= MAX_REVOCATIONS {
            return false;
        }
        let known_user =
            relevant(&body.user) || self.certificates.values().any(|(_, c)| c.user == body.user);
        if !known_user {
            return false;
        }
        self.revocations.insert(key, revocation);
        // A replaced revocation counts from now: withdrawals wait for an exchange since.
        self.learnt.insert(key, (now_ms, std::time::Instant::now()));
        self.generation += 1;
        true
    }

    /// The revocation of `device` by its user, by the certificate held for it, if revoked.
    pub(crate) fn revocation_of(&self, device: DeviceId) -> Option<DeviceRevocation> {
        let user = self.certificates.get(&device).map(|(_, c)| c.user)?;
        self.revocations
            .get(&(user, device))
            .and_then(|r| r.verify_revocation().ok())
    }

    /// Whether the device `certificate` is for has been revoked by the user it names.
    pub(crate) fn is_revoked(&self, certificate: &DeviceCertificate) -> bool {
        self.revocations
            .contains_key(&(certificate.user, certificate.device))
    }

    pub(crate) fn revocations(&self) -> impl Iterator<Item = &Signed<DeviceRevocation>> {
        self.revocations.values()
    }

    pub(crate) fn certificate(&self, device: DeviceId) -> Option<&Signed<DeviceCertificate>> {
        self.certificates.get(&device).map(|(signed, _)| signed)
    }

    pub(crate) fn certificates(&self) -> impl Iterator<Item = &Signed<DeviceCertificate>> {
        self.certificates.values().map(|(signed, _)| signed)
    }

    /// Whether a certificate for `device` is held.
    pub(crate) fn knows(&self, device: DeviceId) -> bool {
        self.certificates.contains_key(&device)
    }

    /// When this run learnt that its user revoked `device`, by the certificate held for it, if
    /// it did: a revocation of the same device ID by another user revokes nothing, and counts
    /// for nothing here.
    pub(crate) fn revoked_since(&self, device: DeviceId) -> Option<i64> {
        let user = self.certificates.get(&device).map(|(_, c)| c.user)?;
        self.learnt.get(&(user, device)).map(|(ms, _)| *ms)
    }

    /// When this run learnt that its user revoked `device`, by the monotonic clock.
    pub(crate) fn revoked_at(&self, device: DeviceId) -> Option<std::time::Instant> {
        let user = self.certificates.get(&device).map(|(_, c)| c.user)?;
        self.learnt.get(&(user, device)).map(|(_, at)| *at)
    }

    /// Whether `user` has revoked `device`.
    pub(crate) fn revoked_by(&self, user: UserId, device: DeviceId) -> bool {
        self.revocations.contains_key(&(user, device))
    }

    /// Notes that a peer holding `author`'s versions answered it cannot vouch for it. Returns
    /// true if that is new.
    pub(crate) fn cannot_vouch(&mut self, author: DeviceId) -> bool {
        !self.knows(author) && self.unvouchable.insert(author)
    }

    /// Whether a peer answered it cannot vouch for `author`.
    pub(crate) fn unvouchable(&self, author: DeviceId) -> bool {
        self.unvouchable.contains(&author)
    }

    /// Whether `author`'s versions in `folder`, whose members `is_member` tells, may be applied:
    /// why not if not, and whether that is new, for the log (ADR 0024).
    pub(crate) fn judge(
        &mut self,
        folder: FolderId,
        author: DeviceId,
        is_member: impl Fn(&UserId) -> bool,
    ) -> Result<(), (Unvouched, bool)> {
        let why = match self.certificates.get(&author).map(|(_, c)| c) {
            None => Unvouched::NoCertificate,
            Some(c) if self.is_revoked(c) => Unvouched::Revoked,
            Some(c) if !is_member(&c.user) => Unvouched::NotAMember,
            Some(_) => return Ok(()),
        };
        Err((why, self.noted.insert((folder, author))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidefell_identity::{DeviceKey, UserKey};

    fn certify(user: &UserKey, device: &DeviceKey, issued_ms: i64) -> Signed<DeviceCertificate> {
        user.certify(device.id(), "d", issued_ms)
    }

    #[test]
    fn revocations_bind_their_user_and_only_relevant_ones_are_kept() {
        let (alice, mallory) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
        let phone = DeviceKey::from_seed([3; 32]);
        let mut vouching = Vouching::default();
        let alices_phone = alice
            .certify(phone.id(), "phone", 0)
            .verify_certificate()
            .unwrap();
        // Mallory revoking alice's phone revokes nothing: it is not mallory's device.
        let by_mallory = mallory.revoke(phone.id(), 5, None);
        assert!(vouching.learn_revocation(by_mallory, |_| true, 0));
        assert!(!vouching.is_revoked(&alices_phone));
        // A user this device has nothing to do with is not kept at all.
        let stranger = UserKey::from_seed([4; 32]);
        assert!(!vouching.learn_revocation(
            stranger.revoke(phone.id(), 5, None),
            |u| *u == alice.id(),
            0
        ));
        // Alice's own: kept once, and it revokes the phone.
        let by_alice = alice.revoke(phone.id(), 6, None);
        assert!(vouching.learn_revocation(by_alice.clone(), |u| *u == alice.id(), 0));
        assert!(
            !vouching.learn_revocation(by_alice, |u| *u == alice.id(), 0),
            "already known"
        );
        assert!(vouching.is_revoked(&alices_phone));
        assert_eq!(vouching.revocations().count(), 2);
    }

    #[test]
    fn an_author_is_vouched_for_by_a_member_users_certificate_not_revoked() {
        let (alice, bob) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
        let device = DeviceKey::from_seed([3; 32]);
        let folder = FolderId([9; 32]);
        let alice_only = |u: &UserId| *u == alice.id();
        let mut vouching = Vouching::default();
        assert_eq!(
            vouching.judge(folder, device.id(), alice_only),
            Err((Unvouched::NoCertificate, true))
        );
        assert!(!vouching.knows(device.id()));

        assert!(vouching.learn(certify(&bob, &device, 1)));
        assert_eq!(
            vouching.judge(folder, device.id(), alice_only),
            Err((Unvouched::NotAMember, false)),
            "bob is not a member, and it was noted already"
        );
        assert!(vouching.learn(certify(&alice, &device, 2)), "newer");
        assert!(
            !vouching.learn(certify(&bob, &device, 1)),
            "older is ignored"
        );
        assert_eq!(vouching.judge(folder, device.id(), alice_only), Ok(()));

        assert!(vouching.learn_revocation(alice.revoke(device.id(), 3, None), |_| true, 0));
        assert_eq!(
            vouching.judge(folder, device.id(), alice_only),
            Err((Unvouched::Revoked, false))
        );
    }
}
