# Tidefell

Tidefell is open-source, peer-to-peer file sync with the feel of Dropbox, OneDrive or
Nextcloud, but with no central server.

- **Every device is a peer.** Some run as *full replicas* that keep everything (a NAS, a home
  server); others run as *partial replicas* that keep metadata and fetch content on demand.
- **Swarm downloads.** Files download from many peers at once, verified chunk by chunk.
- **Always on, without a central server.** An [always-on peer](guide/always-on.md) on a
  machine you control keeps devices in sync when they are rarely online together.
- **The web interface is just another peer**, one that adds accounts and sessions, uploads, and
  [share links](guide/web.md#share-links).
- **Deletes are recoverable.** Deleted files go to a recycle bin and are kept for a
  [retention period](guide/recycle-bin.md#retention) each device chooses per folder.

This book covers installing and running Tidefell, self-hosting it, the protocols it speaks,
and how the project is organised. Terms in *italics* are defined in the
[glossary](glossary.md).

> **Status:** early development. There is no stability promise for wire or on-disk formats
> yet. See [known limitations](guide/install.md#known-limitations).

## Where things live

These pages are built from the `docs/` directory of the
[monorepo](https://github.com/tidefell/tidefell). Each release publishes the book built from
its tag, so the documentation always matches the version it describes.
