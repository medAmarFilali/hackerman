//! Files on the clipboard that are still on another computer.
//!
//! Windows calls these virtual files. What goes on the clipboard is an object that
//! describes them and hands out a stream for each. File Explorer asks for those when
//! something is pasted, and shows its usual progress and handles its usual choices
//! about name clashes. So however large the files are, nothing moves and no disk space
//! is used until then, and the file is written once, straight to where it was pasted.
//!
//! Only programs that understand this format can paste these. That includes File
//! Explorer, and not every program that accepts a file copied from File Explorer.
//!
//! COM objects like these belong to the thread that made them, which has to keep
//! waiting for messages, so they get a thread of their own. Other programs' calls
//! arrive on it one at a time, so a read that waits on the network holds up the others,
//! which for a paste is only the next read.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::time::Duration;

use hk_core::clipboard::SharedFile;
use windows::Win32::Foundation::{
    CLIPBRD_E_CANT_OPEN, DATA_S_SAMEFORMATETC, DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX,
    DV_E_TYMED, E_FAIL, E_NOTIMPL, E_POINTER, FILETIME, GlobalFree, HGLOBAL, LPARAM,
    OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
    STG_E_READFAULT, STG_E_SEEKERROR, WPARAM,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
    STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, OleInitialize, OleSetClipboard, ReleaseStgMedium,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES,
    FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_APP,
    WM_USER,
};
use windows::core::{HRESULT, Result as WinResult};
use windows_core::{BOOL, Ref, implement};

use super::clipboard::own_write;
use crate::{Error, FileSource};

/// Tells the thread there's something new to put on the clipboard.
const NEW_FILES: u32 = WM_APP + 1;

/// The clipboard is often busy for a moment, as other programs look at what's new.
const ATTEMPTS: u32 = 20;
const BETWEEN_ATTEMPTS: Duration = Duration::from_millis(25);

/// Between 1601, when Windows' file times begin, and 1970.
const FILETIME_MS_BEFORE_1970: u64 = 11_644_473_600_000;

struct NewFiles {
    files: Vec<SharedFile>,
    source: Arc<dyn FileSource>,
    done: mpsc::Sender<Result<(), Error>>,
}

struct Thread {
    id: u32,
    work: Mutex<mpsc::Sender<NewFiles>>,
}

/// Puts files on the clipboard, which stay there until something else is copied or
/// this program ends.
pub(super) fn put_on_clipboard(
    files: Vec<SharedFile>,
    source: Arc<dyn FileSource>,
) -> Result<(), Error> {
    static THREAD: OnceLock<Result<Thread, String>> = OnceLock::new();
    let thread = THREAD
        .get_or_init(start_thread)
        .as_ref()
        .map_err(|error| Error::Clipboard(error.clone()))?;

    let (done_tx, done_rx) = mpsc::channel();
    let work = NewFiles {
        files,
        source,
        done: done_tx,
    };
    let stopped = |_| Error::Clipboard("the thread for files on the clipboard stopped".into());
    thread
        .work
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .send(work)
        .map_err(|_| stopped(()))?;
    let _ = unsafe { PostThreadMessageW(thread.id, NEW_FILES, WPARAM(0), LPARAM(0)) };
    done_rx.recv().map_err(|_| stopped(()))?
}

fn start_thread() -> Result<Thread, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (work_tx, work_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard-files".into())
        .spawn(move || thread_main(&ready_tx, &work_rx))
        .map_err(|error| error.to_string())?;
    let id = ready_rx
        .recv()
        .map_err(|_| "the thread for files on the clipboard stopped".to_string())??;
    Ok(Thread {
        id,
        work: Mutex::new(work_tx),
    })
}

fn thread_main(ready: &mpsc::Sender<Result<u32, String>>, work: &mpsc::Receiver<NewFiles>) {
    if let Err(error) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(error.message()));
        return;
    }
    // Create this thread's message queue before anyone posts to it.
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    // Other programs' calls to the objects here arrive as messages.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.message == NEW_FILES && message.hwnd.is_invalid() {
            while let Ok(new) = work.try_recv() {
                let result = set_clipboard(&data_object(new.files, new.source));
                let _ = new.done.send(result);
            }
        } else {
            unsafe { DispatchMessageW(&message) };
        }
    }
}

