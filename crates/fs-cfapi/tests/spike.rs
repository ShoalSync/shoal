//! Phase 8's spike (ADR 0042): can a program installed without package identity register a
//! sync root that Windows lists, create a placeholder in it, and fill that placeholder through
//! a callback when it is read? Needs a desktop session, as the Windows runner's is.
#![cfg(windows)]

use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, SyncFilter};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder_file::PlaceholderFile;
use cloud_filter::root::{
    HydrationType, PopulationType, SecurityId, Session, SyncRootIdBuilder, SyncRootInfo,
};
use cloud_filter::utility::WriteAt;

/// A placeholder's data is not on disk until it is read.
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Serves each file's content by the blob its placeholder was made with: its name.
struct Memory(HashMap<Vec<u8>, Vec<u8>>);

impl SyncFilter for Memory {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let content = self
            .0
            .get(request.file_blob())
            .ok_or(CloudErrorKind::InvalidRequest)?;
        let range = info.required_file_range();
        let start = usize::try_from(range.start).map_err(|_| CloudErrorKind::InvalidRequest)?;
        ticket
            .write_at(&content[start..], range.start)
            .map_err(|_| CloudErrorKind::InvalidRequest)?;
        Ok(())
    }

    fn fetch_placeholders(
        &self,
        _request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        ticket
            .pass_with_placeholder(&mut [])
            .map_err(|_| CloudErrorKind::InvalidRequest)
    }
}

fn attributes(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().file_attributes()
}

#[test]
fn an_unpackaged_program_registers_a_sync_root_and_hydrates_a_placeholder() {
    // In the user's profile, as sync roots are: the shell will not show one inside a hidden
    // folder, such as AppData, where temporary directories are.
    let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
    let dir = tempfile::Builder::new()
        .prefix("Shoal spike ")
        .tempdir_in(profile)
        .unwrap();
    let root = dir.path().join("Shoal spike");
    std::fs::create_dir(&root).unwrap();

    let id = SyncRootIdBuilder::new("ShoalSpike")
        .user_security_id(SecurityId::current_user().unwrap())
        .account_name("spike")
        .build();
    if id.is_registered().unwrap() {
        id.unregister().unwrap();
    }
    let registered = id.register(
        SyncRootInfo::default()
            .with_display_name("Shoal spike")
            .with_hydration_type(HydrationType::Full)
            .with_population_type(PopulationType::AlwaysFull)
            .with_icon("%SystemRoot%\\system32\\imageres.dll,0")
            .with_version("0.0.1")
            .with_path(&root)
            .unwrap(),
    );
    eprintln!("register: {registered:?}");
    registered.expect("registering a sync root without package identity");

    // Windows lists it, as Explorer's navigation pane does.
    let canonical = |p: &Path| {
        std::fs::canonicalize(p)
            .map(|c| c.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let roots = cloud_filter::root::active_roots().unwrap();
    for r in &roots {
        eprintln!("active sync root: {:?} at {:?}", r.display_name(), r.path());
    }
    let listed = roots
        .iter()
        .any(|r| canonical(&r.path()) == canonical(&root));
    eprintln!(
        "listed among active sync roots: {listed}; registered: {:?}",
        id.is_registered()
    );

    let content = b"hello from a shoal placeholder".to_vec();
    let connection = Session::new()
        .connect(
            &root,
            Memory(HashMap::from([(b"hello.txt".to_vec(), content.clone())])),
        )
        .unwrap();

    PlaceholderFile::new("hello.txt")
        .metadata(Metadata::file().size(content.len() as u64))
        .blob(b"hello.txt".to_vec())
        .mark_in_sync()
        .create::<&Path>(&root)
        .unwrap();
    let file = root.join("hello.txt");
    let before = attributes(&file);
    eprintln!("attributes before reading: {before:#x}");
    assert_ne!(
        before & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        0,
        "a placeholder's data is not on disk"
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().len(),
        content.len() as u64
    );

    // Reading it calls the filter back for its data.
    assert_eq!(std::fs::read(&file).unwrap(), content);
    let after = attributes(&file);
    eprintln!("attributes after reading: {after:#x}");
    assert_eq!(
        after & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        0,
        "now hydrated"
    );

    drop(connection);
    id.unregister().unwrap();
    assert!(listed, "Windows lists the sync root");
}
