# Unregisters the sync roots tests left behind on a Windows runner: Tidefell's own (IDs
# "Tidefell!...") and the spikes' ("TidefellSpike!..."), when their folder is gone or is one tests
# make: under the temp directory, or the exit criterion's "Tidefell exit <run>" in the profile.
# A test that fails or is cancelled before it cleans up would otherwise leave one in Explorer's
# navigation pane for good. Run before and after the Windows jobs; it changes nothing else.
$ErrorActionPreference = 'Stop'
[void][Windows.Storage.Provider.StorageProviderSyncRootManager, Windows.Storage, ContentType = WindowsRuntime]
$manager = [Windows.Storage.Provider.StorageProviderSyncRootManager]
$temp = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
$exit = Join-Path $env:USERPROFILE 'Tidefell exit '
foreach ($root in $manager::GetCurrentSyncRoots()) {
    if (-not ($root.Id -like 'Tidefell*')) { continue }
    $path = $null
    try { $path = $root.Path.Path } catch {}
    $test = -not $path -or -not (Test-Path -LiteralPath $path) -or
        $path.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -or
        $path.StartsWith($exit, [StringComparison]::OrdinalIgnoreCase)
    if (-not $test) { continue }
    try {
        $manager::Unregister($root.Id)
        Write-Output "unregistered $($root.DisplayNameResource) ($path)"
    } catch {
        Write-Output "cannot unregister $($root.Id): $_"
    }
}
