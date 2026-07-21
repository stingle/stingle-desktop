//! WinFsp adapter: presents a [`Vfs`] as a read-only Windows volume.
//!
//! Implements WinFsp's [`FileSystemContext`] over the platform-agnostic
//! [`Vfs`] core. All filesystem semantics (the directory tree, `stat` sizes,
//! decrypted reads) live in [`crate::tree`] / [`crate::ops`]; this file is the
//! thin marshalling layer between WinFsp's C callbacks and that core.
//!
//! The volume is mounted **read-only** and presents files as `READONLY` +
//! `NOT_CONTENT_INDEXED`, part of the side-channel hardening (keep the shell
//! indexer/thumbnailer off the decrypted bytes). Reads are served in ≤4 MiB
//! windows from memory and nothing is persisted by us.
//!
//! Modelled on the crate's own `memfs-winfsp-rs` example.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    LocalFree, HLOCAL, STATUS_END_OF_FILE, STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
    FILE_ATTRIBUTE_READONLY,
};
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};

use crate::ops::Vfs;
use crate::tree::{Attr, ROOT_INO};

/// Allocation unit reported to Windows (cosmetic; the volume is virtual).
const ALLOCATION_UNIT: u64 = 4096;

/// Ticks (100 ns) between the Windows FILETIME epoch (1601) and Unix epoch (1970).
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// `STATUS_UNEXPECTED_IO_ERROR` — returned when a decrypt/read fails.
const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9u32 as i32;

/// A permissive read/execute descriptor applied to every node. The volume is
/// mounted read-only, so no write can occur regardless of the ACL.
const ROOT_SDDL: &str = "O:BAG:BAD:P(A;;FRFX;;;WD)";

/// A WinFsp open handle — just the tree inode. Pointer-sized and `Copy`, so it
/// fits WinFsp's user-context slot with no boxing.
#[derive(Clone, Copy, Debug)]
pub struct Handle(u64);

/// The WinFsp filesystem context wrapping the read-only [`Vfs`].
pub struct StingleFs {
    vfs: Vfs,
    /// A serialized SECURITY_DESCRIPTOR handed back for every file/dir.
    security: Vec<u8>,
}

impl StingleFs {
    fn new(vfs: Vfs) -> Self {
        StingleFs {
            vfs,
            security: read_sddl(ROOT_SDDL).unwrap_or_default(),
        }
    }

    /// Resolve a WinFsp path (`\Gallery\2024\...`, root `\`) to a tree inode.
    fn resolve(&self, file_name: &U16CStr) -> Option<u64> {
        let path = file_name.to_string_lossy().replace('\\', "/");
        self.vfs.tree.resolve(&path)
    }

    fn attr_to_file_info(&self, attr: Attr) -> FileInfo {
        let mut fi = FileInfo::default();
        let base = if attr.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };
        fi.file_attributes = base | FILE_ATTRIBUTE_READONLY.0 | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.0;
        fi.file_size = attr.size;
        fi.allocation_size = attr.size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT;
        let ft = to_filetime(attr.mtime_ms);
        fi.creation_time = ft;
        fi.last_access_time = ft;
        fi.last_write_time = ft;
        fi.change_time = ft;
        fi.index_number = attr.ino;
        fi
    }

    /// Copy the shared security descriptor into `buf` if it fits; return its
    /// size in bytes either way (WinFsp probes with a too-small buffer first).
    fn write_security(&self, buf: Option<&mut [c_void]>) -> u64 {
        let size = self.security.len() as u64;
        if let Some(buf) = buf {
            if !self.security.is_empty() && (buf.len() as u64) >= size {
                // SAFETY: buf is at least `size` bytes (checked above) and the
                // source is a valid serialized descriptor of exactly that length.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.security.as_ptr(),
                        buf.as_mut_ptr() as *mut u8,
                        self.security.len(),
                    );
                }
            }
        }
        size
    }
}

fn not_found() -> FspError {
    FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND.0)
}

/// Convert epoch-milliseconds to a Windows FILETIME (100 ns ticks since 1601).
fn to_filetime(ms: i64) -> u64 {
    ms.saturating_mul(10_000)
        .saturating_add(FILETIME_UNIX_EPOCH)
        .max(0) as u64
}

/// Whether the directory marker equals a specific name (`.` / `..`).
fn marker_is(marker: &DirMarker, name: &[u16]) -> bool {
    marker.inner().map(|m| m == name).unwrap_or(false)
}

/// Parse an SDDL string into a serialized SECURITY_DESCRIPTOR byte buffer.
fn read_sddl(sddl: &str) -> Option<Vec<u8>> {
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; the out-params
    // are owned locals. On success WinAPI allocates `descriptor` via LocalAlloc.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .ok()?;
    }
    if descriptor.0.is_null() {
        return None;
    }
    // SAFETY: `descriptor` is a valid self-relative descriptor allocated above.
    let len = unsafe { GetSecurityDescriptorLength(descriptor) } as usize;
    let bytes =
        unsafe { std::slice::from_raw_parts(descriptor.0 as *const u8, len) }.to_vec();
    // SAFETY: `descriptor.0` came from a LocalAlloc inside the conversion call.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    Some(bytes)
}

