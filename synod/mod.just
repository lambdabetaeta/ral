set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# Guest media are resources of tauri.bundle.conf.json alone, so a checkout
# without a built guest image still compiles synod.

# Bundle synod into a .app and .dmg (needs the Tauri CLI and an arm64 guest image).
[macos]
bundle:
    cargo tauri build --config tauri.bundle.conf.json

# Bundle synod into an .msi (needs the Tauri CLI and an amd64 guest image).
[windows]
bundle: _broker
    cargo tauri build --config tauri.bundle.conf.json

# The installer registers the broker as a service, so it is built first.
_broker:
    cargo build --release -p synod-machine-broker

# Install the machine broker as a LocalSystem service (elevated shell).
[windows]
broker-install: _broker
    sc.exe create SynodMachineBroker binPath= "{{justfile_directory()}}\target\release\synod-machine-broker.exe" DisplayName= "Synod machine broker" type= own start= auto obj= LocalSystem
    sc.exe description SynodMachineBroker "Creates and stops the virtual machines Synod runs its work in. Synod itself runs without privileges; this service holds the one privilege the platform requires for a virtual machine, and nothing else."
    sc.exe start SynodMachineBroker

# Stop and remove that service (elevated shell).
[windows]
broker-uninstall:
    -sc.exe stop SynodMachineBroker
    sc.exe delete SynodMachineBroker
