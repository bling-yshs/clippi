//! Native drag-out of clipboard history items to other applications.
//!
//! Windows only for now. Clippi implements the OLE data object, its format
//! enumerator and the drop source itself and calls `ole32!DoDragDrop`
//! directly. The Shell helpers (`SHCreateDataObject` / `SHDoDragDrop`) are
//! deliberately not used: their data object answers Shell-specific formats
//! for an empty item list, and a Remote Desktop session (which intercepts OLE
//! drags for cross-machine drag-and-drop) crashed inside shell32 reading
//! through a null pointer. Here the drop target only ever sees the formats
//! listed in the payload.
//!
//! Only `DROPEFFECT_COPY` is offered so a drop on Explorer always copies — it
//! must never move the backing image file out of Clippi's storage directory.

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
        self.files.is_empty() && self.text.as_deref().is_none_or(str::is_empty)
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
    use std::cell::{Cell, RefCell};
    use windows::core::{implement, w, Error, Result, HRESULT, PCWSTR};
    use windows::Win32::Foundation::{
        GlobalFree, BOOL, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
        DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY,
        HGLOBAL, OLE_E_ADVISENOTSUPPORTED, POINT, S_FALSE, S_OK,
    };
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
        IEnumSTATDATA, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0,
        TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND};
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, ReleaseStgMedium, CF_HDROP, CF_UNICODETEXT,
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::DROPFILES;

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

    // --- Payload encoding -------------------------------------------------

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
            bytes.extend(
                path.encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes),
            );
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    /// Every (format, bytes) pair the data object serves, all as HGLOBAL.
    fn payload_formats(payload: &DragPayload) -> Vec<(u16, Vec<u8>)> {
        let mut formats = Vec::new();
        if !payload.files.is_empty() {
            formats.push((CF_HDROP.0, hdrop_bytes(&payload.files)));
        }
        if let Some(text) = payload.text.as_deref().filter(|text| !text.is_empty()) {
            formats.push((CF_UNICODETEXT.0, utf16z_bytes(text)));
            if let Some(html) = payload.html.as_deref().filter(|html| !html.is_empty()) {
                let cf_html = crate::core::html_text::encode_cf_html(html);
                formats.push((register_format(w!("HTML Format")), nul_terminated(&cf_html)));
            }
            if let Some(rtf) = payload.rtf.as_deref().filter(|rtf| !rtf.is_empty()) {
                formats.push((register_format(w!("Rich Text Format")), nul_terminated(rtf)));
            }
        }
        if !formats.is_empty() {
            // Ask Explorer to copy rather than move.
            formats.push((
                register_format(w!("Preferred DropEffect")),
                DROPEFFECT_COPY.0.to_le_bytes().to_vec(),
            ));
        }
        formats.retain(|(format, _)| *format != 0);
        formats
    }

    fn hglobal_format_etc(format: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    /// A fresh HGLOBAL copy of `bytes`; ownership passes to the caller.
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

    /// Copy the contents of an HGLOBAL.
    fn read_global(handle: HGLOBAL) -> Option<Vec<u8>> {
        unsafe {
            let size = GlobalSize(handle);
            let ptr = GlobalLock(handle) as *const u8;
            if ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(handle);
            Some(bytes)
        }
    }

    // --- IDataObject ------------------------------------------------------

    /// Data object serving a fixed list of HGLOBAL formats. Calls arrive on
    /// the UI (STA) thread — directly, or marshaled from the drop target's
    /// process while `DoDragDrop` pumps messages — so interior mutability
    /// via `RefCell` is sufficient.
    #[implement(IDataObject)]
    struct DragDataObject {
        formats: RefCell<Vec<(u16, Vec<u8>)>>,
    }

    impl DragDataObject {
        fn find(&self, format_etc: *const FORMATETC) -> std::result::Result<Vec<u8>, HRESULT> {
            let Some(format_etc) = (unsafe { format_etc.as_ref() }) else {
                return Err(E_INVALIDARG);
            };
            if format_etc.tymed & TYMED_HGLOBAL.0 as u32 == 0
                || format_etc.dwAspect != DVASPECT_CONTENT.0
            {
                return Err(DV_E_FORMATETC);
            }
            self.formats
                .borrow()
                .iter()
                .find(|(format, _)| *format == format_etc.cfFormat)
                .map(|(_, bytes)| bytes.clone())
                .ok_or(DV_E_FORMATETC)
        }
    }

    impl IDataObject_Impl for DragDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
            let bytes = self.find(pformatetcin).map_err(Error::from)?;
            let handle = alloc_global(&bytes).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: handle },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }

        fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            match self.find(pformatetc) {
                Ok(_) => S_OK,
                Err(hr) => hr,
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if let Some(out) = unsafe { pformatetcout.as_mut() } {
                out.ptd = std::ptr::null_mut();
            }
            DATA_S_SAMEFORMATETC
        }

        /// Drop targets (Explorer among them) report back through SetData
        /// ("Performed DropEffect", "Paste Succeeded", …). Keep HGLOBAL data;
        /// refuse anything else.
        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> Result<()> {
            let (Some(format_etc), Some(medium)) =
                (unsafe { pformatetc.as_ref() }, unsafe { pmedium.as_ref() })
            else {
                return Err(E_INVALIDARG.into());
            };
            if medium.tymed != TYMED_HGLOBAL.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let bytes = read_global(unsafe { medium.u.hGlobal })
                .ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
            {
                let mut formats = self.formats.borrow_mut();
                formats.retain(|(format, _)| *format != format_etc.cfFormat);
                formats.push((format_etc.cfFormat, bytes));
            }
            if frelease.as_bool() {
                unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
            }
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let formats = self
                .formats
                .borrow()
                .iter()
                .map(|(format, _)| hglobal_format_etc(*format))
                .collect();
            Ok(FormatEnumerator {
                formats,
                position: Cell::new(0),
            }
            .into())
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Option<&IAdviseSink>,
        ) -> Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    // --- IEnumFORMATETC ---------------------------------------------------

    #[implement(IEnumFORMATETC)]
    struct FormatEnumerator {
        formats: Vec<FORMATETC>,
        position: Cell<usize>,
    }

    impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
        fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
            if rgelt.is_null() || (celt > 1 && pceltfetched.is_null()) {
                return E_INVALIDARG;
            }
            let start = self.position.get().min(self.formats.len());
            let count = (celt as usize).min(self.formats.len() - start);
            for (offset, format) in self.formats[start..start + count].iter().enumerate() {
                unsafe { *rgelt.add(offset) = *format };
            }
            self.position.set(start + count);
            if let Some(fetched) = unsafe { pceltfetched.as_mut() } {
                *fetched = count as u32;
            }
            if count == celt as usize {
                S_OK
            } else {
                S_FALSE
            }
        }

        fn Skip(&self, celt: u32) -> Result<()> {
            let target = self.position.get() + celt as usize;
            self.position.set(target.min(self.formats.len()));
            if target <= self.formats.len() {
                Ok(())
            } else {
                Err(S_FALSE.into())
            }
        }

        fn Reset(&self) -> Result<()> {
            self.position.set(0);
            Ok(())
        }

        fn Clone(&self) -> Result<IEnumFORMATETC> {
            Ok(FormatEnumerator {
                formats: self.formats.clone(),
                position: Cell::new(self.position.get()),
            }
            .into())
        }
    }

    // --- IDropSource ------------------------------------------------------

    /// Standard drop source: Esc cancels, releasing the left button drops,
    /// and the system's default drag cursors are used.
    #[implement(IDropSource)]
    struct DragSource;

    impl IDropSource_Impl for DragSource_Impl {
        fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
            if fescapepressed.as_bool() {
                DRAGDROP_S_CANCEL
            } else if grfkeystate.0 & MK_LBUTTON.0 == 0 {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }

        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    // --- Drag ---------------------------------------------------------------

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
        let formats = payload_formats(payload);
        if formats.is_empty() {
            return false;
        }
        let data: IDataObject = DragDataObject {
            formats: RefCell::new(formats),
        }
        .into();
        let source: IDropSource = DragSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
        if hr.is_err() {
            log::warn!("drag_out: DoDragDrop failed: {hr:?}");
            return false;
        }
        hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE && !cursor_over_own_window(hwnd)
    }
}
