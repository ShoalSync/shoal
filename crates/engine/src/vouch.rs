//! Which devices this one knows, and whether an author is vouched for (ADR 0024): a version's
//! author must hold a certificate from a user who is a member of the folder. This release keeps
//! and serves certificates and records whether each author is vouched for; a later one refuses
//! versions that are not (ADR 0024's rollout).

use std::collections::{BTreeMap, BTreeSet};

use tidefell_proto::identity::DeviceCertificate;
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
}

/// Why an author is not vouched for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unvouched {
    /// No certificate for the author is known; one is asked for.
    NoCertificate,
    /// The author's certificate is from a user who is not a member of the folder.
    NotAMember,
}

impl Vouching {
    pub(crate) fn new(certificates: impl IntoIterator<Item = Signed<DeviceCertificate>>) -> Self {
        let mut this = Self::default();
        for certificate in certificates {
            this.learn(certificate);
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
        true
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
