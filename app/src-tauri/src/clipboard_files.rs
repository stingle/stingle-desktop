//! Native file clipboard (Windows `CF_HDROP`) — copy/paste actual files the way
//! Explorer does, so a multi-file copy pastes all of them into Telegram, etc.
//!
//! The tauri clipboard-manager plugin only handles text/html/image, so for real
//! file copy/paste we talk to the Win32 clipboard directly.

const CF_HDROP: u32 = 15;

/// Put a list of file paths on the clipboard as `CF_HDROP`.
#[cfg(windows)]
pub fn set_files(paths: &[std::path::PathBuf]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return Ok(());
    }

    // Double-null-terminated, wide-char list of paths.
    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.as_os_str().encode_wide());
        wide.push(0);
    }
    wide.push(0);

    let header = std::mem::size_of::<DROPFILES>();
    let total = header + wide.len() * std::mem::size_of::<u16>();

    unsafe {
        OpenClipboard(None).map_err(|e| e.to_string())?;
        let result = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let hmem: HGLOBAL =
                GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock failed".into());
            }
            let df = ptr as *mut DROPFILES;
            (*df).pFiles = header as u32;
            (*df).fWide = BOOL(1);
            let dst = (ptr as *mut u8).add(header) as *mut u16;
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(hmem);
            // On success the system owns the memory; we must not free it.
            SetClipboardData(CF_HDROP, Some(HANDLE(hmem.0))).map_err(|e| e.to_string())?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Read a `CF_HDROP` file list from the clipboard (empty if none).
#[cfg(windows)]
pub fn get_files() -> Vec<String> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    let mut out = Vec::new();
    unsafe {
        if OpenClipboard(None).is_err() {
            return out;
        }
        if let Ok(handle) = GetClipboardData(CF_HDROP) {
            let hdrop = HDROP(handle.0);
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None);
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; (len + 1) as usize];
                let n = DragQueryFileW(hdrop, i, Some(&mut buf));
                if n > 0 {
                    out.push(String::from_utf16_lossy(&buf[..n as usize]));
                }
            }
        }
        let _ = CloseClipboard();
    }
    out
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[std::path::PathBuf]) -> Result<(), String> {
    Err("file clipboard is only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn get_files() -> Vec<String> {
    Vec::new()
}
