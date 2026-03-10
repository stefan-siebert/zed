//! Windows native drag-and-drop source implementation.
//!
//! Provides COM objects (`IDropSource`, `IDataObject`) needed to initiate an
//! outbound OLE drag operation via `DoDragDrop`, allowing files to be dragged
//! from the application to external Windows programs (Explorer, etc.).

use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use gpui::{NativeDragIcon, NativeDragMode, NativeDragResult};

/// Data passed via `PostMessageW` to defer `DoDragDrop` outside of GPUI borrows.
pub(crate) struct NativeDragRequest {
    pub paths: Vec<PathBuf>,
    pub icon: Option<NativeDragIcon>,
    pub mode: NativeDragMode,
    pub callback: Box<dyn FnOnce(NativeDragResult) + Send>,
}
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::{
            CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS, HBITMAP,
        },
        System::{
            Com::{
                IStream, 
                FORMATETC, IDataObject, IDataObject_Impl, IEnumFORMATETC, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            Memory::*,
            Ole::*,
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
        },
        UI::Shell::{
            CLSID_DragDropHelper, DROPFILES, IDragSourceHelper, SHDRAGIMAGE, SHCreateStdEnumFmtEtc,
        },
    },
    core::*,
};

const DVASPECT_CONTENT: u32 = 1;

// ---------------------------------------------------------------------------
// IDropSource implementation
// ---------------------------------------------------------------------------

#[implement(IDropSource)]
struct DropSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !grfkeystate.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// ---------------------------------------------------------------------------
// IDataObject implementation — serves CF_HDROP containing file paths
// ---------------------------------------------------------------------------

/// A stored format entry (FORMATETC + STGMEDIUM pair).
struct StoredMedium {
    fmt: FORMATETC,
    medium: STGMEDIUM,
}

impl Drop for StoredMedium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.medium);
        }
    }
}

#[implement(IDataObject)]
struct FileDataObject {
    /// Additional formats stored via SetData (e.g. drag images from IDragSourceHelper).
    extra: RefCell<Vec<StoredMedium>>,
}

impl FileDataObject {
    fn new(paths: &[PathBuf]) -> anyhow::Result<Self> {
        let hglobal = build_cf_hdrop(paths)?;
        let mut hdrop_medium = STGMEDIUM::default();
        hdrop_medium.tymed = TYMED_HGLOBAL.0 as u32;
        hdrop_medium.u.hGlobal = hglobal;

        let hdrop_entry = StoredMedium {
            fmt: FORMATETC {
                cfFormat: CF_HDROP.0,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            },
            medium: hdrop_medium,
        };
        Ok(Self {
            extra: RefCell::new(vec![hdrop_entry]),
        })
    }

    /// Find a stored entry matching the requested format.
    /// Does not transfer ownership.
    fn find_entry(&self, pformatetc: *const FORMATETC) -> Option<usize> {
        let fmt = unsafe { &*pformatetc };
        let entries = self.extra.borrow();
        for (index, entry) in entries.iter().enumerate() {
            if entry.fmt.cfFormat == fmt.cfFormat
                && (fmt.tymed & entry.fmt.tymed) != 0
                && (fmt.dwAspect == 0 || entry.fmt.dwAspect == fmt.dwAspect)
            {
                return Some(index);
            }
        }
        None
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetc: *const FORMATETC) -> Result<STGMEDIUM> {
        let index = self
            .find_entry(pformatetc)
            .ok_or_else(|| Error::new(DV_E_FORMATETC, "unsupported format"))?;
        
        let entries = self.extra.borrow();
        let source_entry = &entries[index];
        let medium_type = source_entry.medium.tymed;

        let mut copied_medium = STGMEDIUM::default();
        copied_medium.tymed = medium_type;
        copied_medium.pUnkForRelease = std::mem::ManuallyDrop::new(None);

        if medium_type == TYMED_HGLOBAL.0 as u32 {
            let source_hglobal = unsafe { source_entry.medium.u.hGlobal };
            let size = unsafe { GlobalSize(source_hglobal) };
            let src = unsafe { GlobalLock(source_hglobal) };
            if src.is_null() {
                return Err(Error::new(E_OUTOFMEMORY, "GlobalLock failed"));
            }

            let copy = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size)? };
            let dst = unsafe { GlobalLock(copy) };
            if dst.is_null() {
                unsafe {
                    GlobalUnlock(source_hglobal).ok();
                    let _ = GlobalFree(Some(copy));
                }
                return Err(Error::new(E_OUTOFMEMORY, "GlobalLock failed on copy"));
            }