fn set_clipboard(object: &IDataObject) -> Result<(), Error> {
    let mut attempt = 0;
    loop {
        let result = own_write(|| unsafe { OleSetClipboard(object) });
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.code() == CLIPBRD_E_CANT_OPEN && attempt < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(BETWEEN_ATTEMPTS);
            }
            Err(error) => return Err(Error::Clipboard(error.message())),
        }
    }
}

/// The object describing these files, for the clipboard or anything else that takes one.
pub fn data_object(files: Vec<SharedFile>, source: Arc<dyn FileSource>) -> IDataObject {
    FilesOnClipboard {
        descriptor: registered_format(CFSTR_FILEDESCRIPTORW),
        contents: registered_format(CFSTR_FILECONTENTS),
        drop_effect: registered_format(CFSTR_PREFERREDDROPEFFECT),
        files,
        source,
    }
    .into()
}

fn registered_format(name: windows::core::PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

fn filetime(ms_since_1970: u64) -> FILETIME {
    let ticks = (ms_since_1970 + FILETIME_MS_BEFORE_1970).saturating_mul(10_000);
    FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    }
}

fn describe(file: &SharedFile) -> FILEDESCRIPTORW {
    let mut descriptor = FILEDESCRIPTORW {
        dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
        dwFileAttributes: if file.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        },
        nFileSizeHigh: (file.size >> 32) as u32,
        nFileSizeLow: file.size as u32,
        ..Default::default()
    };
    if let Some(modified) = file.modified_ms {
        descriptor.dwFlags |= FD_WRITESTIME.0 as u32;
        descriptor.ftLastWriteTime = filetime(modified);
    }
    // Paths are checked before they get here, so this always fits, ending in a zero.
    let mut name = [0; 260];
    let path = file.path.replace('/', "\\");
    for (slot, unit) in name.iter_mut().take(259).zip(path.encode_utf16()) {
        *slot = unit;
    }
    descriptor.cFileName = name;
    descriptor
}

/// Memory for the clipboard, filled in by `fill`, which is given its address.
fn global_memory(size: usize, fill: impl FnOnce(*mut u8)) -> WinResult<HGLOBAL> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, size)?;
        let address = GlobalLock(memory);
        if address.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(E_FAIL.into());
        }
        fill(address.cast());
        // Reports an error when the memory is no longer locked, which is the aim.
        let _ = GlobalUnlock(memory);
        Ok(memory)
    }
}

fn memory_medium(memory: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: memory },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    }
}

#[implement(IDataObject)]
struct FilesOnClipboard {
    descriptor: u16,
    contents: u16,
    drop_effect: u16,
    files: Vec<SharedFile>,
    source: Arc<dyn FileSource>,
}

impl FilesOnClipboard {
    /// Which medium would be used for a format, if it's one this has.
    fn medium_for(&self, format: &FORMATETC) -> Result<u32, HRESULT> {
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT);
        }
        let medium = if format.cfFormat == self.contents {
            TYMED_ISTREAM.0 as u32
        } else if format.cfFormat == self.descriptor || format.cfFormat == self.drop_effect {
            TYMED_HGLOBAL.0 as u32
        } else {
            return Err(DV_E_FORMATETC);
        };
        if format.tymed & medium == 0 {
            return Err(DV_E_TYMED);
        }
        if format.cfFormat == self.contents {
            let file = usize::try_from(format.lindex)
                .ok()
                .and_then(|index| self.files.get(index));
            if file.is_none_or(|file| file.is_dir) {
                return Err(DV_E_LINDEX);
            }
        }
        Ok(medium)
    }

    fn descriptor_memory(&self) -> WinResult<HGLOBAL> {
        const HEADER: usize = size_of::<u32>();
        let size = HEADER + self.files.len() * size_of::<FILEDESCRIPTORW>();
        global_memory(size, |address| unsafe {
            address.cast::<u32>().write(self.files.len() as u32);
            let descriptors = address.add(HEADER).cast::<FILEDESCRIPTORW>();
            for (index, file) in self.files.iter().enumerate() {
                descriptors.add(index).write(describe(file));
            }
        })
    }

    fn drop_effect_memory(&self) -> WinResult<HGLOBAL> {
        global_memory(size_of::<u32>(), |address| unsafe {
            address.cast::<u32>().write(DROPEFFECT_COPY.0);
        })
    }
}

