# Install exarch.
# Usage: irm https://lambdabetaeta.github.io/ral/scripts/exarch-install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "lambdabetaeta/ral"

# github.com/$Repo/releases/latest redirects to .../releases/tag/<Tag> —
# the newest *versioned* release, distinct from any release whose tag name
# is literally "latest" (build-binaries.yml's workflow_dispatch channel).
$latestResponse = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest"
$Tag = ($latestResponse.BaseResponse.ResponseUri.ToString() -split "/tag/")[1]
if (-not $Tag) {
    Write-Error "Could not resolve the latest release tag for $Repo."
}

# ── Platform detection ────────────────────────────────────────────────────────

$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    "AMD64" { $Artifact = "exarch-windows.exe" }
    "ARM64" {
        Write-Host "No native ARM64 Windows build; using x86_64 (runs under Windows 11's x64 emulation)."
        $Artifact = "exarch-windows.exe"
    }
    default {
        Write-Error "Unsupported Windows architecture: $arch"
    }
}

# ── Download ───────────────────────────────────────────────────────────────────

$url = "https://github.com/$Repo/releases/download/$Tag/$Artifact"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $Artifact from $Repo ($Tag)"
    try {
        Invoke-WebRequest -Uri $url          -OutFile (Join-Path $tmp "exarch.exe")
        Invoke-WebRequest -Uri "$url.sha256" -OutFile (Join-Path $tmp "exarch.exe.sha256")
    }
    catch {
        if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) {
            Write-Error "No Windows exarch release exists yet; build from source (cargo build --release -p exarch) or wait for the next release."
        }
        throw
    }

    $expected = (Get-Content (Join-Path $tmp "exarch.exe.sha256") -Raw).Trim()
    $actual   = (Get-FileHash (Join-Path $tmp "exarch.exe") -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Write-Error "Checksum mismatch!`n  expected: $expected`n  got:      $actual"
    }
    Write-Host "Checksum OK."

    # ── Install binary ────────────────────────────────────────────────────────

    $installDir = Join-Path $env:LOCALAPPDATA "Programs\ral"
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $tmp "exarch.exe") (Join-Path $installDir "exarch.exe") -Force
    Write-Host "Installed $installDir\exarch.exe"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $installDir) {
        # Not `setx PATH "$env:Path;<dir>"`: that bakes the *merged*
        # machine+user PATH into the user PATH, and setx silently truncates
        # at 1024 characters. Read and write the user PATH only.
        $sample = "[Environment]::SetEnvironmentVariable(`"Path`", [Environment]::GetEnvironmentVariable(`"Path`",`"User`") + `";$installDir`", `"User`")"
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
Write-Host "exarch is ready.  The ral shell it runs on is built in — no separate install."
Write-Host "Set a provider key in your environment, then run exarch from a project:"
Write-Host "  `$env:ANTHROPIC_API_KEY = '...'; exarch"
