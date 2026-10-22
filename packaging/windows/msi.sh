#!/usr/bin/env bash
# Builds the Windows installer with wixl (msitools): msi.sh <dir> <version> <out.msi>, where
# <dir> holds tidefell-tray.exe, tidefelld.exe, tidefell.exe, LICENSE and README.md. An MSI's
# version is three numbers, so a pre-release's suffix (as in 0.23.0-rc.1) is left out.
set -euo pipefail
dir=$1
version=${2%%-*}
out=$3
here=$(cd "$(dirname "$0")" && pwd)
wixl --arch x64 -D "Version=$version" -D "BinDir=$dir" -o "$out" "$here/tidefell.wxs"
