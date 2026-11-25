set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# Guest media and synod's third-party notices are resources of
# tauri.bundle.conf.json alone, and OpenVMM of tauri.bundle.windows.conf.json
# alone, so a checkout without them still compiles synod.

notices := justfile_directory() / 'target' / 'third-party' / 'THIRDPARTY-SYNOD'

# Bundle synod into a .app and .dmg (needs the Tauri CLI, cargo-about and an arm64 guest image).
[macos]
bundle:
    mkdir -p {{quote(parent_directory(notices))}}
    {{quote(just_executable())}} --justfile {{quote(justfile())}} third-party synod "$(rustc --print host-tuple)" {{quote(notices)}}
    cargo tauri build --config tauri.bundle.conf.json

# Bundle synod into an .msi (needs the Tauri CLI, cargo-about, an amd64 guest image and a staged OpenVMM).
[windows]
bundle: _openvmm
    New-Item -ItemType Directory -Force {{quote(parent_directory(notices))}} > $null
    & {{quote(just_executable())}} --justfile {{quote(justfile())}} third-party synod (rustc --print host-tuple) {{quote(notices)}}
    cargo tauri build --config tauri.bundle.conf.json --config tauri.bundle.windows.conf.json

# The files scripts/build-openvmm.ps1 stages, which the .msi installs beside synod.exe.
[windows]
_openvmm:
    foreach ($file in 'openvmm.exe', 'LICENSE-OPENVMM', 'THIRDPARTY-OPENVMM') { $path = Join-Path '{{justfile_directory() / 'target' / 'openvmm'}}' $file; if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "OpenVMM is not staged: $path is missing; run scripts/build-openvmm.ps1 from the repository root first" } }
