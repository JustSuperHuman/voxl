param([string]$Version = '0.1.0')
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$') { throw 'Invalid package version.' }
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskBundle = Join-Path ([IO.Path]::GetTempPath()) ("bro-rmm-source-$Version-"+[guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $taskBundle) { throw "Source bundle directory already exists: $taskBundle" }
New-Item -ItemType Directory -Path $taskBundle | Out-Null
$taskSource = Join-Path $taskBundle 'voxl'
New-Item -ItemType Directory -Path $taskSource | Out-Null
Copy-Item -LiteralPath (Join-Path $taskRoot 'Cargo.toml'),(Join-Path $taskRoot 'Cargo.lock'),(Join-Path $taskRoot 'build.rs'),(Join-Path $taskRoot 'LICENSE'),(Join-Path $taskRoot 'BRO-RMM.md'),(Join-Path $taskRoot 'README.md'),(Join-Path $taskRoot 'package.json'),(Join-Path $taskRoot 'package-lock.json') -Destination $taskSource
foreach ($taskPart in @('src','web','packaging','examples')) {
    if (Test-Path -LiteralPath (Join-Path $taskRoot $taskPart)) { Copy-Item -LiteralPath (Join-Path $taskRoot $taskPart) -Destination $taskSource -Recurse }
}
Push-Location $taskRoot
try {
    New-Item -ItemType Directory -Path (Join-Path $taskSource '.cargo') -Force | Out-Null
    & cargo vendor --locked --versioned-dirs (Join-Path $taskSource 'vendor') 2> (Join-Path $taskBundle 'cargo-vendor.log') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Rust dependency source collection failed.' }
    @'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
'@ | Set-Content -LiteralPath (Join-Path $taskSource '.cargo\config.toml') -Encoding utf8NoBOM
} finally { Pop-Location }
if (-not $env:VCPKG_ROOT) { throw 'VCPKG_ROOT is required.' }
$taskDownloads = Join-Path $env:VCPKG_ROOT 'downloads'
$taskRequiredArchives = @('videolan-x264-*.tar.gz','libyuv-*.tar.gz','xiph-opus-*.tar.gz','libjpeg-turbo-libjpeg-turbo-*.tar.gz','randy408-libspng-*.tar.gz','madler-zlib-*.tar.gz')
$taskMissingArchives = @($taskRequiredArchives | Where-Object { -not (Get-ChildItem -LiteralPath $taskDownloads -Filter $_ -File) })
if ($taskMissingArchives.Count -gt 0) {
    # A binary cache can restore a library without fetching its source archive.
    # Resolve the sources into an empty install state before packaging them.
    $taskDownloadState = Join-Path ([IO.Path]::GetTempPath()) ('bro-rmm-native-sources-'+[guid]::NewGuid().ToString('N'))
    & (Join-Path $env:VCPKG_ROOT 'vcpkg.exe') install x264 libyuv opus --triplet x64-windows-static-md --only-downloads --binarysource=clear "--x-install-root=$taskDownloadState"
    if ($LASTEXITCODE -ne 0) { throw 'Native dependency source collection failed.' }
}
foreach ($taskPattern in $taskRequiredArchives) {
    if (-not (Get-ChildItem -LiteralPath $taskDownloads -Filter $taskPattern -File)) { throw "Missing corresponding source: $taskPattern" }
}
$taskNative = Join-Path $taskBundle 'native'
New-Item -ItemType Directory -Path $taskNative | Out-Null
foreach ($taskPort in @('x264','libyuv','opus','libjpeg-turbo','libspng','zlib','vcpkg-make','vcpkg-cmake','vcpkg-cmake-config','vcpkg-cmake-get-vars')) {
    Copy-Item -LiteralPath (Join-Path $env:VCPKG_ROOT "ports\$taskPort") -Destination $taskNative -Recurse
}
Get-ChildItem -LiteralPath $taskDownloads -File | Where-Object { $_.Name -match '\.(tar\.gz|tar\.xz|tgz)$' } | Copy-Item -Destination $taskNative
Get-ChildItem -LiteralPath $taskNative -File | ForEach-Object { $taskSourceHash=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); "$taskSourceHash  $($_.Name)" } | Set-Content -LiteralPath (Join-Path $taskNative 'SHA256SUMS.sources') -Encoding ascii
$taskVcpkgCommit = & git -C $env:VCPKG_ROOT rev-parse HEAD
$taskRustVersion = & rustc --version
@"
Bro RMM $Version corresponding source

Voxl source, Rust dependency sources and native library source archives are included.
The source retains each project's license notices. The linked executable is GPL-2.0.

Build tools: $taskRustVersion; Microsoft C++ Build Tools; LLVM; Node.js and esbuild.
vcpkg revision: $taskVcpkgCommit
Triplet: x64-windows-static-md

To rebuild: clone https://github.com/microsoft/vcpkg, check out the revision above,
run bootstrap-vcpkg.bat, and set VCPKG_ROOT to that checkout. Run
voxl/packaging/build-windows.ps1. Cargo uses the vendored sources in voxl/vendor.
Native source archives and the exact vcpkg port recipes are in native/.
The Windows runtime and system libraries are supplied by Windows.
"@ | Set-Content -LiteralPath (Join-Path $taskBundle 'BUILD.txt') -Encoding utf8NoBOM
$taskOut = Join-Path $taskRoot 'dist'
New-Item -ItemType Directory -Path $taskOut -Force | Out-Null
Compress-Archive -Path (Join-Path $taskBundle '*') -DestinationPath (Join-Path $taskOut 'bro-rmm-source.zip') -CompressionLevel Optimal -WarningAction SilentlyContinue
$taskHash = (Get-FileHash -LiteralPath (Join-Path $taskOut 'bro-rmm-source.zip') -Algorithm SHA256).Hash.ToLowerInvariant()
"$taskHash  bro-rmm-source.zip" | Add-Content -LiteralPath (Join-Path $taskOut 'SHA256SUMS') -Encoding ascii
