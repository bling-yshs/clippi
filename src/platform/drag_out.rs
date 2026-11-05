//! Native drag-out of clipboard history items to other applications.
//!
//! Windows only for now: the payload is put into a Shell data object
//! (`SHCreateDataObject`) and handed to `SHDoDragDrop`, which supplies the
//! default drop source and drag image. Only `DROPEFFECT_COPY` is offered so a
//! drop on Explorer always copies — it must never move the backing image file
//! out of Clippi's storage directory.

/// Data offered to the drop target. The target picks whichever format it
/// understands: Explorer/desktop take `files`, editors and browsers take the
/// text (or HTML/RTF) representations.
#[derive(Debug, Default, Clone)]
pub struct DragPayload {
    /// Absolute paths offered as `CF_HDROP`.
    pub files: Vec<String>,
    /// Plain text offered as `CF_UNICODETEXT`.
    pub text: Option<String>,
    /// Raw HTML fragment (encoded to `HTML Format` on Windows).
    pub html: Option<String>,
    /// RTF document offered as `Rich Text Format`.
    pub rtf: Option<String>,
}

impl DragPayload {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.text.as_deref().map_or(true, str::is_empty)
    }
}

/// Whether drag-out is available on this platform.
pub const SUPPORTED: bool = cfg!(target_os = "windows");

#[cfg(target_os = "windows")]
pub use imp::{drag_threshold_px, run_drag};

#[cfg(not(target_os = "windows"))]
pub fn drag_threshold_px() -> (f32, f32) {
    (4.0, 4.0)
}

/// Not supported on this platform; never starts a drag.
#[cfg(not(target_os = "windows"))]
pub fn run_drag(_hwnd: isize, _payload: &DragPayload) -> bool {
    false
}

#[cfg(target_os = "windows")]
mod imp {
    use super::DragPayload;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{GlobalFree, BOOL, HGLOBAL, HWND, POINT};
    use windows::Win32::System::Com::{
        IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
    use windows::Win32::System::Ole::{
        IDropSource, CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::UI::Shell::{SHCreateDataObject, SHDoDragDrop, DROPFILES};

    /// System drag threshold in physical pixels (`SM_CXDRAG` / `SM_CYDRAG`).
    pub fn drag_threshold_px() -> (f32, f32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXDRAG, SM_CYDRAG,
        };
        let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
        (cx.max(1) as f32, cy.max(1) as f32)
    }

    /// Whether the logical primary (left) mouse button is physically held now.
    /// A drag must only start while it is, otherwise `DoDragDrop` would drop
    /// immediately at the current cursor position.
    fn primary_button_down() -> bool {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
        let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
        let key = if swapped { VK_RBUTTON } else { VK_LBUTTON };
        (unsafe { GetAsyncKeyState(key as i32) } as u16 & 0x8000) != 0
    }

    fn alloc_global(bytes: &[u8]) -> Option<HGLOBAL> {
        unsafe {
            let handle = GlobalAlloc(GHND, bytes.len().max(1)).ok()?;
            let ptr = GlobalLock(handle) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(handle);
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            // Returns an "error" with NO_ERROR once the lock count hits zero.
            let _ = GlobalUnlock(handle);
            Some(handle)
        }
    }

    /// Store `bytes` in the data object as an HGLOBAL; the data object takes
    /// ownership on success (`fRelease = TRUE`).
    fn set_global(data: &IDataObject, format: u16, bytes: &[u8]) -> bool {
        if format == 0 {
            return false;
        }
        let Some(handle) = alloc_global(bytes) else {
            log::warn!("drag_out: GlobalAlloc failed for format {format}");
            return false;
        };
        let format_etc = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: handle },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        match unsafe { data.SetData(&format_etc, &medium, BOOL(1)) } {
            Ok(()) => true,
            Err(e) => {
                log::warn!("drag_out: SetData failed for format {format}: {e}");
                unsafe {
                    let _ = GlobalFree(handle);
                }
                false
            }
        }
    }

