//! Folder membership: which devices may sync a folder.

use std::collections::BTreeSet;

use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{DeviceId, FolderId, Signed, UserId};

/// Why a device was not admitted to a folder.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The certificate's signature is invalid or not made by its user.
    #[error("invalid device certificate")]
    InvalidCertificate,
    /// The certificate is for a different device than the one connected.
    #[error("certificate is for another device")]
    WrongDevice,
    /// The certificate's user is not a member of the folder.
    #[error("user is not a member of this folder")]
    NotAMember,
}

/// The users allowed to sync one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMembers {
    folder: FolderId,
    users: BTreeSet<UserId>,
}

impl FolderMembers {
    /// A member list for `folder`.
    pub fn new(folder: FolderId, users: impl IntoIterator<Item = UserId>) -> Self {
        Self {
            folder,
            users: users.into_iter().collect(),
        }
    }

    /// The folder.
    pub fn folder(&self) -> FolderId {
        self.folder
    }

    /// The member users.
    pub fn users(&self) -> impl Iterator<Item = &UserId> {
        self.users.iter()
    }

    /// True if `user` is a member.
    pub fn contains(&self, user: &UserId) -> bool {
        self.users.contains(user)
    }

    /// Adds a member. Returns false if already present.
    pub fn add(&mut self, user: UserId) -> bool {
        self.users.insert(user)
    }

    /// Removes a member. Returns false if absent.
    pub fn remove(&mut self, user: &UserId) -> bool {
        self.users.remove(user)
    }

    /// Admits `peer` if `certificate` is valid, is for `peer`, and names a member user.
    /// `peer` must be the authenticated endpoint ID of the connection.
    pub fn authorize(
        &self,
        certificate: &Signed<DeviceCertificate>,
        peer: DeviceId,
    ) -> Result<DeviceCertificate, AuthError> {
        let certificate = certificate
            .verify_certificate()
            .map_err(|_| AuthError::InvalidCertificate)?;
        if certificate.device != peer {
            return Err(AuthError::WrongDevice);
        }
        if !self.contains(&certificate.user) {
            return Err(AuthError::NotAMember);
        }
        Ok(certificate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKey, UserKey};

    #[test]
    fn authorize() {
        let alice = UserKey::from_seed([1; 32]);
        let mallory = UserKey::from_seed([2; 32]);
        let laptop = DeviceKey::from_seed([3; 32]);
        let phone = DeviceKey::from_seed([4; 32]);
        let members = FolderMembers::new(FolderId([9; 32]), [alice.id()]);

        let cert = alice.certify(laptop.id(), "laptop", 0);
        assert_eq!(
            members.authorize(&cert, laptop.id()).unwrap().user,
            alice.id()
        );
        assert_eq!(
            members.authorize(&cert, phone.id()),
            Err(AuthError::WrongDevice),
            "a certificate cannot be replayed by another device"
        );

        let outsider = mallory.certify(phone.id(), "phone", 0);
        assert_eq!(
            members.authorize(&outsider, phone.id()),
            Err(AuthError::NotAMember)
        );

        // A certificate claiming alice but signed by mallory.
        let mut forged = alice.certify(phone.id(), "phone", 0);
        forged.signer = mallory.certify(phone.id(), "phone", 0).signer;
        assert_eq!(
            members.authorize(&forged, phone.id()),
            Err(AuthError::InvalidCertificate)
        );
    }

    #[test]
    fn membership_changes() {
        let mut members = FolderMembers::new(FolderId([9; 32]), []);
        let alice = UserKey::from_seed([1; 32]).id();
        assert!(members.add(alice));
        assert!(!members.add(alice));
        assert!(members.contains(&alice));
        assert!(members.remove(&alice));
        assert_eq!(members.users().count(), 0);
    }
}
