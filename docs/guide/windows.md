# Shoal on Windows

On Windows, the [installer](install.md#windows) sets up the tray app, `shoal-tray`, which
starts when you sign in. It runs the daemon, `shoald`, for you: it starts it, starts it again
if it stops, and stops it when you quit. Its icon is in the notification area; click it for the
menu.

## Setting up

The first time, the menu offers two choices:

- **Set up as my first device**, if Shoal is new to you. This makes your *user key*,
  `%LOCALAPPDATA%\Shoal\user.key`, which proves which devices are yours. Back it up somewhere
  safe: without it, no new device can join yours.
- **Join my other devices…**, if you already have one with your user key. A window shows a
  pairing code. On that device, choose **Add a device…** in its tray, or run
  `shoal pair <code>`. Both then show a confirmation code: approve only if they match.

Either way the tray then starts Shoal. Its data directory is `%LOCALAPPDATA%\Shoal`, and
its configuration `%LOCALAPPDATA%\Shoal\shoald.toml`; see
[Configuration](configuration.md).

## Folders

Folders are made and joined with the command-line client, in a new terminal (the installer puts
it on your `PATH`):

```powershell
shoal folder create "$env:USERPROFILE\Photos" --partial 20000000000
shoal folder join <folder id> "$env:USERPROFILE\Documents\Work" --partial 5000000000
```

A *partial* folder (`--partial`, with how many bytes of other devices' files to keep here) is
a cloud folder in Explorer, as OneDrive's are:

- Every file is listed, with its size and date. Those not on this device are online-only: the
  cloud icon in Explorer's **Status** column.
- Opening one downloads it from your other devices first. It fails, with Windows' "the cloud
  provider is offline" message, if none of them that has it is connected.
- **Always keep on this device** (right-click) pins a file or folder: it is downloaded now and
  never freed. **Free up space** makes a file online-only again.
- When the files kept here outgrow the budget, the least recently used are freed the same way.
- Deleting or renaming files in Explorer deletes or renames them on your other devices.

A folder without `--partial` keeps every file on this device, as on any other.

A partial folder must be on a local NTFS drive, and not inside a hidden or system folder
(Explorer does not show cloud folders there: not under `AppData`, for one), nor inside another
provider's cloud folder, such as a Documents folder OneDrive keeps. Shoal still syncs one that
is, without online-only files, and its log says why.

## The tray's menu

- What Shoal is doing, and each folder: **Open in Explorer**, and, for an encrypted folder
  with storage peers, **Storage…** (below).
- **Pause syncing** and **Resume syncing**, for the whole device.
- **Add a device…**: approve a new device from the pairing code it shows. Only a device that
  holds your user key can.
- **Quit Shoal** stops the daemon, after it has let go of the cloud folders.

## What storage peers keep

An encrypted folder's **Storage…** window ranks what its [storage peers](storage-peer.md) keep
when they run short of room. On the left is the folder's tree, each file and folder with its
priority, 0 to 255: higher is kept first, 0 is never stored there, and 128 is the default. Choose
one, type a priority and **Set**; a folder's priority covers what is in it, unless something in
it has its own. **Use the default** removes the one set there. The rules are kept in the
folder's `.shoalpriorities`, so your other devices rank the same.

On the right is what each storage peer holds, and for each priority how much of it. A storage
peer never drops anything to make room; to make room for what is ranked higher, choose it, a
priority, and **Drop…**: it asks first, and again, listing them, if a file would go that has no
copy known anywhere else.

Explorer shows the same in its **Status** column, beside each file's sync state: a star for a
file kept first, a downward triangle for one kept after others, and a red cross for one never
stored on storage peers; files at the default show nothing. Hover over the icon for its
priority. Explorer only shows them; they are set in the Storage window.

## Good to know

- **Windows Firewall** may ask whether to let `shoald` accept connections, the first time it
  runs. Without that, connections to it go out from this device, or through a relay, which still
  works, if more slowly.
- **Logs** are in `%LOCALAPPDATA%\Shoal` (see [When something goes wrong](troubleshooting.md));
  what the daemon said if it could not start at all is in `shoald.stderr.log` there, and the
  tray shows it.
- `shoald run --config %LOCALAPPDATA%\Shoal\shoald.toml` in a terminal runs the daemon
  by hand, for development; Ctrl-C stops it cleanly. The tray uses one already running.
