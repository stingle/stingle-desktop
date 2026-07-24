# Bundled driver installers (virtual drive)

The virtual-drive feature (`vfs-winfsp` / `vfs-fuse`) needs a filesystem driver
on the user's machine. To ship it inside the Stingle installer (no external
downloads), drop the vendor installers here and enable bundling.

## Files to place here (NOT committed — fetched by CI at build time)

- `winfsp.msi`  — WinFsp installer (Windows). From https://winfsp.dev/rel/ (the
  signed `winfsp-<ver>.msi`). **Verify the WinFsp GPLv3/commercial redistribution
  terms before shipping.**
- `macfuse.pkg` — macFUSE installer (macOS). From https://macfuse.io.

Linux needs no bundled installer: `fuse3` is declared as a `.deb` dependency in
`tauri.conf.json` (`bundle.linux.deb.depends`).

## Enable bundling

Add these files to the app bundle so they land in the runtime resource dir
(`resource_dir()/resources/…`, which `vfs_install_driver` looks in). In
`tauri.conf.json` under `bundle`:

```jsonc
"resources": {
  "resources/winfsp.msi": "resources/winfsp.msi",
  "resources/macfuse.pkg": "resources/macfuse.pkg"
}
```

(Only list the file(s) relevant to the platform you're building; a missing entry
just means `vfs_install_driver` reports "installer isn't bundled" and the user
is pointed at the vendor page.)

## Windows: install silently during our installer

Instead of (or in addition to) the runtime "Install driver…" button, run the
MSI from the NSIS installer while it's already elevated — see
`../installer-hooks.nsh` and wire it via
`bundle.windows.nsis.installerHooks`. That way the driver is present right after
Stingle installs, with no separate prompt.

## macOS: guided, not silent

macFUSE is a system extension; Apple requires the user to approve it in
System Settings → Privacy & Security and reboot once. `vfs_install_driver` opens
`macfuse.pkg`; the app shows the approval/reboot instructions. This cannot be
made fully silent.