    fn register_format(name: PCWSTR) -> u16 {
        // Registered clipboard formats are always in 0xC000..=0xFFFF.
        unsafe { RegisterClipboardFormatW(name) as u16 }
    }

    fn utf16z_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn nul_terminated(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    /// `DROPFILES` header followed by a double-NUL-terminated UTF-16 path list.
    fn hdrop_bytes(files: &[String]) -> Vec<u8> {
        let header_len = std::mem::size_of::<DROPFILES>();
        let header = DROPFILES {
            pFiles: header_len as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: BOOL(0),
            fWide: BOOL(1),
        };
        let mut bytes = vec![0u8; header_len];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &header as *const DROPFILES as *const u8,
                bytes.as_mut_ptr(),
                header_len,
            );
        }
        for file in files {
            let path = file.replace('/', "\\");
            bytes.extend(path.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes));
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    fn build_data_object(payload: &DragPayload) -> Option<IDataObject> {
        let data: IDataObject =
            match unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) } {
                Ok(data) => data,
                Err(e) => {
                    log::warn!("drag_out: SHCreateDataObject failed: {e}");
                    return None;
                }
            };

        let mut any = false;
        if !payload.files.is_empty() {
            any |= set_global(&data, CF_HDROP.0, &hdrop_bytes(&payload.files));
        }
        if let Some(text) = payload.text.as_deref().filter(|text| !text.is_empty()) {
            any |= set_global(&data, CF_UNICODETEXT.0, &utf16z_bytes(text));
            if let Some(html) = payload.html.as_deref().filter(|html| !html.is_empty()) {
                let cf_html = crate::core::html_text::encode_cf_html(html);
                set_global(&data, register_format(w!("HTML Format")), &nul_terminated(&cf_html));
            }
            if let Some(rtf) = payload.rtf.as_deref().filter(|rtf| !rtf.is_empty()) {
                set_global(&data, register_format(w!("Rich Text Format")), &nul_terminated(rtf));
            }
        }
        if !any {
            return None;
        }
        // Ask Explorer to copy rather than move.
        set_global(
            &data,
            register_format(w!("Preferred DropEffect")),
            &DROPEFFECT_COPY.0.to_le_bytes(),
        );
        Some(data)
    }

    /// Whether the cursor is currently over one of `hwnd`'s own windows
    /// (a drop back onto Clippi itself is not a real drag-out).
    fn cursor_over_own_window(hwnd: isize) -> bool {
        use windows_sys::Win32::Foundation::POINT as SysPoint;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetCursorPos, WindowFromPoint, GA_ROOT,
        };
        unsafe {
            let mut point = SysPoint { x: 0, y: 0 };
            if GetCursorPos(&mut point) == 0 {
                return false;
            }
            let under = WindowFromPoint(point);
            if under.is_null() {
                return false;
            }
            GetAncestor(under, GA_ROOT) as isize == hwnd
        }
    }

    /// Run a modal OLE drag. Blocks until the drop or cancel and pumps window
    /// messages meanwhile, so it must never be called while the GPUI app is
    /// borrowed (call it from a foreground task body, outside `update`).
    /// Returns whether the payload was dropped on another application.
    pub fn run_drag(hwnd: isize, payload: &DragPayload) -> bool {
        if payload.is_empty() || !primary_button_down() {
            return false;
        }
        let Some(data) = build_data_object(payload) else {
            return false;
        };
        let result = unsafe {
            SHDoDragDrop(
                HWND(hwnd as *mut core::ffi::c_void),
                &data,
                None::<&IDropSource>,
                DROPEFFECT_COPY,
            )
        };
        match result {
            Ok(effect) => effect != DROPEFFECT_NONE && !cursor_over_own_window(hwnd),
            Err(e) => {
                log::warn!("drag_out: SHDoDragDrop failed: {e}");
                false
            }
        }
    }
}
