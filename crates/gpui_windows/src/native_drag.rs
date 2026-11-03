//! Windows outbound file drag: the platform half of GPUI's
//! `Interactivity::external_drag_payload`.
//!
//! When an internal drag carrying an external payload leaves the window, GPUI
//! calls `PlatformWindow::start_external_drag`; the window posts the paths to
//! itself (`WM_GPUI_EXTERNAL_DRAG`) and `do_external_drag` runs the OLE drag
//! from a clean message-processing context. The COM objects here
//! (`IDropSource`, a `CF_HDROP` `IDataObject`) are what `SHDoDragDrop` needs.

use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::{
    Win32::{
        Foundation::*,
        System::{
            Com::{
                FORMATETC, IDataObject, IDataObject_Impl, IEnumFORMATETC, IStream, STGMEDIUM,
                TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            Memory::*,
            Ole::*,
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
        },
        UI::Shell::{DROPFILES, SHCreateStdEnumFmtEtc, SHDoDragDrop},
    },
    core::*,
};

/// The paths of one outbound drag, boxed into the `LPARAM` of
/// `WM_GPUI_EXTERNAL_DRAG` so `SHDoDragDrop` runs outside GPUI's borrows.
pub(crate) struct ExternalDragRequest {
    pub paths: Vec<PathBuf>,
}

const DVASPECT_CONTENT: u32 = 1;

// ---------------------------------------------------------------------------
// IDropSource implementation
// ---------------------------------------------------------------------------

#[implement(IDropSource)]
struct DropSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
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
            let source_stream: std::mem::ManuallyDrop<Option<IStream>> =
                unsafe { std::ptr::read(&source_entry.medium.u.pstm) };
            if let Some(stream) = &*source_stream {
                let cloned_stream = unsafe { stream.Clone()? };
                copied_medium.u.pstm = std::mem::ManuallyDrop::new(Some(cloned_stream));
                return Ok(copied_medium);
            }
            return Err(Error::new(E_FAIL, "Empty IStream"));
        } else {
            // Unhandled TYMED for deep cloning
            Err(Error::new(
                DV_E_TYMED,
                "GetData clone not implemented for TYMED",
            ))
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
            fmt.cfFormat,
            fmt.dwAspect,
            fmt.tymed,
            medium.tymed,
            frelease.0
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
                let source_stream: std::mem::ManuallyDrop<Option<IStream>> =
                    unsafe { std::ptr::read(&medium.u.pstm) };
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
        Err(Error::new(
            OLE_E_ADVISENOTSUPPORTED,
            "DAdvise not supported",
        ))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(Error::new(
            OLE_E_ADVISENOTSUPPORTED,
            "DUnadvise not supported",
        ))
    }

    fn EnumDAdvise(&self) -> Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(Error::new(
            OLE_E_ADVISENOTSUPPORTED,
            "EnumDAdvise not supported",
        ))
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
// Public entry point
// ---------------------------------------------------------------------------

/// Run an OLE drag of `paths` and return once it is dropped or cancelled.
///
/// **Must be called on the UI thread** (OLE is initialized there at platform
/// startup). `SHDoDragDrop` runs a nested message loop until the drag ends,
/// so the caller must hold no GPUI borrow — hence the posted message.
///
/// The drag image is the Shell's: `hwnd` is `None` and the data object
/// carries none, so the Shell provides a generic one (Vista and later,
/// per the `SHDoDragDrop` documentation) — as the macOS and Wayland halves
/// leave the image to their platforms. Only copy is offered: with move
/// allowed, Explorer moves on the same volume by default and takes the
/// files out from under the panel they were dragged from (macOS offers
/// copy alone outside the app for the same reason).
pub(crate) fn do_external_drag(paths: Vec<PathBuf>) -> anyhow::Result<()> {
    if paths.is_empty() {
        anyhow::bail!("no paths to drag");
    }
    let data_object: IDataObject = FileDataObject::new(&paths)?.into();
    let drop_source: IDropSource = DropSource.into();
    let effect = unsafe { SHDoDragDrop(None, &data_object, &drop_source, DROPEFFECT_COPY)? };
    log::debug!("external drag ended with effect {}", effect.0);
    Ok(())
}