            unsafe {
                std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size);
                GlobalUnlock(copy).ok();
                GlobalUnlock(source_hglobal).ok();
            }
            copied_medium.u.hGlobal = copy;
            Ok(copied_medium)
        } else if medium_type == TYMED_ISTREAM.0 as u32 {
            let source_stream: std::mem::ManuallyDrop<Option<IStream>> = unsafe { std::ptr::read(&source_entry.medium.u.pstm) };
            if let Some(stream) = &*source_stream {
                let cloned_stream = unsafe { stream.Clone()? };
                copied_medium.u.pstm = std::mem::ManuallyDrop::new(Some(cloned_stream));
                return Ok(copied_medium);
            }
            return Err(Error::new(E_FAIL, "Empty IStream"));
        } else {
            // Unhandled TYMED for deep cloning
            Err(Error::new(DV_E_TYMED, "GetData clone not implemented for TYMED"))
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(Error::new(E_NOTIMPL, "GetDataHere not supported"))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if self.find_entry(pformatetc).is_some() {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        unsafe {
            (*pformatetcout).ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> Result<()> {
        let fmt = unsafe { &*pformatetc };
        let medium = unsafe { &*pmedium };

        log::debug!(
            "IDataObject::SetData called: cfFormat={}, dwAspect={}, tymed={}, medium.tymed={}, frelease={}",
            fmt.cfFormat, fmt.dwAspect, fmt.tymed, medium.tymed, frelease.0
        );

        // STGMEDIUM must be fully owned since it will be kept in memory for the duration of the drag.
        let owned_medium = if frelease.as_bool() {
            // The shell gave us ownership.
            let mut stg = STGMEDIUM::default();
            stg.tymed = medium.tymed;
            
            // Transfer unions manually based on tymed. This is unsafe but Windows OLE relies on this layout.
            // When taking ownership, we must copy the fields directly.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pmedium as *const u8,
                    &mut stg as *mut _ as *mut u8,
                    std::mem::size_of::<STGMEDIUM>(),
                );
            }
            stg
        } else {
            // Need to deeply clone the medium from the shell.
            let mut stg = STGMEDIUM::default();
            stg.tymed = medium.tymed;
            stg.pUnkForRelease = std::mem::ManuallyDrop::new(None);

            if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                let source_hglobal = unsafe { medium.u.hGlobal };
                if source_hglobal.0.is_null() {
                    return Err(Error::new(E_INVALIDARG, "null HGLOBAL"));
                }
                let size = unsafe { GlobalSize(source_hglobal) };
                let src = unsafe { GlobalLock(source_hglobal) };
                if src.is_null() {
                    return Err(Error::new(E_OUTOFMEMORY, "GlobalLock failed"));
                }
                let copy = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size)? };
                let dst = unsafe { GlobalLock(copy) };
                if dst.is_null() {
                    unsafe {
                        GlobalUnlock(source_hglobal).ok();
                        let _ = GlobalFree(Some(copy));
                    }
                    return Err(Error::new(E_OUTOFMEMORY, "GlobalLock failed on copy"));
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, size);
                    GlobalUnlock(copy).ok();
                    GlobalUnlock(source_hglobal).ok();
                }
                stg.u.hGlobal = copy;
            } else if medium.tymed == TYMED_ISTREAM.0 as u32 {
                let source_stream: std::mem::ManuallyDrop<Option<IStream>> = unsafe { std::ptr::read(&medium.u.pstm) };
                if let Some(stream) = &*source_stream {
                    let cloned_stream = unsafe { stream.Clone()? };
                    stg.u.pstm = std::mem::ManuallyDrop::new(Some(cloned_stream));
                } else {
                    return Err(Error::new(E_INVALIDARG, "null IStream"));
                }
            } else {
                log::warn!("IDataObject::SetData rejected format (unsupported TYMED duplicate)");
                return Err(Error::new(DV_E_TYMED, "TYMED duplicate not supported"));
            }
            stg
        };

        // Remove any existing entry with the same cfFormat.
        let mut entries = self.extra.borrow_mut();
        entries.retain(|e| e.fmt.cfFormat != fmt.cfFormat);
        entries.push(StoredMedium {
            fmt: FORMATETC {
                cfFormat: fmt.cfFormat,
                ptd: std::ptr::null_mut(),
                dwAspect: fmt.dwAspect,
                lindex: fmt.lindex,
                tymed: medium.tymed, // The item tymed
            },
            medium: owned_medium,
        });

        Ok(())
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> Result<IEnumFORMATETC> {
        let entries = self.extra.borrow();
        let fmts: Vec<FORMATETC> = entries.iter().map(|e| e.fmt).collect();
        unsafe { SHCreateStdEnumFmtEtc(&fmts) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: windows::core::Ref<'_, windows::Win32::System::Com::IAdviseSink>,
    ) -> Result<u32> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, "DAdvise not supported"))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, "DUnadvise not supported"))
    }

    fn EnumDAdvise(&self) -> Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(Error::new(OLE_E_ADVISENOTSUPPORTED, "EnumDAdvise not supported"))
    }
}

// ---------------------------------------------------------------------------
// Build a CF_HDROP HGLOBAL from file paths
// ---------------------------------------------------------------------------

