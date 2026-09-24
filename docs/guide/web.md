# The web peer

A daemon can also serve a browser interface. It is a peer like any other: it syncs the folders
it has, and people sign in with a *web account* to see them. Each web account acts for one
Tidefell user and sees exactly the folders that user is a member of.

## Enabling it

Add a `[web]` section to the daemon's configuration:

```toml
[web]
bind = "127.0.0.1:8080"
secure_cookies = true
```

Serve it behind a TLS reverse proxy. Session cookies are marked `Secure` unless
`secure_cookies = false`, which is only meant for local testing over plain HTTP.

## Accounts

Accounts live in the web peer's data directory and are managed on that host:

```sh
tidefelld web account add --data-dir /var/lib/tidefell --username alice --admin
```

The password is read from standard input (at least 10 characters). `--user <user id>` links the
account to another user; by default it acts for the device's own user. Running `add` again for
an existing username changes its password. `tidefelld web account list` and `remove` do the
rest. Admin accounts can also manage accounts from the browser.

Passwords are hashed with argon2id. Changes take effect at the next sign-in, without restarting
the daemon.

## What people can do

- Browse folders and files, and download files. A placeholder is fetched from a connected peer
  first.
- Upload files, optionally into a new directory, which is created as needed. Uploads are
  recorded at once and sync to every device like any other change. Empty directories are not
  synced, so a directory appears once a file is uploaded into it.
- Open a folder's recycle bin and restore deleted files.
- Share a file or directory through a link, if share links are turned on (see below).
- Admins: add and remove accounts, and see and revoke everyone's share links.

Sessions expire after 12 hours. Every form is protected against cross-site request forgery.

## Share links

Share links let someone without an account download a file, or browse and download the files
in a directory. They are off unless the configuration turns them on:

```toml
[web]
bind = "127.0.0.1:8080"
share_links = true
```

Open a folder and choose **share** next to a file, or **Share the whole folder**. A link can
have:

- an expiry, in days;
- a password, which visitors enter once per browser session on that link;
- a download limit, counting each file downloaded through the link.

The link is shown once, when it is created. Only a hash of it is stored, in
`<data dir>/web/shares.json`, so that file cannot be used to recover working links. **Your share
links** lists your links with their download counts, and revokes them. Admin accounts see every
account's links.

A link gives read-only access to what was shared and nothing else. It stops working when it is
revoked or expires, when its download limit is reached, or when the account that created it is
removed or its user loses access to the folder. Every use of a link is logged at `info` with
the link's short ID, never its token.

Links are only as reachable as the web peer. On a home NAS that means port forwarding or a
tunnel to the reverse proxy; on a self-run server it works as it is.
