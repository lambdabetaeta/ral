set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# Guests are built for this host's arch: nothing here cross-builds.

# The office userland, rootfs.img.
[unix]
rootfs:
    bash build-rootfs.sh

# The kernel and initramfs; `--clean` drops the kept build cache.
[unix]
boot *flags:
    bash build-boot.sh {{flags}}

[windows]
rootfs:
    wsl.exe -- bash -lc 'cd "$(wslpath "{{source_directory()}}")" && bash build-rootfs.sh'

[windows]
boot *flags:
    wsl.exe -- bash -lc 'cd "$(wslpath "{{source_directory()}}")" && bash build-boot.sh {{flags}}'

# Boot the Windows machine layer and hold it open (elevated shell or Hyper-V Administrators).
[no-cd]
[windows]
smoke KERNEL=(source_directory() / 'out/boot/kernel') INITRAMFS=(source_directory() / 'out/boot/initramfs.img') ROOTFS=(source_directory() / 'out/rootfs/rootfs.img') FOLDER=justfile_directory():
    cargo run -p vm-manager --example boot-smoke -- {{KERNEL}} {{INITRAMFS}} {{ROOTFS}} {{FOLDER}}
