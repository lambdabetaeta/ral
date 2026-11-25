# Build the pinned OpenVMM and stage it where Synod and its installer look:
# target/openvmm/openvmm.exe, LICENSE-OPENVMM and THIRDPARTY-OPENVMM.
# Usage: powershell -NoProfile -File scripts/build-openvmm.ps1
#
# Needs git, protoc with its well-known-type includes, MSVC with the Windows
# SDK, a Rust toolchain at least as new as OpenVMM's rust-version, and
# cargo-about 0.9.3.

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
# Windows PowerShell's progress bar slows file operations by orders of magnitude.
$ProgressPreference = "SilentlyContinue"

$Upstream = "https://github.com/microsoft/openvmm.git"
$Pin = "efe0b08df805fb567cc4023359ef8e94c773837e"

function Die([string]$message) {
    throw "build-openvmm.ps1: $message"
}

# Runs a native command; a non-zero exit is fatal.
function Run([string]$what, [scriptblock]$command) {
    & $command
    if ($LASTEXITCODE -ne 0) { Die "$what failed (exit code $LASTEXITCODE)" }
}

$repo = Split-Path -Parent $PSScriptRoot
$out = Join-Path $repo "target\openvmm"
$about = Join-Path $repo "packaging\about"
$src = Join-Path $out "src"

# ── Prerequisites ─────────────────────────────────────────────────────────────

# Synod ships x64 only, and this builds for the host.
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    Die "run this from 64-bit PowerShell on x64 Windows; this one reports $env:PROCESSOR_ARCHITECTURE"
}

foreach ($tool in "git", "cargo") {
    if (-not (Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue)) {
        Die "$tool is not on PATH"
    }
}
if (-not (Get-Command cargo-about -CommandType Application -ErrorAction SilentlyContinue)) {
    Die "cargo-about is not on PATH: cargo install --locked --features cli cargo-about@0.9.3"
}

# Either would replace OpenVMM's own rustflags (delay-loads, CRT, CFG) wholesale.
foreach ($name in "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS") {
    if (Test-Path -LiteralPath "Env:$name") {
        Die "$name is set, which would override OpenVMM's link flags; unset it for this build"
    }
}

# OpenVMM's .cargo/config.toml points PROTOC at a .packages/ path that only its
# own setup tooling creates, and prost-build does not search PATH; so PROTOC
# must be exported, as an absolute path.
$protoc = if ($env:PROTOC) {
    if (-not (Test-Path -LiteralPath $env:PROTOC -PathType Leaf)) { Die "PROTOC names $env:PROTOC, which is not a file" }
    (Resolve-Path -LiteralPath $env:PROTOC).ProviderPath
}
else {
    $found = Get-Command protoc -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $found) {
        Die "protoc is missing: install the protobuf compiler (e.g. winget install protobuf) and put it on PATH, or set PROTOC to its absolute path"
    }
    $found.Source
}

# Ask protoc itself, so its own include discovery is what is checked.
$probe = Join-Path $out "protoc-probe"
New-Item -ItemType Directory -Force -Path $probe | Out-Null
Set-Content -LiteralPath (Join-Path $probe "probe.proto") -Encoding ascii -Value @(
    'syntax = "proto3";'
    'import "google/protobuf/empty.proto";'
    'message Probe { google.protobuf.Empty empty = 1; }'
)
& $protoc "--proto_path=$probe" "--descriptor_set_out=$(Join-Path $probe 'probe.pb')" (Join-Path $probe "probe.proto")
$includes = $LASTEXITCODE -eq 0
Remove-Item -LiteralPath $probe -Recurse -Force
if (-not $includes) {
    Die "$protoc cannot find google/protobuf/empty.proto; use a protoc release that ships its include/ directory beside bin/"
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$msvc = if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
    & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
if (-not $msvc) {
    Die "MSVC is missing: install Visual Studio or its Build Tools with the 'Desktop development with C++' workload"
}

# OpenVMM's build script compiles its version resource with the SDK's rc.exe.
$kits = Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots" -Name KitsRoot10 -ErrorAction SilentlyContinue
$rc = @(Get-Command rc.exe -CommandType Application -ErrorAction SilentlyContinue) +
    @(if ($kits) { Get-ChildItem -Path (Join-Path $kits.KitsRoot10 "bin\*\x64\rc.exe") -ErrorAction SilentlyContinue })
if ($rc.Count -eq 0) {
    Die "the Windows SDK's rc.exe is missing: add the Windows 11 SDK to Visual Studio's 'Desktop development with C++' workload"
}

# ── Source ────────────────────────────────────────────────────────────────────

if (-not (Test-Path -LiteralPath (Join-Path $src ".git"))) {
    New-Item -ItemType Directory -Force -Path $src | Out-Null
    Run "git init" { git -C $src init --quiet }
    Run "git remote add" { git -C $src remote add origin $Upstream }
}
git -C $src rev-parse --quiet --verify "$Pin^{commit}" | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Fetching microsoft/openvmm at $Pin"
    Run "fetching $Pin from $Upstream" { git -C $src fetch --quiet --depth 1 origin $Pin }
}
Run "checking out $Pin" { git -C $src checkout --quiet --force --detach $Pin }
$head = git -C $src rev-parse HEAD
if ($head -ne $Pin) { Die "$src is at $head, not the pin $Pin" }

# ── Build ─────────────────────────────────────────────────────────────────────

# Cargo reads .cargo/config.toml from the working directory, so build from $src.
$previousProtoc = $env:PROTOC
$env:PROTOC = $protoc
Push-Location -LiteralPath $src
try {
    Write-Host "Building openvmm.exe (protoc: $protoc)"
    Run "cargo build of OpenVMM" {
        cargo build --release --locked -p openvmm --no-default-features --features virt_whp --target-dir (Join-Path $src "target")
    }
    # The build's graph; OpenVMM's own crates are unpublished, so left out.
    Write-Host "Generating THIRDPARTY-OPENVMM"
    Run "cargo about for OpenVMM" {
        cargo about generate --fail --locked --config (Join-Path $about "about.toml") --manifest-path (Join-Path $src "openvmm\openvmm\Cargo.toml") --no-default-features --features virt_whp --target x86_64-pc-windows-msvc --name openvmm --output-file (Join-Path $out "THIRDPARTY-OPENVMM") $about
    }
}
finally {
    Pop-Location
    $env:PROTOC = $previousProtoc
}

# ── Stage ─────────────────────────────────────────────────────────────────────

Copy-Item -LiteralPath (Join-Path $src "target\release\openvmm.exe") -Destination (Join-Path $out "openvmm.exe") -Force
Copy-Item -LiteralPath (Join-Path $src "LICENSE") -Destination (Join-Path $out "LICENSE-OPENVMM") -Force
Write-Host "Staged openvmm.exe, LICENSE-OPENVMM and THIRDPARTY-OPENVMM in $out"
