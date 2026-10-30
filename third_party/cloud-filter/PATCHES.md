# cloud-filter: Shoal's patches

This is `cloud-filter` from <https://github.com/ho-229/cloud-filter-rs>, `main` at commit
`fb36bf4`: the version after 0.0.6, not yet released. It has every fix Shoal made to 0.0.6
(fixes 1 to 9 in the history of this file), merged upstream as pull requests
[#11](https://github.com/ho-229/cloud-filter-rs/pull/11),
[#12](https://github.com/ho-229/cloud-filter-rs/pull/12),
[#13](https://github.com/ho-229/cloud-filter-rs/pull/13) and
[#14](https://github.com/ho-229/cloud-filter-rs/pull/14), with the maintainer's own changes since
(owned callback data, checked native ranges, `PlaceholderFile` renamed `PlaceholderCreateInfo`,
and `SyncFilter::state_changed` taking a `StateWatchEvent`).

Shoal uses it through `[patch.crates-io]` in the workspace `Cargo.toml`. The copy sits
outside the workspace, so the workspace's `unsafe_code = "forbid"` does not apply to it. The
crate's tests, examples, dev-dependencies and `Cargo.lock` are left out.

On top of upstream there is one patch, an addition: custom item states, below. Its `src` is
exactly the branch `feat/item-properties` of <https://github.com/GraemeKnowles/cloud-filter-rs>,
sent upstream as <https://github.com/ho-229/cloud-filter-rs/pull/17>.

Drop this copy, and the `[patch.crates-io]` entry, once a release upstream has the addition.

`git log -p -- third_party/cloud-filter` shows the history: 0.0.6 as published, Shoal's fixes
to it (described in the commits up to the one that moved to upstream `main`), and then this.

## Custom item states (an addition, not a fix)

A provider can define states of its own, which Explorer shows as icons in the Status column,
with their values as tooltips: `StorageProviderItemPropertyDefinition` in the registration, and
`StorageProviderItemProperties.SetAsync` on an item. The crate exposed neither. Added:
`SyncRootInfo::add_item_property_definition` (and `with_`), `SyncRootInfo::
item_property_definitions`, and `properties::set_item_properties`, with the `windows` crate's
`implement` feature for the list it takes. Checked on Windows 11 25H2 by an unpackaged,
unelevated process, also registering again in place over a root without the definition.
Shoal shows storage priorities with it (ADR 0023). Upstream, it comes with a behavior test,
`custom_states`.

## Also observed, not patched

These are useful context for upstream, but they are not bugs in the crate:
- Operations a provider makes in its own process raise no callbacks: its own deletes and
  renames in the sync root, for example.
- `std::fs::metadata`, which follows the placeholder, does not show
  `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`. `symlink_metadata` does.
- `convert_to_placeholder` and `update` need a handle opened with
  `Placeholder::options().write_access().exclusive()`. With `Placeholder::options()` alone they
  fail with `HRESULT(0x80070006)` "The handle is invalid."
- Renaming an ordinary file over a dehydrated placeholder first hydrates the old placeholder,
  through `fetch_data`.
- Reporting progress during a fetch, or progressive hydration, keeps a fetch alive past 60 s.
- Another process renaming a plain folder of placeholders raises no callback. Renaming a
  placeholder folder raises one `rename` and one `renamed` for the folder.
- A placeholder moved into a hidden folder in the root, or out of the root, stays a placeholder,
  but is no longer in sync. Dehydrating it is then refused: "The file is not in sync with the
  cloud", or, out of the root, "only supported on files under a cloud sync root".
- `PlaceholderCreateInfo` (`PlaceholderFile` before upstream renamed it) refuses to create a
  folder placeholder with `has_no_children` and `mark_in_sync`. Converting a plain folder works.
