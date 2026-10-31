# Builds stremio-rune-setup.exe: the release app first, then Setup with that exe inside.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
cargo build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$env:STREMIO_RUNE_PAYLOAD = "$root\target\release\stremio-rune.exe"
cargo build --release --manifest-path "$PSScriptRoot\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Copy-Item "$PSScriptRoot\target\release\stremio-rune-setup.exe" "$root\target\release\stremio-rune-setup.exe"
Write-Host "$root\target\release\stremio-rune-setup.exe"
