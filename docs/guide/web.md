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

- Browse folders and files, and download files stored on the web peer.
- Open a folder's recycle bin and restore deleted files.
- Admins: add and remove accounts.

Sessions expire after 12 hours. Every form is protected against cross-site request forgery.
