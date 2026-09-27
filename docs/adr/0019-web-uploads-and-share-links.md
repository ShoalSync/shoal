# 0019: The web peer uploads files and shares links

- Status: accepted
- Date: 2026-09-24

## Context

The web peer (ADRs 0010–0012) lets an account browse, download and restore files. It is the
only way into a folder from a device without a Tidefell client, such as iOS, Windows, a work
computer or a borrowed machine. It cannot add files, and it cannot share a file with someone
who is not a Tidefell user, which is the most common reason people reach for a hosted sync
service.

## Decision

- **Uploads.** Signed-in accounts can upload files to any folder they can see, and create
  folders. Uploads are recorded with the engine's `write`, so they sync like any local
  change. Large uploads stream to a staging file in the folder's reserved directory.
- **Share links.** An account can create a link to a file or a directory. A link is a random
  128-bit token in the URL. It can have an expiry, a password (argon2id) and a download
  limit. It gives read-only access, with no account and no view of anything outside what
  was shared. A directory link can instead be upload-only (ADR 0029). Links are stored in the web peer's data directory, listed per account, and
  revocable.
- **Link pages** are the same server-rendered UI with a reduced layout. A directory link
  lists its contents and lets visitors download single files. A placeholder is fetched on
  demand before download, as for a signed-in account.
- **Limits.** Share links are off unless `[web] share_links = true`. Every link access is
  logged at `info` with the link ID, not the token.

## Consequences

- A NAS or self-run server running the web peer covers everyday Dropbox use: getting a file
  onto a phone without a client, and sending one to someone.
- Links are only as available as the web peer's host. On a home NAS that needs port
  forwarding or a tunnel, which the docs explain.
- Share links are a web peer feature and never touch the sync protocol.
