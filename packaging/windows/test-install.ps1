# Installs the MSI given, as the signed-in user, checks what it put where, and uninstalls it
# again (ADR 0042), as the CI runner does. It does not start the programs: their files are
# whatever the MSI holds. Each msiexec run has five minutes; a log is printed if one fails.
param([Parameter(Mandatory)] [string] $Msi)
$ErrorActionPreference = 'Stop'
$msi = (Resolve-Path $Msi).Path
$logs = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Shoal'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Shoal.lnk'

function Invoke-Msiexec([string] $What, [string[]] $Arguments) {
    $log = Join-Path $logs "$What.log"
    $process = Start-Process msiexec.exe -ArgumentList ($Arguments + @('/qn', '/l*v', "`"$log`"")) -PassThru
    if (-not $process.WaitForExit(300000)) {
        $process.Kill()
        if (Test-Path $log) { Get-Content $log -Tail 60 }
        throw "msiexec ($What) did not finish in five minutes"
    }
    if ($process.ExitCode -ne 0) {
        if (Test-Path $log) { Get-Content $log -Tail 60 }
        throw "msiexec ($What) failed with $($process.ExitCode)"
    }
}

# What an earlier run that failed left on the user's PATH goes first: the runner is not
# ephemeral.
$path = [Environment]::GetEnvironmentVariable('PATH', 'User')
$kept = ($path -split ';' | Where-Object { $_ -and $_ -notlike '*Programs\Shoal*' }) -join ';'
if ($kept -ne $path) {
    "removing an earlier run's PATH entry"
    [Environment]::SetEnvironmentVariable('PATH', $kept, 'User')
}

Invoke-Msiexec 'install' @('/i', "`"$msi`"")
try {
    foreach ($file in 'shoal-tray.exe', 'shoald.exe', 'shoal.exe', 'LICENSE-MIT.txt', 'LICENSE-APACHE.txt', 'README.md') {
        if (-not (Test-Path (Join-Path $dir $file))) { throw "missing $dir\$file" }
    }
    $run = (Get-ItemProperty $runKey -Name Shoal -ErrorAction SilentlyContinue).Shoal
    if ($run -notlike '*shoal-tray.exe*') { throw "no Run entry: $run" }
    $path = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if ($path -notlike '*Programs\Shoal*') { throw "not on the user's PATH: $path" }
    if (-not (Test-Path $shortcut)) { throw "no Start menu shortcut" }
    'installed'

    # Installing again repairs it in place.
    Invoke-Msiexec 'reinstall' @('/i', "`"$msi`"")
    if (-not (Test-Path (Join-Path $dir 'shoald.exe'))) { throw "gone after installing again" }

} catch {
    # Not left installed for the next run.
    Invoke-Msiexec 'cleanup' @('/x', "`"$msi`"")
    throw
}

Invoke-Msiexec 'uninstall' @('/x', "`"$msi`"")
if (Test-Path $dir) { throw "$dir is still there" }
if (Get-ItemProperty $runKey -Name Shoal -ErrorAction SilentlyContinue) { throw "Run entry left" }
if (Test-Path $shortcut) { throw "shortcut left" }
$path = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($path -like '*Programs\Shoal*') { throw "still on the user's PATH: $path" }
'uninstalled'
