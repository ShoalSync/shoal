//! Tidefell `fs-cfapi` crate: Windows placeholders through the Cloud Files API (ADR 0042).
//!
//! A partial replica's folder is a sync root: files not on this device are placeholders that
//! Explorer shows, and reading one fetches it through the engine. Deletes and renames other
//! programs make arrive as callbacks and become the engine's; pins and "Free up space" in
//! Explorer become the engine's pins and records. Windows only: elsewhere the crate is empty,
//! so the workspace builds everywhere.
#![cfg(windows)]

mod backend;
mod ops;
mod place;
mod priorities;
mod provider;
mod reconcile;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cloud_filter::root::{
    Connection, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};
use tidefell_proto::FolderId;

pub use backend::{Backend, BackendError, EngineFolder, FileEntry, Sink};
pub use ops::free;
pub use place::unusable;
pub use reconcile::Reconciled;

/// The provider's name, as Windows lists it.
const PROVIDER: &str = "Tidefell";

/// A folder registered and connected as a sync root. Dropping it disconnects; the
/// registration stays, so Explorer keeps showing the folder, until [`SyncRoot::unregister`].
///
/// Disconnecting waits for the provider's callbacks in flight, which call into the engine. So
/// dropping hands the disconnect to a thread of its own: on one of the engine's runtime
/// threads it would wait on work that thread has to do, until Windows gave up on the fetches
/// pending, two minutes later.
pub struct SyncRoot {
    root: PathBuf,
    id: SyncRootId,
    backend: Arc<dyn Backend>,
    connection: Option<Connection<provider::Provider>>,
    /// The storage priorities Explorer shows, by path (see [`SyncRoot::show_priorities`]).
    shown: Mutex<HashMap<String, u8>>,
}

fn sync_root_id(folder: FolderId) -> io::Result<SyncRootId> {
    Ok(SyncRootIdBuilder::new(PROVIDER)
        .user_security_id(SecurityId::current_user().map_err(io::Error::other)?)
        .account_name(folder.to_string())
        .build())
}

/// Registers `root` as `folder`'s sync root, named `name` in Explorer, if it is not already,
/// connects to it, and makes its placeholders match the folder. A root registered before it
/// defined the storage priority state is registered again in place, which leaves its
/// placeholders as they are.
pub fn connect<B: Backend>(
    backend: B,
    root: &Path,
    folder: FolderId,
    name: &str,
) -> io::Result<SyncRoot> {
    let id = sync_root_id(folder)?;
    // The sync root manager too, not only connecting, waits while no fill writes.
    let alone = provider::TRANSFERS
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let registered = id.is_registered().map_err(io::Error::other)?
        && id.info().is_ok_and(|info| {
            info.item_property_definitions()
                .iter()
                .any(|(property, _)| *property == priorities::PROPERTY)
        });
    if !registered {
        id.register(
            SyncRootInfo::default()
                .with_display_name(name)
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::AlwaysFull)
                .with_allow_pinning(true)
                .with_icon("%SystemRoot%\\system32\\imageres.dll,0")
                .with_version(env!("CARGO_PKG_VERSION"))
                .with_path(root)
                .map_err(io::Error::other)?
                .with_item_property_definition(priorities::PROPERTY, priorities::NAME)
                .map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)?;
    }
    let backend: Arc<dyn Backend> = Arc::new(backend);
    let connection = Session::new()
        .connect(
            root,
            provider::Provider::new(root.to_owned(), backend.clone()),
        )
        .map_err(io::Error::other)?;
    drop(alone);
    let sync_root = SyncRoot {
        root: root.to_owned(),
        id,
        backend,
        connection: Some(connection),
        shown: Mutex::default(),
    };
    // Deletes made while disconnected are found by the engine's scan, before anything is made
    // again.
    sync_root.backend.scan();
    sync_root.changed()?;
    Ok(sync_root)
}

impl SyncRoot {
    /// The folder's root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Makes the placeholders match the folder again, as after any change to it.
    pub fn changed(&self) -> io::Result<Reconciled> {
        reconcile::reconcile(&self.root, &*self.backend)
    }

    /// Shows an encrypted folder's storage priorities, each folder-relative path's, in
    /// Explorer's Status column: an icon for each file and directory not at the default, 128.
    /// Only those changed since the last call are set again. Blocks, so call it off the
    /// engine's runtime threads. Returns how many it set.
    pub fn show_priorities(&self, priorities: &[(String, u8)]) -> io::Result<usize> {
        let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        priorities::show(&self.root, priorities, &mut shown)
    }

    /// Disconnects, keeping the registration, and returns once disconnected. Blocks, so call it
    /// off the engine's runtime threads, as from `spawn_blocking`.
    pub fn disconnect(mut self) {
        disconnect(self.connection.take());
    }

    /// Disconnects and removes the registration: Explorer no longer shows the folder as a
    /// cloud folder. Placeholders left are files not on this device. Blocks until both are
    /// done, so call it off the engine's runtime threads, as from `spawn_blocking`.
    pub fn unregister(mut self) -> io::Result<()> {
        disconnect(self.connection.take());
        let _alone = provider::TRANSFERS
            .write()
            .unwrap_or_else(|e| e.into_inner());
        self.id.unregister().map_err(io::Error::other)
    }
}

/// Disconnects, while no fill of any root writes to Windows.
fn disconnect(connection: Option<Connection<provider::Provider>>) {
    if let Some(connection) = connection {
        let _alone = provider::TRANSFERS
            .write()
            .unwrap_or_else(|e| e.into_inner());
        drop(connection);
    }
}

impl Drop for SyncRoot {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            std::thread::spawn(move || disconnect(Some(connection)));
        }
    }
}
