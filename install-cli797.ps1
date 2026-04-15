param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [switch]$SkipBuild,
    [switch]$NoPathUpdate,
    [string]$InstallDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-NormalizedPath {
    param([Parameter(Mandatory = $true)][string]$PathValue)

    return [System.IO.Path]::GetFullPath($PathValue).TrimEnd('\')
}

function Test-PathEntryPresent {
    param(
        [Parameter(Mandatory = $true)][string]$PathList,
        [Parameter(Mandatory = $true)][string]$Candidate
    )

    if ([string]::IsNullOrWhiteSpace($PathList)) {
        return $false
    }

    $normalizedCandidate = Resolve-NormalizedPath -PathValue $Candidate
    foreach ($entry in ($PathList -split ';')) {
        if ([string]::IsNullOrWhiteSpace($entry)) {
            continue
        }

        $normalizedEntry = Resolve-NormalizedPath -PathValue $entry
        if ($normalizedEntry.Equals($normalizedCandidate, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }

    return $false
}

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$rustDir = Join-Path $repoRoot "rust"
$cargoToml = Join-Path $rustDir "Cargo.toml"

if (-not (Test-Path -LiteralPath $cargoToml)) {
    throw "Could not find rust/Cargo.toml next to install-cli797.ps1."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
        $InstallDir = Join-Path $env:CARGO_HOME "bin"
    } elseif (Test-Path -LiteralPath (Join-Path $HOME ".cargo\bin")) {
        $InstallDir = Join-Path $HOME ".cargo\bin"
    } else {
        $InstallDir = Join-Path $HOME ".local\bin"
    }
}

$installDirFull = Resolve-NormalizedPath -PathValue $InstallDir
$targetDir = Join-Path $rustDir ("target\" + $Profile)
$sourceBinary = Join-Path $targetDir "cli797.exe"
$destinationBinary = Join-Path $installDirFull "cli797.exe"

Write-Host "Repo root:   $repoRoot"
Write-Host "Rust dir:    $rustDir"
Write-Host "Profile:     $Profile"
Write-Host "Install dir: $installDirFull"

if (-not $SkipBuild) {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        throw "cargo was not found on PATH. Install Rust from https://rustup.rs/ first."
    }

    $buildArgs = @("build", "--package", "rusty-claude-cli", "--bin", "cli797")
    if ($Profile -eq "release") {
        $buildArgs += "--release"
    }

    Write-Host ""
    Write-Host "Building cli797..."
    Push-Location $rustDir
    try {
        & $cargo.Source @buildArgs
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $sourceBinary)) {
    throw "Expected built binary at '$sourceBinary'. Re-run without -SkipBuild after cargo build succeeds."
}

New-Item -ItemType Directory -Path $installDirFull -Force | Out-Null
Copy-Item -LiteralPath $sourceBinary -Destination $destinationBinary -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathWasUpdated = $false

if (-not $NoPathUpdate) {
    if (-not (Test-PathEntryPresent -PathList $userPath -Candidate $installDirFull)) {
        $pathEntries = @()
        if (-not [string]::IsNullOrWhiteSpace($userPath)) {
            $pathEntries = $userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
        }

        $newUserPath = @($pathEntries + $installDirFull) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")

        if (-not (Test-PathEntryPresent -PathList $env:Path -Candidate $installDirFull)) {
            if ([string]::IsNullOrWhiteSpace($env:Path)) {
                $env:Path = $installDirFull
            } else {
                $env:Path = $env:Path.TrimEnd(';') + ";" + $installDirFull
            }
        }

        $pathWasUpdated = $true
    }
}

Write-Host ""
Write-Host "Installed: $destinationBinary"
Write-Host ""
Write-Host "Launch from any folder with:"
Write-Host "  cli797"
Write-Host "  cli797 prompt `"summarize this repository`""
Write-Host ""
Write-Host "Behavior:"
Write-Host "  - The folder where you run cli797 becomes the active workspace."
Write-Host "  - cli797 defaults to danger-full-access."
Write-Host "  - cli797 allows broad working directories such as your home folder."

if ($pathWasUpdated) {
    Write-Host ""
    Write-Host "PATH was updated for your user account and the current PowerShell session."
    Write-Host "Open a new terminal if another shell still cannot find cli797."
} elseif ($NoPathUpdate) {
    Write-Host ""
    Write-Host "PATH was not modified because -NoPathUpdate was used."
} else {
    Write-Host ""
    Write-Host "Install directory was already present on PATH."
}
