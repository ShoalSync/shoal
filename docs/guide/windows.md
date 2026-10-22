# Tidefell on Windows

On Windows, the [installer](install.md#windows) sets up the tray app, `tidefell-tray`, which
starts when you sign in. It runs the daemon, `tidefelld`, for you: it starts it, starts it again
if it stops, and stops it when you quit. Its icon is in the notification area; click it for the
menu.

## Setting up

The first time, the menu offers two choices:

- **Set up as my first device**, if Tidefell is new to you. This makes your *user key*,
  `%LOCALAPPDATA%\Tidefell\user.key`, which proves which devices are yours. Back it up somewhere
  safe: without it, no new device can join yours.
- **Join my other devices…**, if you already have one with your user key. A window shows a
  pairing code. On that device, choose **Add a device…** in its tray, or run
  `tidefell pair <code>`. Both then show a confirmation code: approve only if they match.

Either way the tray then starts Tidefell. Its data directory is `%LOCALAPPDATA%\Tidefell`, and
its configuration `%LOCALAPPDATA%\Tidefell\tidefelld.toml`; see
[Configuration](configuration.md).

## Folders

Folders are made and joined with the command-line client, in a new terminal (the installer puts
it on your `PATH`):

```powershell
tidefell folder create "$env:USERPROFILE\Photos" --partial 20000000000
tidefell folder join <folder id> "$env:USERPROFILE\Documents\Work" --partial 5000000000
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
provider's cloud folder, such as a Documents folder OneDrive keeps. Tidefell still syncs one that
is, without online-only files, and its log says why.

## The tray's menu

- What Tidefell is doing, and each folder, which opens in Explorer.
- **Pause syncing** and **Resume syncing**, for the whole device.
- **Add a device…**: approve a new device from the pairing code it shows. Only a device that
  holds your user key can.
- **Quit Tidefell** stops the daemon, after it has let go of the cloud folders.

## Good to know

- **Windows Firewall** may ask whether to let `tidefelld` accept connections, the first time it
  runs. Without that, connections to it go out from this device, or through a relay, which still
  works, if more slowly.
- **Logs** are in `%LOCALAPPDATA%\Tidefell` (see [When something goes wrong](troubleshooting.md));
  what the daemon said if it could not start at all is in `tidefelld.stderr.log` there, and the
  tray shows it.
- `tidefelld run --config %LOCALAPPDATA%\Tidefell\tidefelld.toml` in a terminal runs the daemon
  by hand, for development; Ctrl-C stops it cleanly. The tray uses one already running.
