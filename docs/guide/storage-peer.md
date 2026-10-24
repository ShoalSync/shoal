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

A storage peer is `shoald` with a `[storage]` section. It is its operator's device, certified
by the operator's own user, who is not a member of the folders it stores.

1. On the server, install Shoal and run `shoald init`. Note the **device ID** that
   `shoald id` prints.
2. Ask the folder's members for the folder ID and their **user IDs** (`shoald id` on any of
   their devices).
3. List the folder in the server's configuration, with its members and a quota, and restart
   `shoald`:

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
   shoal folder store-with <folder id> <storage peer's device id>
   shoal peer add <storage peer's device id> --addr <host:7777>
   ```

   A folder made with `--storage-peer <device id>` has it from the start.

A storage peer stores only folders that are encrypted, and only those listed in its
configuration. A device either syncs a folder or stores it, never both.

### On NixOS

The infrastructure flake's `role-storage-peer` runs `shoald` as a storage peer and opens UDP
7777; stored folders are set per host:

```nix
{
  imports = [ infra.nixosModules.role-storage-peer ];
  services.shoald.settings.storage.folders = [
    {
      id = "<folder id>";
      members = [ "<user id>" ];
      quota_bytes = 100000000000;
    }
  ];
}
```

## Day to day

`shoal storage` on the server lists the folders it stores: how many versions, how many
replaced versions it keeps, its size against the quota, and how many chunks it is still fetching.
`shoal status` shows the same for each stored folder.

- It keeps the newest version of every file, and versions that later ones replaced, deletes
  included, for `retention_days`. After that it drops them, and the chunks only they used.
- Once a folder reaches its quota, it fetches nothing more for it and says so. It never drops a
  current version to make room: members then see their copy there as incomplete, not lost. Raise
  the quota and restart to let it catch up.
- A total across all stored folders can be set as well, with an order between them: under the
  total, a folder fetches only while none ranked above it still lacks what a connected member
  could send. Each folder's own quota still holds.

  ```toml
  [storage]
  quota_bytes = 500000000000   # 500 GB for all stored folders together

  [[storage.folders]]
  id = "<folder id>"
  members = ["<user id>"]
  quota_bytes = 300000000000
  priority = 200               # 0 to 255, higher filled first; 128 if not set
  ```
- Members that are never online together sync through it: one leaves its changes there, the next
  picks them up, and a new device of a member gets the folder's key, granted by another member,
  the same way.

## What it keeps first

When a storage peer runs short of room, the folder's members decide what it keeps first, by
ranking files from 0 to 255 (128 unless a rule says): it fills its quota highest first, and never
stores a file ranked 0. The rules are in the folder's `.shoalpriorities`, which syncs to every
member device like any file; a member device holding the folder's key gives the storage peer the
priority of each file, by its ID only, never its name.

```sh
shoal folder priority <folder id> Photos/ 200      # keep photos first
shoal folder priority <folder id> '*.iso' 0        # never store disc images there
shoal folder priority <folder id> Photos/ --clear  # back to 128
shoal folder priorities <folder id>
```

On Windows the tray's **Storage…** window does the same, and shows what each storage peer keeps
([Windows](windows.md#what-storage-peers-keep)). Patterns are as in `.shoalignore`, and the
last rule that matches a file wins. A device
without the file on disk, as a partial replica may be, fetches it to read and edit it. The storage
peer learns which files are ranked above others, though not which files they are.

`shoal folder storage <folder id>` shows, for each storage peer, what it holds and the room
it has left, and for each priority how much of its files it keeps:

```text
<device id>: holds 3.0 GiB, room for 0 B more
  priority 200: 120 file(s), 2.0 GiB: complete
  priority 128: 4 file(s), 12.0 KiB: complete
  priority  50: 30 file(s), 2.0 GiB: 50%, 1.0 GiB not stored
  priority   0: 2 file(s), 8.0 GiB: never stored there
```

A storage peer never drops what it holds to make room: a file it already has stays when its
rank falls, and one ranked higher waits. To make room, ask it to forget what is ranked low:

```sh
shoal folder storage-drop <folder id> <storage peer's device id> --below 100
```

It asks first, and drops nothing if a file would go that has no copy known anywhere else, on
this device or confirmed by another, listing those files; `--force` drops them anyway. What it
drops it fetches again, highest priority first, whenever it has room.

## Sizing

Besides disk for the sealed chunks, a storage peer holds the versions it keeps in memory: about
1.2 KB each, replaced versions included until `retention_days` drops them. A folder of 100,000
files with a version each needs about 120 MB; ten such folders, about 1.2 GB. A later release
reads them from disk instead (ADR 0023).

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