impl IDataObject_Impl for FilesOnClipboard_Impl {
    fn GetData(&self, format: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(E_POINTER)?;
        self.medium_for(format)?;
        if format.cfFormat == self.descriptor {
            Ok(memory_medium(self.descriptor_memory()?))
        } else if format.cfFormat == self.drop_effect {
            Ok(memory_medium(self.drop_effect_memory()?))
        } else {
            let index = format.lindex as usize;
            let stream: IStream = RemoteStream {
                source: self.source.clone(),
                index,
                name: self.files[index].path.clone(),
                size: self.files[index].size,
                position: Cell::new(0),
                reader: RefCell::new(None),
            }
            .into();
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: std::mem::ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }
    }

    fn GetDataHere(&self, _format: *const FORMATETC, _medium: *mut STGMEDIUM) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        match unsafe { format.as_ref() } {
            None => E_POINTER,
            // Without a position, this is asking if there are contents at all.
            Some(format) => {
                let any_file = FORMATETC {
                    lindex: if format.cfFormat == self.contents && format.lindex < 0 {
                        self.files
                            .iter()
                            .position(|file| !file.is_dir)
                            .map_or(-1, |index| index as i32)
                    } else {
                        format.lindex
                    },
                    ..*format
                };
                match self.medium_for(&any_file) {
                    Ok(_) => S_OK,
                    Err(error) => error,
                }
            }
        }
    }

    fn GetCanonicalFormatEtc(&self, _in: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    /// File Explorer reports how a paste went here. That's nothing to act on, since
    /// the files on the other computer stay as they are whatever it did.
    fn SetData(
        &self,
        _format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> WinResult<()> {
        if release.as_bool() && !medium.is_null() {
            unsafe { ReleaseStgMedium(medium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        const DATADIR_GET: u32 = 1;
        if direction != DATADIR_GET {
            return Err(E_NOTIMPL.into());
        }
        let format = |format: u16, medium: i32, lindex: i32| FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: medium as u32,
        };
        unsafe {
            SHCreateStdEnumFmtEtc(&[
                format(self.descriptor, TYMED_HGLOBAL.0, -1),
                format(self.contents, TYMED_ISTREAM.0, -1),
                format(self.drop_effect, TYMED_HGLOBAL.0, -1),
            ])
        }
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: Ref<IAdviseSink>,
    ) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _connection: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// The content of one file, read as something asks for it.
#[implement(IStream)]
struct RemoteStream {
    source: Arc<dyn FileSource>,
    index: usize,
    name: String,
    size: u64,
    position: Cell<u64>,
    /// Starts when the first read comes, from wherever the position is by then.
    reader: RefCell<Option<Box<dyn Read + Send>>>,
}

impl RemoteStream {
    /// As much as fits, unless the file ends first or reading fails. Callers can take a
    /// short read to mean the end, so this keeps going until there's nothing left.
    fn read_into(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Ok(mut reader) = self.reader.try_borrow_mut() else {
            return Err(std::io::Error::other("already reading"));
        };
        let reader =
            reader.get_or_insert_with(|| self.source.open(self.index, self.position.get()));
        let mut total = 0;
        while total < buffer.len() {
            // Windows called this, and a panic mustn't go back into its code. Reading can
            // panic only as this program stops, when nobody will be reading it anyway.
            let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reader.read(&mut buffer[total..])
            }))
            .unwrap_or_else(|_| Err(std::io::Error::other("this program is stopping")));
            match read {
                Ok(0) => break,
                Ok(count) => {
                    total += count;
                    self.position.set(self.position.get() + count as u64);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(total)
    }
}

impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, buffer: *mut core::ffi::c_void, wanted: u32, read: *mut u32) -> HRESULT {
        if buffer.is_null() {
            return E_POINTER;
        }
        let target =
            unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), wanted as usize) };
        match self.read_into(target) {
            Ok(count) => {
                if !read.is_null() {
                    unsafe { read.write(count as u32) };
                }
                if count == wanted as usize {
                    S_OK
                } else {
                    S_FALSE
                }
            }
            Err(error) => {
                tracing::warn!(file = self.name, %error, "pasting a file failed");
                if !read.is_null() {
                    unsafe { read.write(0) };
                }
                STG_E_READFAULT
            }
        }
    }

    fn Write(&self, _data: *const core::ffi::c_void, _size: u32, _written: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, distance: i64, origin: STREAM_SEEK, new_position: *mut u64) -> WinResult<()> {
        let from = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.position.get(),
            STREAM_SEEK_END => self.size,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let target = from.checked_add_signed(distance).ok_or(STG_E_SEEKERROR)?;
        if target != self.position.get() {
            self.position.set(target);
            let mut reader = self
                .reader
                .try_borrow_mut()
                .map_err(|_| windows::core::Error::from(STG_E_ACCESSDENIED))?;
            *reader = None;
        }
        if !new_position.is_null() {
            unsafe { new_position.write(target) };
        }
        Ok(())
    }

    fn SetSize(&self, _size: u64) -> WinResult<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        target: Ref<IStream>,
        wanted: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> WinResult<()> {
        let target = target.ok()?;
        let mut buffer = vec![0; 1024 * 1024];
        let (mut total_read, mut total_written) = (0, 0);
        while total_read < wanted {
            let piece = (wanted - total_read).min(buffer.len() as u64) as usize;
            let count = self
                .read_into(&mut buffer[..piece])
                .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
            if count == 0 {
                break;
            }
            total_read += count as u64;
            let mut accepted = 0;
            unsafe {
                target.Write(
                    buffer.as_ptr().cast(),
                    count as u32,
                    Some(&raw mut accepted),
                )
            }
            .ok()?;
            total_written += u64::from(accepted);
        }
        if !read.is_null() {
            unsafe { read.write(total_read) };
        }
        if !written.is_null() {
            unsafe { written.write(total_written) };
        }
        Ok(())
    }

    fn Commit(&self, _flags: &STGC) -> WinResult<()> {
        Ok(())
    }

    fn Revert(&self) -> WinResult<()> {
        Ok(())
    }

    fn LockRegion(&self, _offset: u64, _size: u64, _kind: &LOCKTYPE) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _offset: u64, _size: u64, _kind: u32) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, stat: *mut STATSTG, flags: &STATFLAG) -> WinResult<()> {
        let stat = unsafe { stat.as_mut() }.ok_or(E_POINTER)?;
        *stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size,
            ..Default::default()
        };
        if *flags != STATFLAG_NONAME {
            // Whoever asked frees this.
            let name: Vec<u16> = self.name.encode_utf16().chain([0]).collect();
            let copy = unsafe { CoTaskMemAlloc(name.len() * size_of::<u16>()) }.cast::<u16>();
            if copy.is_null() {
                return Err(E_FAIL.into());
            }
            unsafe { copy.copy_from_nonoverlapping(name.as_ptr(), name.len()) };
            stat.pwcsName = windows::core::PWSTR(copy);
        }
        Ok(())
    }

    fn Clone(&self) -> WinResult<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::System::Ole::{DROPEFFECT, IDropTarget, OleGetClipboard};
    use windows::Win32::System::SystemServices::MK_LBUTTON;
    use windows::Win32::UI::Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName};
    use windows::Win32::UI::WindowsAndMessaging::{PM_REMOVE, TranslateMessage};
    use windows::core::HSTRING;

    use super::*;

    #[test]
    fn a_descriptor_says_everything_about_a_large_file() {
        let descriptor = describe(&SharedFile {
            path: "photos/2024/été.mkv".into(),
            size: 5_000_000_000,
            is_dir: false,
            modified_ms: Some(0),
        });
        let (high, low) = (descriptor.nFileSizeHigh, descriptor.nFileSizeLow);
        let size = (u64::from(high) << 32) | u64::from(low);
        assert_eq!(size, 5_000_000_000);
        assert_eq!({ descriptor.dwFileAttributes }, FILE_ATTRIBUTE_NORMAL.0);
        let file_name = { descriptor.cFileName };
        let name_length = file_name.iter().position(|&unit| unit == 0);
        let name = String::from_utf16(&file_name[..name_length.unwrap()]).unwrap();
        assert_eq!(name, "photos\\2024\\été.mkv");
        let written = { descriptor.ftLastWriteTime };
        let ticks = (u64::from(written.dwHighDateTime) << 32) | u64::from(written.dwLowDateTime);
        assert_eq!(ticks, 116_444_736_000_000_000, "1970 in Windows' file time");
    }

    #[test]
    fn the_longest_allowed_path_keeps_its_terminator() {
        let descriptor = describe(&SharedFile {
            path: "a".repeat(hk_core::clipboard::MAX_SHARED_PATH),
            size: 0,
            is_dir: true,
            modified_ms: None,
        });
        let file_name = { descriptor.cFileName };
        assert_eq!(file_name[258], u16::from(b'a'));
        assert_eq!(file_name[259], 0);
        assert_eq!({ descriptor.dwFileAttributes }, FILE_ATTRIBUTE_DIRECTORY.0);
        assert_eq!({ descriptor.dwFlags } & FD_WRITESTIME.0 as u32, 0);
    }

    /// Bytes that differ by file and position, so that anything out of place shows.
    fn pattern(index: usize, position: u64) -> u8 {
        ((position / 251 + position % 241 + index as u64 * 7) % 256) as u8
    }

    struct PatternReader {
        index: usize,
        position: u64,
        end: u64,
    }

    impl Read for PatternReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            // Less than was asked for, as a network would.
            let count = buffer.len().min(64 * 1024) as u64;
            let count = count.min(self.end.saturating_sub(self.position)) as usize;
            for byte in &mut buffer[..count] {
                *byte = pattern(self.index, self.position);
                self.position += 1;
            }
            Ok(count)
        }
    }

    struct PatternSource(Vec<u64>);

    impl FileSource for PatternSource {
        fn open(&self, index: usize, offset: u64) -> Box<dyn Read + Send> {
            Box::new(PatternReader {
                index,
                position: offset,
                end: self.0[index],
            })
        }
    }

    const MODIFIED_MS: u64 = 1_600_000_000_000;

    fn entry(path: &str, size: u64, is_dir: bool) -> SharedFile {
        SharedFile {
            path: path.into(),
            size,
            is_dir,
            modified_ms: Some(MODIFIED_MS),
        }
    }

    fn sample_files() -> Vec<SharedFile> {
        vec![
            entry("Dossier été", 0, true),
            entry("Dossier été/sous dossier", 0, true),
            entry("Dossier été/sous dossier/photo ✓.bin", 3_000_000, false),
            entry("Dossier été/vide.txt", 0, false),
            entry("seul.bin", 1_234_567, false),
        ]
    }

    fn source_for(files: &[SharedFile]) -> Arc<dyn FileSource> {
        Arc::new(PatternSource(files.iter().map(|file| file.size).collect()))
    }

    fn fresh_folder(name: &str) -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("hackerman-{name}"));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    /// Pastes as File Explorer does, by dropping onto the folder, with its own code doing
    /// the copying. Then keeps this thread answering messages until the files are all
    /// there, since that code may call back here from another thread.
    fn paste_with_windows(object: &IDataObject, into: &std::path::Path, files: &[SharedFile]) {
        unsafe {
            let folder: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(into.as_os_str()), None).unwrap();
            let target: IDropTarget = folder.BindToHandler(None, &BHID_SFUIObject).unwrap();
            let mut effect: DROPEFFECT = DROPEFFECT_COPY;
            let at = POINTL { x: 0, y: 0 };
            target
                .DragEnter(object, MK_LBUTTON, at, &raw mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY, "the folder doesn't take these");
            target
                .Drop(object, MK_LBUTTON, at, &raw mut effect)
                .unwrap();

            let give_up_at = std::time::Instant::now() + Duration::from_secs(600);
            let mut message = MSG::default();
            while !all_there(files, into) {
                assert!(
                    std::time::Instant::now() < give_up_at,
                    "the paste never finished"
                );
                while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn all_there(files: &[SharedFile], into: &std::path::Path) -> bool {
        files.iter().all(|file| {
            std::fs::metadata(into.join(&file.path)).is_ok_and(|found| {
                found.is_dir() == file.is_dir && (file.is_dir || found.len() == file.size)
            })
        })
    }

    fn assert_pasted(files: &[SharedFile], into: &std::path::Path) {
        for (index, file) in files.iter().enumerate() {
            let path = into.join(&file.path);
            let metadata = std::fs::metadata(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_eq!(metadata.is_dir(), file.is_dir, "{}", file.path);
            if file.is_dir {
                continue;
            }
            assert_eq!(metadata.len(), file.size, "{}", file.path);
            let modified = metadata.modified().unwrap();
            let modified = modified.duration_since(std::time::UNIX_EPOCH).unwrap();
            assert_eq!(modified.as_millis() as u64, MODIFIED_MS, "{}", file.path);

            let mut reader = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
            let mut buffer = vec![0; 1024 * 1024];
            let mut position = 0;
            loop {
                let count = reader.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                for &byte in &buffer[..count] {
                    assert_eq!(
                        byte,
                        pattern(index, position),
                        "{} at {position}",
                        file.path
                    );
                    position += 1;
                }
            }
        }
    }

    fn format(format: windows::core::PCWSTR, tymed: i32, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: registered_format(format),
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed as u32,
        }
    }

    /// What the object hands out, with nothing of Windows' involved but the calls.
    #[test]
    fn the_object_describes_the_files_and_streams_their_content() {
        let files = sample_files();
        let object = data_object(files.clone(), source_for(&files));
        unsafe {
            let descriptor = format(CFSTR_FILEDESCRIPTORW, TYMED_HGLOBAL.0, -1);
            assert_eq!(object.QueryGetData(&raw const descriptor), S_OK);
            let mut medium = object.GetData(&raw const descriptor).unwrap();
            let memory = medium.u.hGlobal;
            let address = GlobalLock(memory).cast::<u8>();
            assert_eq!(address.cast::<u32>().read_unaligned(), files.len() as u32);
            let third = address.add(4 + 2 * size_of::<FILEDESCRIPTORW>());
            let third = third.cast::<FILEDESCRIPTORW>().read_unaligned();
            let low = third.nFileSizeLow;
            assert_eq!(u64::from(low), files[2].size);
            let _ = GlobalUnlock(memory);
            ReleaseStgMedium(&raw mut medium);

            // A folder has no content, and there's nothing past the end.
            for missing in [0, 99, -1] {
                let contents = format(CFSTR_FILECONTENTS, TYMED_ISTREAM.0, missing);
                assert_eq!(
                    object
                        .GetData(&raw const contents)
                        .err()
                        .map(|error| error.code()),
                    Some(DV_E_LINDEX),
                    "{missing}"
                );
            }
            let wrong_medium = format(CFSTR_FILECONTENTS, TYMED_HGLOBAL.0, 2);
            assert_eq!(object.QueryGetData(&raw const wrong_medium), DV_E_TYMED);

            let contents = format(CFSTR_FILECONTENTS, TYMED_ISTREAM.0, 2);
            let mut medium = object.GetData(&raw const contents).unwrap();
            let stream = (*medium.u.pstm).clone().unwrap();

            let mut buffer = vec![0; 500_000];
            let mut count = 0;
            let result = stream.Read(buffer.as_mut_ptr().cast(), 500_000, Some(&raw mut count));
            assert_eq!(
                (result, count),
                (S_OK, 500_000),
                "a full read, in several pieces"
            );
            assert!(
                buffer
                    .iter()
                    .enumerate()
                    .all(|(at, &byte)| byte == pattern(2, at as u64))
            );

            // Going somewhere else carries on correctly from there.
            let mut position = 0;
            stream
                .Seek(-10, STREAM_SEEK_END, Some(&raw mut position))
                .unwrap();
            assert_eq!(position, files[2].size - 10);
            let result = stream.Read(buffer.as_mut_ptr().cast(), 500_000, Some(&raw mut count));
            assert_eq!((result, count), (S_FALSE, 10), "only ten left");
            assert_eq!(buffer[9], pattern(2, files[2].size - 1));

            let mut stat = STATSTG::default();
            stream.Stat(&raw mut stat, STATFLAG_NONAME).unwrap();
            assert_eq!(stat.cbSize, files[2].size);
            ReleaseStgMedium(&raw mut medium);
        }
    }

    #[test]
    fn a_failed_read_is_reported_rather_than_looking_like_the_end() {
        struct Failing;
        impl FileSource for Failing {
            fn open(&self, _index: usize, _offset: u64) -> Box<dyn Read + Send> {
                struct FailingReader;
                impl Read for FailingReader {
                    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                        Err(std::io::Error::other("the network went away"))
                    }
                }
                Box::new(FailingReader)
            }
        }
        let object = data_object(vec![entry("file", 100, false)], Arc::new(Failing));
        unsafe {
            let contents = format(CFSTR_FILECONTENTS, TYMED_ISTREAM.0, 0);
            let mut medium = object.GetData(&raw const contents).unwrap();
            let stream = (*medium.u.pstm).clone().unwrap();
            let mut buffer = [0; 10];
            let result = stream.Read(buffer.as_mut_ptr().cast(), 10, None);
            assert_eq!(result, STG_E_READFAULT);
            ReleaseStgMedium(&raw mut medium);
        }
    }

    /// May show a progress window, so it runs out of sight: see scripts/run-isolated.ps1.
    #[test]
    #[ignore = "may show a window: see scripts/run-isolated.ps1"]
    fn windows_pastes_these_files_into_a_folder() {
        unsafe { OleInitialize(None) }.unwrap();
        let files = sample_files();
        let object = data_object(files.clone(), source_for(&files));
        let into = fresh_folder("paste");
        paste_with_windows(&object, &into, &files);
        assert_pasted(&files, &into);
        let _ = std::fs::remove_dir_all(into);
    }

    /// Past what 32 bits can count. Slow, and needs several gigabytes for a moment.
    #[test]
    #[ignore = "writes 4.5 GB to the temporary folder: see scripts/run-isolated.ps1"]
    fn windows_pastes_a_file_over_4_gb() {
        unsafe { OleInitialize(None) }.unwrap();
        let files = vec![entry("énorme.bin", 4_500_000_000, false)];
        let object = data_object(files.clone(), source_for(&files));
        let into = fresh_folder("paste-big");
        paste_with_windows(&object, &into, &files);
        assert_pasted(&files, &into);
        let _ = std::fs::remove_dir_all(into);
    }

    const PASTE_INTO: &str = "HACKERMAN_TEST_PASTE_INTO";

    /// Stands in for File Explorer, as another program pasting from the clipboard.
    #[test]
    #[ignore = "only for pasted_in_another_program_from_the_clipboard to start"]
    fn another_program_pasting() {
        let into = std::env::var_os(PASTE_INTO).expect("started by the other test");
        unsafe { OleInitialize(None) }.unwrap();
        let object = unsafe { OleGetClipboard() }.unwrap();
        paste_with_windows(&object, std::path::Path::new(&into), &sample_files());
    }

    /// The real path: the real clipboard, and another process, so that every call goes
    /// through Windows to the thread here. Changes the clipboard: see
    /// scripts/run-isolated.ps1.
    #[test]
    #[ignore = "replaces what's on the real clipboard: see scripts/run-isolated.ps1"]
    fn pasted_in_another_program_from_the_clipboard() {
        let reported = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = reported.clone();
        let _watcher = super::super::clipboard::Watcher::start(Box::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }))
        .unwrap();

        let files = sample_files();
        put_on_clipboard(files.clone(), source_for(&files)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            reported.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "that isn't the user copying something"
        );

        let into = fresh_folder("paste-other-program");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "--nocapture"])
            .arg("imp::virtual_files::tests::another_program_pasting")
            .env(PASTE_INTO, &into)
            .status()
            .unwrap();
        assert!(status.success());
        assert_pasted(&files, &into);
        let _ = std::fs::remove_dir_all(into);
    }

    /// Files copied in File Explorer put a list of paths on the clipboard. Reading that
    /// is what was missing, which is why copying files seemed to do nothing.
    #[test]
    #[ignore = "replaces what's on the real clipboard: see scripts/run-isolated.ps1"]
    fn clipboard_copies_files_copied_like_file_explorer() {
        let folder = fresh_folder("copied");
        let file = folder.join("copié.txt");
        std::fs::write(&file, "content").unwrap();
        let mut clipboard = arboard::Clipboard::new().unwrap();
        clipboard.clear().unwrap();
        clipboard
            .set()
            .file_list(std::slice::from_ref(&file))
            .unwrap();

        assert_eq!(super::super::clipboard::files().unwrap(), [file]);
        let data = super::super::clipboard::read().unwrap();
        assert!(data.is_empty(), "files look like nothing else: {data:?}");

        clipboard.set_text("not files").unwrap();
        assert!(super::super::clipboard::files().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }
}
