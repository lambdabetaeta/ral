# Install exarch.
# Usage: irm https://lambdabetaeta.github.io/ral/scripts/exarch-install.ps1 | iex
#
# Everything below is one script block, invoked where it closes.  `iex` runs
# text in the caller's scope, so the block keeps this script's variables and
# preferences out of the user's session; and a truncated download leaves the
# block unclosed, which fails to parse rather than running half an installer.
& {
    $ErrorActionPreference = "Stop"
    # Windows PowerShell's progress bar slows a download by orders of magnitude.
    $ProgressPreference = "SilentlyContinue"
    # Windows PowerShell may offer only TLS 1.0 and 1.1, which GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $Repo = "lambdabetaeta/ral"

    function Die([string]$message) {
        throw "exarch-install.ps1: $message"
    }

    # ── Presentation ──────────────────────────────────────────────────────────

    # Colour only on a terminal that wants it (no-color.org).
    $paint = -not [Console]::IsOutputRedirected -and -not $env:NO_COLOR -and $Host.UI.SupportsVirtualTerminal

    function Sgr([string]$code) {
        if ($paint) { "$([char]27)[${code}m" } else { "" }
    }

    # One line of progress.  * doing, + done, ! heed.
    function Mark([string]$sigil, [string]$verb, [string]$detail) {
        $ink = switch ($sigil) {
            "*" { "36" }
            "+" { "32" }
            "!" { "33" }
            default { Die "Mark: unknown sigil: $sigil" }
        }
        Write-Host ("  {0}[{1}]{2} {3}{4,-8}{2} {5}" -f (Sgr $ink), $sigil, (Sgr 0), (Sgr 1), $verb, $detail)
    }

    # Paints a figure in the ANSI Shadow face with a diagonal gradient from
    # $from to $to.  The figure is drawn in ASCII as in install.sh, so no glyph
    # depends on how `irm` decoded this text: # full block, = and | double
    # rules, and the corners r 7 L J.
    function Gradient([int[]]$from, [int[]]$to, [string[]]$art) {
        $glyph = @{ "#" = 0x2588; "=" = 0x2550; "|" = 0x2551; "r" = 0x2554; "7" = 0x2557; "L" = 0x255A; "J" = 0x255D }
        $span = ($art | Measure-Object -Property Length -Maximum).Maximum + 2 * $art.Count
        for ($y = 0; $y -lt $art.Count; $y++) {
            $line = "  "
            for ($x = 0; $x -lt $art[$y].Length; $x++) {
                $c = [string]$art[$y][$x]
                if ($c -eq " ") { $line += $c; continue }
                $t = ($x + 2 * $y + 3) / $span
                $rgb = foreach ($i in 0..2) { [int]($from[$i] + ($to[$i] - $from[$i]) * $t) }
                $line += (Sgr "38;2;$($rgb -join ';')") + [char]$glyph[$c]
            }
            Write-Host ($line + (Sgr 0))
        }
    }

    Write-Host ""
    Gradient @(190, 100, 255) @(255, 120, 80) @(
        '#######7##7  ##7 #####7 ######7  ######7##7  ##7'
        '##r====JL##7##rJ##r==##7##r==##7##r====J##|  ##|'
        '#####7   L###rJ #######|######rJ##|     #######|'
        '##r==J   ##r##7 ##r==##|##r==##7##|     ##r==##|'
        '#######7##rJ ##7##|  ##|##|  ##|L######7##|  ##|'
        'L======JL=J  L=JL=J  L=JL=J  L=J L=====JL=J  L=J'
    )
    Write-Host ("  {0}a coding agent whose only tool is the shell{1}" -f (Sgr 2), (Sgr 0))
    Write-Host ""

    # ── Platform detection ────────────────────────────────────────────────────

    # A 32-bit PowerShell reports its own architecture; ARCHITEW6432 the machine's.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        "AMD64" { }
        "ARM64" {
            if ([Environment]::OSVersion.Version.Build -lt 22000) {
                Die "there is no ARM64 build, and only Windows 11 on ARM runs x64 programs"
            }
            Mark "!" arch "no native ARM64 build; using x64 under Windows 11's emulation"
        }
        default { Die "there is no build for $arch Windows" }
    }

    # ── Download ──────────────────────────────────────────────────────────────

    $Artifact = "exarch-windows.zip"
    $url = "https://github.com/$Repo/releases/latest/download/$Artifact"

    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $staged = $null
    try {
        Mark "*" release "$Repo latest"
        Mark "*" fetch $Artifact
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $url          -OutFile (Join-Path $tmp $Artifact)
            Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile (Join-Path $tmp "$Artifact.sha256")
        }
        catch {
            if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) {
                Die "no Windows exarch release exists yet; build from source (cargo build --release -p exarch) or wait for the next release"
            }
            throw
        }

        $expected = ("$(Get-Content -Raw (Join-Path $tmp "$Artifact.sha256"))".Trim() -split '\s+')[0]
        if ($expected -cnotmatch '^[0-9a-f]{64}$') {
            Die "the published checksum is not a SHA-256 digest: '$expected'"
        }
        $actual = (Get-FileHash (Join-Path $tmp $Artifact) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Die "checksum mismatch; the download is damaged, so try again`n  expected: $expected`n  actual:   $actual"
        }
        Mark "+" verify "sha256 $($expected.Substring(0, 16))..."
        Expand-Archive -LiteralPath (Join-Path $tmp $Artifact) -DestinationPath $tmp

        # ── Install binary ────────────────────────────────────────────────────

        $installDir = Join-Path $env:LOCALAPPDATA "Programs\ral"
        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        foreach ($file in @("LICENSE-APACHE", "LICENSE-MIT", "NOTICE", "LICENSE-EDTUI")) {
            Copy-Item (Join-Path $tmp $file) (Join-Path $installDir $file) -Force
        }

        # A running exe cannot be overwritten, but it can be renamed aside; a
        # later install deletes it, once nothing runs it any more.
        Get-ChildItem -LiteralPath $installDir -File |
            Where-Object Name -match '^exarch\.[0-9a-f]{32}\.(old|new\.exe)$' |
            Remove-Item -Force -ErrorAction SilentlyContinue
        $exe = Join-Path $installDir "exarch.exe"
        $staged = Join-Path $installDir "exarch.$([guid]::NewGuid().ToString('N')).new.exe"
        Copy-Item (Join-Path $tmp "exarch-windows.exe") $staged
        $runs = try { & $staged --help *> $null; $LASTEXITCODE -eq 0 } catch { $false }
        if (-not $runs) { Die "the downloaded exarch does not run on this $arch machine" }
        if (Test-Path -LiteralPath $exe) {
            Move-Item -LiteralPath $exe (Join-Path $installDir "exarch.$([guid]::NewGuid().ToString('N')).old")
        }
        Move-Item -LiteralPath $staged $exe
        $staged = $null
        Mark "+" install $exe

        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (($userPath -split ";") -notcontains $installDir) {
            # Not `setx PATH "$env:Path;<dir>"`: that bakes the *merged*
            # machine+user PATH into the user PATH, and setx silently truncates
            # at 1024 characters. Read and write the user PATH only.
            $sample = "[Environment]::SetEnvironmentVariable(`"Path`", [Environment]::GetEnvironmentVariable(`"Path`",`"User`") + `";$installDir`", `"User`")"
            Mark "!" path "$installDir is not on your PATH; add it for future sessions:"
            Write-Host ""
            Write-Host "    $sample"
        }
    }
    finally {
        if ($staged) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
        Remove-Item -Recurse -Force $tmp
    }

    Write-Host ""
    Write-Host ("  {0}exarch is ready.{1}" -f (Sgr 1), (Sgr 0))
    Write-Host "  Set a provider key in your environment, then run exarch from a project:"
    Write-Host ""
    Write-Host "    `$env:ANTHROPIC_API_KEY = '...'; exarch"
    Write-Host ""
}
