# A storage peer

An [always-on peer](always-on.md) keeps your devices in sync, but it holds your files in
plaintext, so it has to be a machine you trust. A *storage peer* is for a machine you do not: a
server at a hosting provider, or a friend's NAS. It keeps an [encrypted
folder](folders.md#encrypted-folders) for its members and serves it to them, but it holds no key:
it stores only sealed versions and chunks, and cannot read names or contents
([ADR 0023](../adr/0023-encrypted-storage-peers.md)).

## What it can and cannot see

A storage peer, or whoever runs or breaks into it, **can** see the folder's ID, its members'
user and device IDs, entry IDs and version vectors, which device made each version and when,
chunk counts and approximate sizes, and which versions share chunks. It can refuse to store or
serve, serve old versions, or delete what it holds.

It **cannot** read paths, names, contents or exact sizes, test whether the folder holds a file it
knows, or change anything without members noticing: every version is signed by the device that
made it and every chunk is authenticated, and members check both. The
[threat model](../security/threat-model.md) has the details.

## Setting one up

A storage peer is `tidefelld` with a `[storage]` section. It is its operator's device, certified
by the operator's own user, who is not a member of the folders it stores.

1. On the server, install Tidefell and run `tidefelld init`. Note the **device ID** that
   `tidefelld id` prints.
2. Ask the folder's members for the folder ID and their **user IDs** (`tidefelld id` on any of
   their devices).
3. List the folder in the server's configuration, with its members and a quota, and restart
   `tidefelld`:

   ```toml
   [[storage.folders]]
   id = "<folder id>"
   members = ["<user id>"]
   quota_bytes = 100000000000   # 100 GB of sealed chunks
   retention_days = 30          # how long replaced versions are kept; 0 keeps them forever
   ```

4. On the members' devices, add the storage peer to the encrypted folder, and as a peer if it is
   not found by address lookup:

   ```sh
   tidefell folder store-with <folder id> <storage peer's device id>
   tidefell peer add <storage peer's device id> --addr <host:7777>
   ```

   A folder made with `--storage-peer <device id>` has it from the start.

A storage peer stores only folders that are encrypted, and only those listed in its
configuration. A device either syncs a folder or stores it, never both.

### On NixOS

The infrastructure flake's `role-storage-peer` runs `tidefelld` as a storage peer and opens UDP
7777; stored folders are set per host:

```nix
{
  imports = [ infra.nixosModules.role-storage-peer ];
  services.tidefelld.settings.storage.folders = [
    {
      id = "<folder id>";
      members = [ "<user id>" ];
      quota_bytes = 100000000000;
    }
  ];
}
```

## Day to day

`tidefell storage` on the server lists the folders it stores: how many versions, how many
replaced versions it keeps, its size against the quota, and how many chunks it is still fetching.
`tidefell status` shows the same for each stored folder.

- It keeps the newest version of every file, and versions that later ones replaced, deletes
  included, for `retention_days`. After that it drops them, and the chunks only they used.
- Once a folder reaches its quota, it fetches nothing more for it and says so. It never drops a
  current version to make room: members then see their copy there as incomplete, not lost. Raise
  the quota and restart to let it catch up.
- Members that are never online together sync through it: one leaves its changes there, the next
  picks them up, and a new device of a member gets the folder's key, granted by another member,
  the same way.

## As a backup

A phone counts a file as safe elsewhere once a full replica confirms it holds it (see
[Folders](folders.md#is-it-safe-elsewhere)). A storage peer counts only while another member
device holds the folder's key: the key is never on the storage peer, so if the phone were the only
device with it, losing the phone would make everything the storage peer holds unreadable.

## Removing a member or a device

Removing a member, or revoking a lost device, makes the members' devices rotate the folder's key:
what is written afterwards is sealed with a key the removed member or the lost device never gets.
The storage peer needs no change for a revocation. For a removed member, drop their user ID from
its `members` too, so it stops syncing with their devices.
