param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [switch]$SkipBuild,
    [switch]$NoPathUpdate,
    [string]$InstallDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Warning "install-cli797.ps1 is deprecated. Forwarding to install-cliclaw.ps1."

$forwardScript = Join-Path $PSScriptRoot "install-cliclaw.ps1"
if (-not (Test-Path -LiteralPath $forwardScript)) {
    throw "Could not find install-cliclaw.ps1 next to install-cli797.ps1."
}

& $forwardScript @PSBoundParameters
