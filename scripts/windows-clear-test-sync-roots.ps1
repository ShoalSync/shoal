# Unregisters the sync roots tests left behind on a Windows runner: Shoal's own (IDs
# "Shoal!...") and the spikes' ("ShoalSpike!..."), when their folder is gone or is one tests
# make: under the temp directory, or in the profile as the tests name theirs ("Shoal daemon *",
# "Shoal provider *", "Shoal exit *"). A test that fails or is cancelled before it cleans
# up would otherwise leave one in Explorer's navigation pane for good. The registrations are read
# from the registry: Windows' own list of current sync roots leaves out those whose folder is
# gone. Run before and after the Windows jobs; it changes nothing else.
$ErrorActionPreference = 'Stop'
[void][Windows.Storage.Provider.StorageProviderSyncRootManager, Windows.Storage, ContentType = WindowsRuntime]
$manager = [Windows.Storage.Provider.StorageProviderSyncRootManager]
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$temp = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
$made = 'Shoal daemon ', 'Shoal provider ', 'Shoal exit ' |
    ForEach-Object { Join-Path $env:USERPROFILE $_ }
$registered = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager'
foreach ($key in Get-ChildItem $registered -ErrorAction SilentlyContinue) {
    $id = $key.PSChildName
    if (-not ($id -like 'Shoal*') -or -not ($id -like "*!$sid!*")) { continue }
    $path = $null
    try { $path = (Get-ItemProperty (Join-Path $key.PSPath 'UserSyncRoots') -ErrorAction Stop).$sid } catch {}
    $test = -not $path -or -not (Test-Path -LiteralPath $path) -or
        $path.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -or
        ($made | Where-Object { $path.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) })
    if (-not $test) { continue }
    try {
        $manager::Unregister($id)
        Write-Output "unregistered $id ($path)"
    } catch {
        Write-Output "cannot unregister $id ($path): $_"
    }
}
