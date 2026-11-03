param([switch]$SkipTests)
$ErrorActionPreference = 'Stop'
if (-not $env:VCPKG_ROOT) { throw 'Set VCPKG_ROOT to your vcpkg checkout.' }
$env:VCPKGRS_TRIPLET = 'x64-windows-static-md'
$env:ESBUILD = (Join-Path $PSScriptRoot '..\node_modules\.bin\esbuild.cmd')
Push-Location (Join-Path $PSScriptRoot '..')
try {
    & (Join-Path $env:VCPKG_ROOT 'vcpkg.exe') install x264 libyuv opus --triplet $env:VCPKGRS_TRIPLET
    if ($LASTEXITCODE -ne 0) { throw 'Native dependency build failed.' }
    if (-not $env:LIBCLANG_PATH) {
        $taskNativeClang=Join-Path $env:VCPKG_ROOT 'downloads\tools\clang\clang-21.1.8\bin'
        $env:LIBCLANG_PATH=if (Test-Path -LiteralPath (Join-Path $taskNativeClang 'libclang.dll')) { $taskNativeClang } else { 'C:\Program Files\LLVM\bin' }
    }
    & npm install --no-save esbuild@0.28.2
    if ($LASTEXITCODE -ne 0) { throw 'esbuild installation failed.' }
    if (-not $SkipTests) {
        & cargo test --locked
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
    }
    & cargo build --locked --release --bin voxl
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    & (Join-Path $PSScriptRoot 'package.ps1')
} finally { Pop-Location }
