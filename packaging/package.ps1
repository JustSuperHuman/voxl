param([string]$Version = '0.1.0')
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$') { throw 'Invalid package version.' }
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskOut = Join-Path $taskRoot 'dist'
$taskPayload = Join-Path $taskOut "bro-rmm-$Version-windows-x86_64"
if (Test-Path -LiteralPath $taskPayload) { throw "Package directory already exists: $taskPayload" }
New-Item -ItemType Directory -Path $taskPayload -Force | Out-Null
$taskExe = Join-Path $taskRoot 'target\release\voxl.exe'
Copy-Item -LiteralPath $taskExe -Destination (Join-Path $taskPayload 'voxl.exe')
$taskHash = (Get-FileHash -LiteralPath $taskExe -Algorithm SHA256).Hash.ToLowerInvariant()
@{schemaVersion=1;id='bro-rmm';version=$Version;protocolVersion=1;platform='windows-x86_64';executable='voxl.exe';sha256=$taskHash;transports=@('webrtc','websocket')} |
    ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $taskPayload 'manifest.json') -Encoding utf8NoBOM
Copy-Item -LiteralPath (Join-Path $taskRoot 'LICENSE'),(Join-Path $taskRoot 'BRO-RMM.md') -Destination $taskPayload
$taskLicenses = Join-Path $taskPayload 'licenses'
New-Item -ItemType Directory -Path $taskLicenses | Out-Null
if (-not $env:VCPKG_ROOT) { throw 'VCPKG_ROOT is required to package dependency licenses.' }
$taskShare = Join-Path $env:VCPKG_ROOT 'installed\x64-windows-static-md\share'
Get-ChildItem -LiteralPath $taskShare -Directory | ForEach-Object {
    $taskCopyright = Join-Path $_.FullName 'copyright'
    if (Test-Path -LiteralPath $taskCopyright) { Copy-Item -LiteralPath $taskCopyright -Destination (Join-Path $taskLicenses ($_.Name + '.txt')) }
}
Copy-Item -LiteralPath (Join-Path $taskRoot 'packaging\GPL-2.0.txt') -Destination $taskLicenses
Copy-Item -LiteralPath (Join-Path $taskRoot 'packaging\INTER-LICENSE.txt') -Destination $taskLicenses
$taskMetadata = & cargo metadata --locked --format-version 1 --manifest-path (Join-Path $taskRoot 'Cargo.toml') | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Could not collect Rust dependency notices.' }
foreach ($taskPackage in $taskMetadata.packages) {
    $taskPackageDir = Split-Path -Parent $taskPackage.manifest_path
    $taskNoticeFiles = @(Get-ChildItem -LiteralPath $taskPackageDir -File | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)' })
    if ($taskNoticeFiles.Count -gt 0) {
        $taskNoticeDir = Join-Path $taskLicenses ("rust-"+$taskPackage.name+'-'+$taskPackage.version)
        New-Item -ItemType Directory -Path $taskNoticeDir | Out-Null
        $taskNoticeFiles | Copy-Item -Destination $taskNoticeDir
    }
}
$taskMetadata.packages | Sort-Object name,version | ForEach-Object { "$($_.name) $($_.version): $($_.license)" } | Set-Content -LiteralPath (Join-Path $taskLicenses 'rust-dependencies.txt') -Encoding utf8NoBOM
$taskZip = Join-Path $taskOut 'bro-rmm-windows-x86_64.zip'
Compress-Archive -Path (Join-Path $taskPayload '*') -DestinationPath $taskZip -CompressionLevel Optimal -WarningAction SilentlyContinue
$taskZipHash = (Get-FileHash -LiteralPath $taskZip -Algorithm SHA256).Hash.ToLowerInvariant()
"$taskZipHash  bro-rmm-windows-x86_64.zip" | Set-Content -LiteralPath (Join-Path $taskOut 'SHA256SUMS') -Encoding ascii
Write-Output "Package: $taskZip"
Write-Output "Executable SHA256: $taskHash"
Write-Output "Archive SHA256: $taskZipHash"