/// Build an HGLOBAL containing a `DROPFILES` structure followed by
/// null-terminated wide-char file paths, terminated by a double-null.
fn build_cf_hdrop(paths: &[PathBuf]) -> anyhow::Result<HGLOBAL> {
    let mut wide_data: Vec<u16> = Vec::new();
    for path in paths {
        let path_str: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0u16))
            .collect();
        wide_data.extend_from_slice(&path_str);
    }
    wide_data.push(0u16); // double-null terminator

    let header_size = std::mem::size_of::<DROPFILES>();
    let data_bytes = wide_data.len() * std::mem::size_of::<u16>();
    let total_size = header_size + data_bytes;

    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total_size)? };
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        unsafe {
            let _ = GlobalFree(Some(hglobal));
        }
        anyhow::bail!("GlobalLock failed");
    }

    unsafe {
        let drop_files = ptr as *mut DROPFILES;
        (*drop_files).pFiles = header_size as u32;
        (*drop_files).fWide = BOOL(1); // Unicode paths

        let dest = (ptr as *mut u8).add(header_size);
        std::ptr::copy_nonoverlapping(wide_data.as_ptr() as *const u8, dest, data_bytes);

        GlobalUnlock(hglobal).ok();
    }

    Ok(hglobal)
}

// ---------------------------------------------------------------------------
// Drag icon → HBITMAP conversion
// ---------------------------------------------------------------------------

/// Convert a `NativeDragIcon` (ARGB8888 pre-multiplied) to a 32-bit DIB
/// section HBITMAP suitable for `IDragSourceHelper::InitializeFromBitmap`.
///
/// The shell requires a DIB section (not a DDB from `CreateBitmap`) so that
/// the alpha channel is preserved for the drag image overlay.
fn create_drag_bitmap(icon: &NativeDragIcon) -> anyhow::Result<HBITMAP> {
    let width = icon.width as i32;
    let height = icon.height as i32;

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height = top-down DIB (row 0 is the top, matching our pixel layout).
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbitmap = unsafe {
        CreateDIBSection(
            None,
            &mut bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        )?
    };

    if bits_ptr.is_null() {
        anyhow::bail!("CreateDIBSection returned null bits pointer");
    }

    // Copy our pre-multiplied ARGB pixels into the DIB section.
    let pixel_count = (width * height) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(
            icon.pixels.as_ptr(),
            bits_ptr as *mut u32,
            pixel_count.min(icon.pixels.len()),
        );
    }

    Ok(hbitmap)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Perform an OLE drag-and-drop operation with the given file paths.
///
/// **Must be called from a thread with OLE initialized** (the main/UI thread
/// satisfies this since `OleInitialize` is called at platform startup).
///
/// `DoDragDrop` is a blocking call — it runs a nested message loop until the
/// user drops or cancels. The caller is responsible for ensuring this is
/// acceptable (e.g. calling from the UI thread during a mouse-drag gesture).
pub(crate) fn do_native_drag(
    paths: Vec<PathBuf>,
    icon: Option<NativeDragIcon>,
    mode: NativeDragMode,
    callback: Box<dyn FnOnce(NativeDragResult) + Send>,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        anyhow::bail!("no paths to drag");
    }

    let data_object: IDataObject = FileDataObject::new(&paths)?.into();
    let drop_source: IDropSource = DropSource.into();

    // Attach a drag image via IDragSourceHelper if we have an icon.
    let mut drag_bitmap: Option<HBITMAP> = None;
    if let Some(ref icon_data) = icon {
        if let Ok(helper) = create_drag_source_helper() {
            if let Ok(hbitmap) = create_drag_bitmap(icon_data) {
                let sdi = SHDRAGIMAGE {
                    sizeDragImage: SIZE {
                        cx: icon_data.width as i32,
                        cy: icon_data.height as i32,
                    },
                    ptOffset: POINT {
                        x: icon_data.offset.x.0,
                        y: icon_data.offset.y.0,
                    },
                    hbmpDragImage: hbitmap,
                    crColorKey: COLORREF(0xFFFFFFFF), // CLR_NONE — use per-pixel alpha
                };
                unsafe {
                    match helper.InitializeFromBitmap(&sdi, &data_object) {
                        Ok(()) => {
                            log::debug!("IDragSourceHelper::InitializeFromBitmap succeeded");
                            drag_bitmap = None;
                        }
                        Err(err) => {
                            log::warn!("IDragSourceHelper::InitializeFromBitmap failed: {err}");
                            drag_bitmap = Some(hbitmap);
                        }
                    }
                }
            }
        }
    }

    let allowed_effects = match mode {
        NativeDragMode::Copy => DROPEFFECT_COPY,
        NativeDragMode::Move => DROPEFFECT_MOVE,
    };

    let mut result_effect = DROPEFFECT_NONE;
    let hr = unsafe { DoDragDrop(&data_object, &drop_source, allowed_effects, &mut result_effect) };

    if let Some(hbm) = drag_bitmap {
        unsafe {
            let _ = DeleteObject(hbm.into());
        }
    }

    let result = if hr == DRAGDROP_S_DROP {
        NativeDragResult::Dropped
    } else {
        NativeDragResult::Cancel
    };

    callback(result);
    Ok(())
}

fn create_drag_source_helper() -> anyhow::Result<IDragSourceHelper> {
    unsafe {
        let helper: IDragSourceHelper = windows::Win32::System::Com::CoCreateInstance(
            &CLSID_DragDropHelper,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )?;
        Ok(helper)
    }
}
