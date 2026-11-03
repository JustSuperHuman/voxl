param([string]$Executable = 'C:\tmp\bro-rmm-target\release\voxl.exe', [int]$Port = 16901)
$ErrorActionPreference = 'Stop'
if ($Port -lt 1024 -or $Port -gt 65535) { throw 'Use an unprivileged local port.' }
$taskProbeDir = Join-Path ([IO.Path]::GetTempPath()) ('bro-rmm-readonly-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskProbeDir | Out-Null
$taskProbeExe = Join-Path $taskProbeDir 'voxl.exe'
Copy-Item -LiteralPath $Executable -Destination $taskProbeExe
$env:VOXL_PASSWORD = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32)).ToLowerInvariant()
$taskRecord = Join-Path $env:USERPROFILE '.bro\rmm\transport-probe.json'
New-Item -ItemType Directory -Path (Split-Path -Parent $taskRecord) -Force | Out-Null
@{httpBase="http://127.0.0.1:$Port";token=$env:VOXL_PASSWORD;readOnly=$true;executable=$taskProbeExe} | ConvertTo-Json | Set-Content -LiteralPath $taskRecord -Encoding utf8NoBOM
$taskAccount = '{0}\{1}' -f $env:USERDOMAIN,$env:USERNAME
& icacls.exe $taskRecord /inheritance:r /grant:r ('{0}:(F)' -f $taskAccount) '*S-1-5-18:(F)' | Out-Null
Write-Output "Read-only loopback probe. Private connection record: $taskRecord"
& $taskProbeExe '--addr=127.0.0.1' "--port=$Port" '--read-only=true' '--audio-enabled=false' '--file-transfers=none' '--enable-resize=false' '--stripes=1' '--framerate=30' '--max-kbps=8000' '--min-kbps=1000' '--title=Bro-RMM-Read-only-Probe'
exit $LASTEXITCODE