/// A fixed, Stingle-specific CLSID for the Explorer navigation-pane entry.
/// Stable across runs so enable/disable reuses (and fully cleans up) the same
/// keys.
const NAV_CLSID: &str = "{53B0F2E7-9C41-4E8A-8B6D-2F1A0C7E5D34}";

/// The system shell "delegate folder" that forwards to the filesystem path in
/// its InitPropertyBag's `TargetFolderPath`. Lets a pure-registry namespace
/// entry point at our mounted drive with no COM DLL of our own.
const TARGET_FOLDER_DELEGATE: &str = "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}";

/// The uppercase drive letter of a mount point like `"S:"`.
fn drive_letter(mount_point: &str) -> Option<char> {
    mount_point
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
}

/// Point the drive letter's Explorer icon at the app executable's embedded icon
/// (`icon` = `"<exe>,0"`), via the per-user `DriveIcons` key. Set before mount so
/// Explorer reads it as the drive appears.
fn set_drive_icon(letter: char, icon: &str) {
    if let Ok(k) = windows_registry::CURRENT_USER
        .create(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}\DefaultIcon"))
    {
        let _ = k.set_string("", icon);
    }
}

/// Register a pinned navigation-pane entry ("Stingle") that opens the drive —
/// the OneDrive-style sidebar item, done entirely with per-user (HKCU) registry
/// via the system shell folder (no admin, no COM DLL, fully reversible).
fn register_nav_entry(letter: char, icon: &str) {
    let cu = windows_registry::CURRENT_USER;
    let clsid = format!(r"Software\Classes\CLSID\{NAV_CLSID}");

    if let Ok(k) = cu.create(&clsid) {
        let _ = k.set_string("", "Stingle");
        // Pin into the navigation tree (like OneDrive) and sort near the drives.
        let _ = k.set_u32("System.IsPinnedToNameSpaceTree", 1);
        let _ = k.set_u32("SortOrderIndex", 0x42);
    }
    if let Ok(k) = cu.create(format!("{clsid}\\DefaultIcon")) {
        let _ = k.set_string("", icon);
    }
    if let Ok(k) = cu.create(format!("{clsid}\\InProcServer32")) {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let _ = k.set_string("", &format!(r"{sysroot}\system32\shell32.dll"));
    }
    if let Ok(k) = cu.create(format!("{clsid}\\Instance")) {
        let _ = k.set_string("CLSID", TARGET_FOLDER_DELEGATE);
    }
    if let Ok(k) = cu.create(format!("{clsid}\\Instance\\InitPropertyBag")) {
        let _ = k.set_u32("Attributes", 0x11);
        let _ = k.set_string("TargetFolderPath", &format!("{letter}:\\"));
    }
    if let Ok(k) = cu.create(format!("{clsid}\\ShellFolder")) {
        let _ = k.set_u32("Attributes", 0xF080_004D);
        let _ = k.set_u32("FolderValueFlags", 0x28);
    }
    if let Ok(k) = cu.create(format!(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{NAV_CLSID}"
    )) {
        let _ = k.set_string("", "Stingle");
    }
    // Keep it out of the Desktop icon set (nav pane only).
    for view in ["NewStartPanel", "ClassicStartMenu"] {
        if let Ok(k) = cu.create(format!(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\{view}"
        )) {
            let _ = k.set_u32(NAV_CLSID, 1);
        }
    }
}

/// Remove everything [`set_drive_icon`] / [`register_nav_entry`] created and
/// nudge Explorer to refresh.
fn teardown_shell_integration(letter: char) {
    let cu = windows_registry::CURRENT_USER;
    let _ = cu.remove_tree(format!(r"Software\Classes\CLSID\{NAV_CLSID}"));
    let _ = cu.remove_tree(format!(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{NAV_CLSID}"
    ));
    let _ = cu.remove_tree(format!(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}"
    ));
    shell_refresh();
}

/// Ask Explorer to re-read icons and namespace entries after a registry change.
fn shell_refresh() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    // SAFETY: SHChangeNotify with null item pointers is always valid.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

