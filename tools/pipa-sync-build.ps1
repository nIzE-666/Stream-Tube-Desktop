<#
.SYNOPSIS
    Sync, build (Snapdragon 870 ARMv8.2-A optimized), strip and atomically install Stream Tube from the local `pipa` git branch onto Xiaomi Pad 6 (pipa).

.DESCRIPTION
    Workflow for future upstream updates:
      1. git fetch origin --tags
      2. git checkout pipa && git rebase v0.7.x-alpha
      3. .\tools\pipa-sync-build.ps1

    - Uses `git archive HEAD` so only clean committed sources are uploaded (preserving `target/` on pipa for fast incremental builds).
    - Compiles inside `/userdata/naomi/devroot` using an isolated `unshare -r -m` mount namespace (no sudo, no lingering mounts).
    - Targets Snapdragon 870 (SM8250, Cortex-A55 + Cortex-A77) common ARMv8.2-A ISA features (+lse,+dotprod,+fp16,+rcpc,+crc).
    - Strips symbols and atomically swaps `~/.local/share/streamtube-app/streamtube`.
#>
[CmdletBinding()]
param(
    [string]$SshHost = "pipa-wifi",
    [string]$RemoteDir = "/home/phablet/src/Stream-Tube-Desktop",
    [int]$Jobs = 3,
    [string]$RustFlags = "-C target-feature=+v8.2a,+lse,+dotprod,+fp16,+rcpc,+crc",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $RepoRoot

function Invoke-Checked {
    param([scriptblock]$Action, [string]$StepName)
    & $Action
    if ($LASTEXITCODE -ne 0) {
        throw "Step '$StepName' failed with exit code $LASTEXITCODE"
    }
}

try {
    $status = git status --porcelain
    if ($status) {
        Write-Warning "Working tree has uncommitted changes; archiving HEAD commit only. Commit first if you want them included."
    }

    $commit = (git rev-parse --short HEAD).Trim()
    $version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    Write-Host "==> Stream Tube v$version (commit $commit) -> $SshHost" -ForegroundColor Cyan

    $tmpTar = Join-Path $env:TEMP "streamtube-pipa-$commit.tar.gz"
    if (Test-Path $tmpTar) { Remove-Item -Force $tmpTar }

    Write-Host "==> Archiving HEAD..."
    Invoke-Checked -StepName "git archive" -Action {
        git archive --format=tar.gz -o $tmpTar HEAD
    }
    $sizeKB = [math]::Round((Get-Item $tmpTar).Length / 1KB, 1)
    Write-Host "    Archive ready: $sizeKB KB"

    Write-Host "==> Uploading to ${SshHost}:/tmp/streamtube-src.tar.gz..."
    Invoke-Checked -StepName "scp source archive" -Action {
        scp -q $tmpTar "${SshHost}:/tmp/streamtube-src.tar.gz"
    }
    Remove-Item -Force $tmpTar

    $vendorTree = (git rev-parse HEAD:vendor).Trim()
    Write-Host "==> Syncing source tree on pipa (preserving target/ and unchanged vendor/)..."
    $syncScript = @"
set -eu
mkdir -p "$RemoteDir"
OLD_VENDOR=""
if [ -f "$RemoteDir/.vendor-tree" ]; then OLD_VENDOR=`$(cat "$RemoteDir/.vendor-tree"); fi
if [ "`$OLD_VENDOR" = "$vendorTree" ] && [ -d "$RemoteDir/vendor" ]; then
    rm -rf "$RemoteDir/src" "$RemoteDir/assets" "$RemoteDir/tools"
    tar -xzf /tmp/streamtube-src.tar.gz --exclude='./vendor' --exclude='vendor' -C "$RemoteDir"
    echo "vendor/ unchanged ($vendorTree) -> fast incremental build"
else
    rm -rf "$RemoteDir/src" "$RemoteDir/vendor" "$RemoteDir/assets" "$RemoteDir/tools"
    tar -xzf /tmp/streamtube-src.tar.gz -C "$RemoteDir"
    printf '%s\n' "$vendorTree" > "$RemoteDir/.vendor-tree"
    echo "vendor/ synced ($vendorTree)"
fi
rm -f /tmp/streamtube-src.tar.gz
"@
    Invoke-Checked -StepName "remote source sync" -Action {
        $syncScript | ssh -T $SshHost "sed '1s/^\xef\xbb\xbf//' | tr -d '\r' | bash -s"
    }

    if ($SkipBuild) {
        Write-Host "==> -SkipBuild set, stopping after sync." -ForegroundColor Yellow
        return
    }

    Write-Host "==> Building release binary inside devroot (jobs=$Jobs, RUSTFLAGS='$RustFlags')..." -ForegroundColor Cyan
    $buildScript = @"
set -eu
mkdir -p /home/phablet/.local/share/streamtube-app
# Fetch any new crates on the host (where NetworkManager/VPN DNS works cleanly) before entering devroot.
(cd "$RemoteDir" && /home/phablet/.cargo/bin/cargo fetch)
unshare -r -m /bin/sh -c '
  set -eu
  mount --rbind /dev /userdata/naomi/devroot/dev
  mount --rbind /proc /userdata/naomi/devroot/proc
  mount --rbind /home/phablet /userdata/naomi/devroot/home/phablet
  mount --bind /tmp /userdata/naomi/devroot/tmp
  chroot /userdata/naomi/devroot /bin/sh -c "
    set -eu
    export HOME=/home/phablet
    export PATH=/home/phablet/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
    export RUSTFLAGS=\"$RustFlags\"
    cd \"$RemoteDir\"
    CARGO_BUILD_JOBS=$Jobs nice -n 10 cargo build --offline --release --bin streamtube
    cp \"$RemoteDir/target/release/streamtube\" /home/phablet/.local/share/streamtube-app/streamtube.new
    /usr/bin/strip --strip-unneeded /home/phablet/.local/share/streamtube-app/streamtube.new
  "
'
chmod 755 /home/phablet/.local/share/streamtube-app/streamtube.new
mv -f /home/phablet/.local/share/streamtube-app/streamtube.new /home/phablet/.local/share/streamtube-app/streamtube
ls -lh /home/phablet/.local/share/streamtube-app/streamtube
echo "INSTALLED_OK v$version ($commit)"
"@
    Invoke-Checked -StepName "remote devroot build & install" -Action {
        $buildScript | ssh -T $SshHost "sed '1s/^\xef\xbb\xbf//' | tr -d '\r' | bash -s"
    }
    Write-Host "==> Done! Stream Tube v$version ($commit) installed on $SshHost." -ForegroundColor Green
}
finally {
    Pop-Location
}
