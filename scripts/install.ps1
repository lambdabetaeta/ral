# Install ral.
# Usage: irm https://lambdabetaeta.github.io/ral/scripts/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "lambdabetaeta/ral"
$Tag  = "latest"

# ── Platform detection ────────────────────────────────────────────────────────

$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    "AMD64" { $Artifact = "ral-windows.exe" }
    "ARM64" {
        Write-Host "No native ARM64 Windows build; using x86_64 (runs under Windows 11's x64 emulation)."
        $Artifact = "ral-windows.exe"
    }
    default {
        Write-Error "Unsupported Windows architecture: $arch"
        exit 1
    }
}

# ── Download ───────────────────────────────────────────────────────────────────

$url = "https://github.com/$Repo/releases/download/$Tag/$Artifact"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $Artifact from $Repo ($Tag)"
    Invoke-WebRequest -Uri $url            -OutFile (Join-Path $tmp "ral.exe")
    Invoke-WebRequest -Uri "$url.sha256"   -OutFile (Join-Path $tmp "ral.exe.sha256")

    $expected = (Get-Content (Join-Path $tmp "ral.exe.sha256") -Raw).Trim()
    $actual   = (Get-FileHash (Join-Path $tmp "ral.exe") -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Write-Error "Checksum mismatch!`n  expected: $expected`n  got:      $actual"
        exit 1
    }
    Write-Host "Checksum OK."

    # ── Install binary ────────────────────────────────────────────────────────

    $installDir = Join-Path $env:LOCALAPPDATA "Programs\ral"
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $tmp "ral.exe") (Join-Path $installDir "ral.exe") -Force
    Write-Host "Installed $installDir\ral.exe"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $installDir) {
        $sample = 'setx PATH "$env:Path;' + $installDir + '"'
        Write-Host ""
        Write-Host "Note: $installDir is not on your PATH."
        Write-Host "Add it for future sessions (PowerShell):"
        Write-Host ""
        Write-Host "  $sample"
        Write-Host ""
    }
}
finally {
    Remove-Item -Recurse -Force $tmp
}

Write-Host ""
Write-Host "ral is ready.  Run: ral"