impl FileSystemContext for StingleFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let ino = self.resolve(file_name).ok_or_else(not_found)?;
        let attr = self.vfs.tree.attr(ino).ok_or_else(not_found)?;
        let attributes = self.attr_to_file_info(attr).file_attributes;
        let sz_security_descriptor = self.write_security(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor,
            attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let ino = self.resolve(file_name).ok_or_else(not_found)?;
        let attr = self.vfs.tree.attr(ino).ok_or_else(not_found)?;
        *file_info.as_mut() = self.attr_to_file_info(attr);
        Ok(Handle(ino))
    }

    fn close(&self, _context: Self::FileContext) {}

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        Ok(self.write_security(security_descriptor))
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self.vfs.tree.attr(context.0).ok_or_else(not_found)?;
        *file_info = self.attr_to_file_info(attr);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let bytes = self
            .vfs
            .read(context.0, offset, buffer.len() as u32)
            .map_err(|_| FspError::NTSTATUS(STATUS_UNEXPECTED_IO_ERROR))?;
        if bytes.is_empty() {
            // Offset at/after EOF — WinFsp expects this status.
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE.0));
        }
        let n = bytes.len();
        buffer[..n].copy_from_slice(&bytes);
        Ok(n as u32)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let ino = context.0;
        let mut cursor = 0u32;
        let mut dir_info: DirInfo<255> = DirInfo::new();

        let dot = [b'.' as u16];
        let dotdot = [b'.' as u16, b'.' as u16];
        let marker_none = marker.is_none();
        let marker_dot = marker_is(&marker, &dot);

        // "." and ".." for non-root directories, gated by the resume marker.
        if ino != ROOT_INO {
            if marker_none {
                dir_info.reset();
                if let Some(a) = self.vfs.tree.attr(ino) {
                    *dir_info.file_info_mut() = self.attr_to_file_info(a);
                }
                dir_info.set_name_raw(dot.as_slice())?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    return Ok(cursor);
                }
            }
            if marker_none || marker_dot {
                dir_info.reset();
                let parent = self.vfs.tree.parent(ino);
                if let Some(a) = self.vfs.tree.attr(parent) {
                    *dir_info.file_info_mut() = self.attr_to_file_info(a);
                }
                dir_info.set_name_raw(dotdot.as_slice())?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    return Ok(cursor);
                }
            }
        }

        // Resume strictly after the marker name, comparing in the same order the
        // tree enumerates (`String` order == the children BTreeMap order).
        let start_after: Option<String> = if marker_none || marker_dot {
            None
        } else {
            marker.inner().map(String::from_utf16_lossy)
        };

        if let Some(children) = self.vfs.tree.children(ino) {
            for d in children {
                if let Some(after) = &start_after {
                    if &d.name <= after {
                        continue;
                    }
                }
                dir_info.reset();
                if let Some(a) = self.vfs.tree.attr(d.ino) {
                    *dir_info.file_info_mut() = self.attr_to_file_info(a);
                }
                let name_w: Vec<u16> = d.name.encode_utf16().collect();
                dir_info.set_name_raw(name_w.as_slice())?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    return Ok(cursor);
                }
            }
        }

        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        // A virtual, read-only volume: advertise a large capacity, no free space.
        out_volume_info.total_size = 1 << 50; // 1 PiB
        out_volume_info.free_size = 0;
        out_volume_info.set_volume_label("Stingle");
        Ok(())
    }
}

/// A live WinFsp mount. Dropping it stops the dispatcher and unmounts (handled
/// by [`FileSystemHost`]'s own `Drop`). The [`winfsp::FspInit`] token is held
/// for the mount's lifetime so the WinFsp library stays loaded.
pub struct WinFspMount {
    // Field order matters for drop: the host tears down before the init token.
    _host: FileSystemHost<StingleFs, FineGuard>,
    _init: winfsp::FspInit,
    /// The mounted drive letter, so its icon key can be cleared on unmount.
    drive_letter: Option<char>,
}

impl WinFspMount {
    /// Mount `vfs` read-only at `mount_point` (e.g. `"S:"`), starting the
    /// dispatcher. Fails if WinFsp isn't installed or the mount point is busy.
    pub fn mount(vfs: Vfs, mount_point: &str) -> winfsp::Result<Self> {
        // Loads the WinFsp DLL (delay-loaded) and initializes the library.
        let init = winfsp::winfsp_init()?;

        let mut params = VolumeParams::new();
        params
            .sector_size(512)
            .sectors_per_allocation_unit(1)
            // Cache attrs/dir listings briefly so a picker scrolling the grid
            // doesn't re-`stat` every tile on every paint.
            .file_info_timeout(1000)
            .case_sensitive_search(false)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .read_only_volume(true)
            .post_cleanup_when_modified_only(true);
        params.filesystem_name("Stingle");

        let context = StingleFs::new(vfs);
        let mut host = FileSystemHost::<StingleFs, FineGuard>::new(params, context)?;

        let drive_letter = drive_letter(mount_point);
        let icon = std::env::current_exe()
            .ok()
            .map(|exe| format!("{},0", exe.display()));
        // The drive icon must exist BEFORE the volume appears so Explorer reads
        // it as the drive shows up (not the cached generic disk).
        if let (Some(l), Some(ic)) = (drive_letter, icon.as_ref()) {
            set_drive_icon(l, ic);
        }

        host.mount(mount_point.to_string())?;
        host.start()?;

        // The nav-pane entry points at the drive root, so register it AFTER the
        // drive exists, then refresh Explorer to surface both it and the icon.
        if let (Some(l), Some(ic)) = (drive_letter, icon.as_ref()) {
            register_nav_entry(l, ic);
            shell_refresh();
        }

        Ok(WinFspMount {
            _host: host,
            _init: init,
            drive_letter,
        })
    }
}

impl Drop for WinFspMount {
    fn drop(&mut self) {
        // Clear the icon + nav-pane keys before the volume goes away. The
        // `_host` field (declared first) unmounts immediately after.
        if let Some(letter) = self.drive_letter {
            teardown_shell_integration(letter);
        }
    }
}
