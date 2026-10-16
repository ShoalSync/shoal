//! Which devices this one knows, and whether an author is vouched for (ADR 0024): a version's
//! author must hold a certificate from a user who is a member of the folder. This release keeps
//! and serves certificates and records whether each author is vouched for; a later one refuses
//! versions that are not (ADR 0024's rollout).

use std::collections::{BTreeMap, BTreeSet};

use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::{DeviceId, FolderId, Signed, UserId};

#[derive(Debug, Default)]
pub(crate) struct Vouching {
    /// The newest certificate seen for each device, as issued.
    certificates: BTreeMap<DeviceId, Signed<DeviceCertificate>>,
    /// Authors to ask the next peer for, by folder: ones that have versions here and no
    /// certificate.
    pending: BTreeMap<FolderId, BTreeSet<DeviceId>>,
    /// Authors noted as not vouched for, once each, so the log says so once.
    noted: BTreeSet<(FolderId, DeviceId)>,
    /// Counts the certificates and revocations learnt, so what was worked out from them can tell
    /// it is stale.
    generation: u64,
    /// Device revocations, by user and device (ADR 0040): the first seen of each is kept. The
    /// set only grows.
    revocations: BTreeMap<(UserId, DeviceId), Signed<DeviceRevocation>>,
}

/// Most revocations kept: users revoke few devices, and a peer cannot make this device keep
/// an unbounded list.
const MAX_REVOCATIONS: usize = 4096;

/// Why an author is not vouched for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unvouched {
    /// No certificate for the author is known; one is asked for.
    NoCertificate,
    /// The author's certificate is from a user who is not a member of the folder.
    NotAMember,
}

impl Vouching {
    pub(crate) fn new(
        certificates: impl IntoIterator<Item = Signed<DeviceCertificate>>,
        revocations: impl IntoIterator<Item = Signed<DeviceRevocation>>,
    ) -> Self {
        let mut this = Self::default();
        for certificate in certificates {
            this.learn(certificate);
        }
        // Kept before, so relevant then.
        for revocation in revocations {
            this.learn_revocation(revocation, |_| true);
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
            .and_then(|c| c.verify_certificate().ok())
            .is_none_or(|held| body.issued_ms > held.issued_ms);
        if !newer {
            return false;
        }
        for authors in self.pending.values_mut() {
            authors.remove(&body.device);
        }
        self.certificates.insert(body.device, certificate);
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
    ) -> bool {
        let Ok(body) = revocation.verify_revocation() else {
            return false;
        };
        let key = (body.user, body.device);
        if self.revocations.contains_key(&key) || self.revocations.len() >= MAX_REVOCATIONS {
            return false;
        }
        let known_user = relevant(&body.user)
            || self
                .certificates
                .values()
                .filter_map(|c| c.verify_certificate().ok())
                .any(|c| c.user == body.user);
        if !known_user {
            return false;
        }
        self.revocations.insert(key, revocation);
        self.generation += 1;
        true
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
        self.certificates.get(&device)
    }

    pub(crate) fn certificates(&self) -> impl Iterator<Item = &Signed<DeviceCertificate>> {
        self.certificates.values()
    }

    /// The user a device's certificate names, if one is held.
    pub(crate) fn user_of(&self, device: DeviceId) -> Option<UserId> {
        self.certificates
            .get(&device)
            .and_then(|c| c.verify_certificate().ok())
            .map(|c| c.user)
    }

    /// Records that `author` made a version in `folder`, whose members `is_member` tells.
    /// Returns why it is not vouched for, if it is not, and whether that is new, for the log.
    pub(crate) fn note_author(
        &mut self,
        folder: FolderId,
        author: DeviceId,
        is_member: impl Fn(&UserId) -> bool,
    ) -> Option<(Unvouched, bool)> {
        let why = match self.user_of(author) {
            Some(user) if is_member(&user) => return None,
            Some(_) => Unvouched::NotAMember,
            None => {
                self.pending.entry(folder).or_default().insert(author);
                Unvouched::NoCertificate
            }
        };
        Some((why, self.noted.insert((folder, author))))
    }

    /// Takes the authors of `folder` to ask a peer for.
    pub(crate) fn take_pending(&mut self, folder: &FolderId) -> Vec<DeviceId> {
        self.pending
            .remove(folder)
            .map(|authors| authors.into_iter().collect())
            .unwrap_or_default()
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
        assert!(vouching.learn_revocation(by_mallory, |_| true));
        assert!(!vouching.is_revoked(&alices_phone));
        // A user this device has nothing to do with is not kept at all.
        let stranger = UserKey::from_seed([4; 32]);
        assert!(
            !vouching.learn_revocation(stranger.revoke(phone.id(), 5, None), |u| *u == alice.id())
        );
        // Alice's own: kept once, and it revokes the phone.
        let by_alice = alice.revoke(phone.id(), 6, None);
        assert!(vouching.learn_revocation(by_alice.clone(), |u| *u == alice.id()));
        assert!(
            !vouching.learn_revocation(by_alice, |u| *u == alice.id()),
            "already known"
        );
        assert!(vouching.is_revoked(&alices_phone));
        assert_eq!(vouching.revocations().count(), 2);
    }

    #[test]
    fn an_author_is_vouched_for_by_a_member_users_certificate() {
        let (alice, bob) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
        let device = DeviceKey::from_seed([3; 32]);
        let folder = FolderId([9; 32]);
        let mut vouching = Vouching::default();
        assert_eq!(
            vouching.note_author(folder, device.id(), |u| *u == alice.id()),
            Some((Unvouched::NoCertificate, true))
        );
        assert_eq!(vouching.take_pending(&folder), vec![device.id()]);
        assert!(vouching.take_pending(&folder).is_empty(), "asked once");

        assert!(vouching.learn(certify(&bob, &device, 1)));
        assert_eq!(
            vouching.note_author(folder, device.id(), |u| *u == alice.id()),
            Some((Unvouched::NotAMember, false)),
            "bob is not a member, and it was noted already"
        );
        assert!(
            vouching.take_pending(&folder).is_empty(),
            "a certificate is held"
        );

        assert!(vouching.learn(certify(&alice, &device, 2)), "newer");
        assert!(
            !vouching.learn(certify(&bob, &device, 1)),
            "older is ignored"
        );
        assert_eq!(
            vouching.note_author(folder, device.id(), |u| *u == alice.id()),
            None
        );
    }
}
