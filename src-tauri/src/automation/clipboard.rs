//! Windows clipboard support for rich Gmail compose pastes.
//!
//! Gmail only accepts rich compose content through a real paste, so the sender
//! briefly replaces the OS clipboard with the email's HTML plus its plain-text
//! fallback. The swap must stay invisible to the operator, who is often working
//! on the same machine while a campaign sends:
//!
//! * A per-desktop named mutex serializes the whole lease (install → restore)
//!   across every NovaMail process the launcher starts. Without it, a second
//!   instance can snapshot an in-flight email payload and "restore" that
//!   payload after its own paste, leaving the operator's clipboard permanently
//!   holding an email body instead of what they copied.
//! * While the payload is on the clipboard it is tagged with the
//!   `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard` /
//!   `ExcludeClipboardContentFromMonitorProcessing` control formats — the same
//!   markers Chromium and password managers write for sensitive data — so a
//!   running campaign cannot flood Win+V history, the cloud clipboard, or
//!   clipboard monitors with one email entry per recipient.
//! * Each lease lives on its own dedicated OS thread because Windows named
//!   mutex ownership is thread-affine: the async runtime may resume the send
//!   future on a different worker, so the release must not happen on whatever
//!   thread happens to drop the lease. Dropping the lease only mails a command
//!   to that thread; the restore there is sequence-checked and retried while
//!   Windows still reports the clipboard busy, and it never blocks the async
//!   runtime.

use std::ffi::c_void;
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Mutex, MutexGuard};
use windows_sys::Win32::Foundation::{
    CloseHandle, GlobalFree, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalFlags, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// `GlobalFlags` returns this bit for an invalid / non-global handle. Declared
/// here rather than imported because the constant's availability varies across
/// the `windows-sys` feature set we enable.
const GMEM_INVALID_HANDLE: u32 = 0x8000;

const CF_UNICODETEXT: u32 = 13;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;

/// One `OpenClipboard` attempt budget. The restore loop extends the total
/// window by retrying while the other app still holds the clipboard open.
const OPEN_ATTEMPT_WINDOW: Duration = Duration::from_millis(750);

/// How long one lease may keep the clipboard reserved before the caller gives
/// up instead of quietly waiting forever behind a sibling instance's send.
const CROSS_INSTANCE_WAIT: Duration = Duration::from_secs(30);

/// How long the lease thread keeps retrying the restore after the paste. While
/// this window is open the lease still owns the cross-process mutex, so no
/// sibling instance can interleave an install or restore of its own.
const RESTORE_PATIENCE: Duration = Duration::from_secs(15);

/// Pause between restore attempts when Windows keeps reporting the clipboard
/// busy (RDP's rdpclip, clipboard managers and similar hold it briefly).
const RESTORE_RETRY_SLEEP: Duration = Duration::from_millis(50);

/// Per-login-session named mutex around the rich-paste lease. Clipboard
/// ownership itself is per-session, and the launcher's per-app children live in
/// the operator's session, so a `Local\` name matches exactly the processes
/// that could be fighting over the clipboard.
const LEASE_MUTEX_NAME: &str = "Local\\NovaMailRichComposeClipboardLease";

static CLIPBOARD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

type SavedFormat = (u32, Vec<u8>);

/// RAII ownership of the cross-process lease mutex; dropping releases it.
struct CrossProcessGuard {
    handle: HANDLE,
}

impl Drop for CrossProcessGuard {
    fn drop(&mut self) {
        // Windows mutex ownership is thread-affine, so the release only works
        // here because the lease keeps acquire and drop on the same thread.
        unsafe { ReleaseMutex(self.handle) };
        unsafe { CloseHandle(self.handle) };
    }
}

/// Take the cross-process lease mutex, waiting up to `timeout` for another
/// NovaMail process to finish its own paste. `WAIT_ABANDONED` still grants
/// ownership (its holder died before it could restore), so the send queue
/// keeps flowing after a crashed instance.
fn acquire_cross_process_guard(timeout: Duration) -> Result<CrossProcessGuard, String> {
    let name: Vec<u16> = LEASE_MUTEX_NAME.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(
            "Windows could not guard the rich email paste across NovaMail instances.".to_string(),
        );
    }
    let deadline = Instant::now() + timeout;
    loop {
        let wait = unsafe { WaitForSingleObject(handle, 250) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            return Ok(CrossProcessGuard { handle });
        }
        if wait == WAIT_FAILED {
            unsafe { CloseHandle(handle) };
            return Err("The clipboard lease guard is not usable on this Windows session.".to_string());
        }
        if Instant::now() >= deadline {
            unsafe { CloseHandle(handle) };
            return Err(
                "Another NovaMail instance is sending on this PC and held the paste clipboard too long. This recipient needs a retry.".to_string(),
            );
        }
    }
}

/// Lock and replace the OS clipboard with HTML plus its plain-text fallback.
///
/// The returned lease restores the saved clipboard when dropped: dropping only
/// asks the lease thread, which keeps retrying while Windows still reports the
/// clipboard busy. If the operator copied anything while the email was being
/// pasted, the restore steps aside — their freshest copy stays untouched.
pub async fn install_html(html: &str, text: &str) -> Result<ClipboardLease, String> {
    let guard = CLIPBOARD_LOCK.get_or_init(|| Mutex::new(())).lock().await;
    let (command_tx, command_rx) = channel::<LeaseCommand>();
    let (announce_tx, announce_rx) = oneshot::channel::<Result<u32, String>>();
    // Evaluate the owned payload before spawning: the thread must not borrow
    // from the caller's `&str` arguments (thread-bound borrows would escape).
    let html = html.to_string();
    let text = text.to_string();
    let worker = std::thread::Builder::new()
        .name("novamail-clipboard-lease".to_string())
        .spawn(move || lease_thread(html, text, announce_tx, command_rx));
    if worker.is_err() {
        return Err("Could not start the clipboard lease thread.".to_string());
    }

    // The OS thread reports the installed payload sequence (or an already
    // cleaned-up failure) before the sender ever presses Ctrl+V.
    let sequence = match announce_rx.await {
        Ok(Ok(sequence)) => sequence,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(
                "The clipboard lease thread stopped before the email paste was armed.".to_string(),
            )
        }
    };

    Ok(ClipboardLease {
        sequence,
        command: command_tx,
        _guard: guard,
    })
}

pub struct ClipboardLease {
    /// The clipboard sequence number the payload was written under. Any change
    /// before the restore means the operator copied something of theirs while
    /// the send owned the clipboard, and their copy wins.
    sequence: u32,
    command: Sender<LeaseCommand>,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for ClipboardLease {
    fn drop(&mut self) {
        // Decide with a single atomic read and mail the decision to the lease
        // thread: releasing the cross-process mutex and restoring the
        // operator's formats must happen on the thread that acquired them, and
        // dropping must never block the async runtime on a busy clipboard.
        let command = if unsafe { GetClipboardSequenceNumber() } == self.sequence {
            LeaseCommand::Restore
        } else {
            LeaseCommand::Release
        };
        if self.command.send(command).is_err() {
            log::warn!("The clipboard lease thread stopped before the restore was requested.");
        }
    }
}

/// The dedicated OS thread that owns one lease end to end.
///
/// A dedicated thread is required because Windows named-mutex ownership is
/// thread-affine and the async runtime may resume the send future on a
/// different worker between here and the lease drop: re-acquiring there could
/// deadlock against a stuck owner, and releasing there would fail outright.
/// The thread parks while the sender drives Gmail, so it costs no CPU and the
/// announce channel returns the installed sequence (or the failure, with the
/// clipboard already restored on the way out).
fn lease_thread(
    html: String,
    text: String,
    announce: oneshot::Sender<Result<u32, String>>,
    commands: Receiver<LeaseCommand>,
) {
    let _cross_process = match acquire_cross_process_guard(CROSS_INSTANCE_WAIT) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = announce.send(Err(error));
            return;
        }
    };

    let (sequence, saved) = match install_payload(&html, &text) {
        Ok((sequence, saved)) => (sequence, saved),
        Err(error) => {
            let _ = announce.send(Err(error));
            return;
        }
    };

    if announce.send(Ok(sequence)).is_err() {
        // The sender gave up between arming the paste and reading the lease:
        // hand the operator's clipboard back instead of leaving the payload.
        restore_with_patience(sequence, &saved);
        return;
    }

    // Park while the sender drives Gmail: focus, Ctrl+V, DOM confirmation. A
    // dead channel (the lease was never created after all) falls back to
    // restoring, which the sequence check keeps safe in every case.
    match commands.recv() {
        Ok(LeaseCommand::Restore) => {
            if unsafe { GetClipboardSequenceNumber() } == sequence {
                restore_with_patience(sequence, &saved);
            }
        }
        Ok(LeaseCommand::Release) => {}
        Err(_) => {
            if unsafe { GetClipboardSequenceNumber() } == sequence {
                restore_with_patience(sequence, &saved);
            }
        }
    }
    // The command is handled and `_cross_process` drops here, releasing the
    // mutex on this thread — the very one that acquired it.
}

/// Open, snapshot and replace the clipboard with the email payload. Returns
/// the sequence number the payload was written under plus the operator's
/// previous clipboard snapshot. The clipboard is closed again before returning.
fn install_payload(html: &str, text: &str) -> Result<(u32, Vec<SavedFormat>), String> {
    let clipboard = open_clipboard(OPEN_ATTEMPT_WINDOW)?;
    let saved = snapshot_clipboard()?;

    let html_name: Vec<u16> = "HTML Format".encode_utf16().chain(Some(0)).collect();
    let html_format = unsafe { RegisterClipboardFormatW(html_name.as_ptr()) };
    if html_format == 0 {
        return Err("Windows could not register the HTML clipboard format.".to_string());
    }

    let mut unicode_text: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    unicode_text.extend_from_slice(&[0, 0]);
    let mut replacement = vec![
        (html_format, html_clipboard_data(html)),
        (CF_UNICODETEXT, unicode_text),
    ];
    // Mark the payload as transient: Win+V history, the cloud clipboard and
    // clipboard monitors skip entries carrying these formats, so a campaign
    // cannot bury the operator's own history under one email per recipient.
    replacement.extend(temporary_payload_flags());

    if unsafe { EmptyClipboard() } == 0 {
        return Err("Windows could not prepare the clipboard for a rich email paste.".to_string());
    }
    if let Err(error) = write_formats(&replacement) {
        let restore_error = restore_open_clipboard(&saved).err();
        return Err(match restore_error {
            Some(restore_error) => format!(
                "Could not prepare the rich email clipboard ({error}); restoring the previous clipboard also failed ({restore_error})."
            ),
            None => format!("Could not prepare the rich email clipboard: {error}"),
        });
    }

    let sequence = unsafe { GetClipboardSequenceNumber() };
    // Keep the clipboard open only while reading and writing its contents.
    drop(clipboard);
    Ok((sequence, saved))
}

/// Register the operator-privacy control formats that Chromium and password
/// managers write for sensitive clipboard data. A missing format id (atom
/// limits, unusual Windows builds) just leaves that marker out instead of
/// failing the paste.
fn temporary_payload_flags() -> Vec<(u32, Vec<u8>)> {
    let mut flags = Vec::with_capacity(3);
    for name in [
        "ExcludeClipboardContentFromMonitorProcessing",
        "CanIncludeInClipboardHistory",
        "CanUploadToCloudClipboard",
    ] {
        let utf16: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let id = unsafe { RegisterClipboardFormatW(utf16.as_ptr()) };
        if id != 0 {
            // The history/cloud formats read a zero DWORD to refuse the entry;
            // the monitor-processing marker only needs to be present.
            flags.push((id, vec![0, 0, 0, 0]));
        }
    }
    flags
}

/// Put the operator's saved formats back, sequence-checked, retrying while
/// Windows still reports the clipboard busy. The re-check inside the open
/// clipboard closes the race against an external copy that slips past the
/// pre-call check, so their freshest data is never overwritten.
fn restore_with_patience(sequence: u32, saved: &[SavedFormat]) {
    let deadline = Instant::now() + RESTORE_PATIENCE;
    loop {
        if let Ok(clipboard) = open_clipboard(OPEN_ATTEMPT_WINDOW) {
            // Recheck under the OS clipboard lock so an external copy that
            // raced with OpenClipboard is never overwritten.
            if unsafe { GetClipboardSequenceNumber() } == sequence {
                if let Err(error) = restore_open_clipboard(saved) {
                    log::warn!(
                        "Could not restore the previous clipboard after the Gmail paste: {error}"
                    );
                }
            }
            drop(clipboard);
            break;
        }
        if Instant::now() >= deadline {
            log::warn!(
                "The clipboard stayed busy after the Gmail paste; the previous contents return on the operator's next copy."
            );
            break;
        }
        std::thread::sleep(RESTORE_RETRY_SLEEP);
    }
}

/// Tells the lease thread what to do once the sender stops holding the lease.
enum LeaseCommand {
    /// No outside copy raced the paste: put the operator's saved formats back.
    Restore,
    /// Something wrote to the clipboard while the send owned the lease (the
    /// operator copied while an email was mid-paste): the freshest contents are
    /// theirs, so release the lease without touching them.
    Release,
}

struct ClipboardOpen;

impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

fn open_clipboard(timeout: Duration) -> Result<ClipboardOpen, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            return Ok(ClipboardOpen);
        }
        if Instant::now() >= deadline {
            return Err(
                "The Windows clipboard is busy. Close the app using it and try again.".to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn snapshot_clipboard() -> Result<Vec<SavedFormat>, String> {
    let mut formats = Vec::new();
    let mut total_bytes = 0usize;
    let mut format = 0u32;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            // Some clipboard owners expose a format without backing storage (or
            // mark it as available without binding data until later). Electron
            // simply skips such formats instead of failing the whole paste, so
            // do the same here instead of aborting the clipboard snapshot.
            continue;
        }
        // The clipboard handle is not always an HGLOBAL. Bitmaps, metafiles and
        // some GDI-based formats return a non-memory handle whose "size" is
        // either zero or, worse, a misleading number. Locking such a handle can
        // trap into unmapped memory and kill the process (which is what made the
        // Gmail window vanish mid-send), so skip anything that is not a global
        // memory object instead of reading it.
        let flags = unsafe { GlobalFlags(handle) };
        if (flags & GMEM_INVALID_HANDLE) != 0 {
            continue;
        }
        let size = unsafe { GlobalSize(handle) };
        if size == 0 {
            // Format present but backed by something other than global memory
            // (or simply empty): leave it out of the snapshot rather than erroring.
            continue;
        }
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > MAX_SNAPSHOT_BYTES {
            return Err(
                "The clipboard is too large to preserve for a rich email paste.".to_string(),
            );
        }
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            // Another process may be using the handle, or it is not global
            // memory after all. Skip instead of risking an invalid read.
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec() };
        unsafe { GlobalUnlock(handle) };
        formats.push((format, bytes));
    }
    Ok(formats)
}

fn write_formats(formats: &[SavedFormat]) -> Result<(), String> {
    let mut allocated: Vec<(u32, *mut c_void)> = Vec::with_capacity(formats.len());
    for (format, bytes) in formats {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
        if handle.is_null() {
            free_allocations(&allocated);
            return Err("Windows could not allocate clipboard memory.".to_string());
        }
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            unsafe { GlobalFree(handle) };
            free_allocations(&allocated);
            return Err("Windows could not fill clipboard memory.".to_string());
        }
        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            GlobalUnlock(handle);
        }
        allocated.push((*format, handle));
    }

    for index in 0..allocated.len() {
        let (format, handle) = allocated[index];
        if unsafe { SetClipboardData(format, handle as HANDLE) }.is_null() {
            // SetClipboardData owns handles it accepted. Free only the current
            // and not-yet-submitted allocations; the caller restores the saved
            // clipboard after clearing any partially written replacement.
            free_allocations(&allocated[index..]);
            return Err(format!("Windows could not set clipboard format {format}."));
        }
    }
    Ok(())
}

fn free_allocations(allocations: &[(u32, *mut c_void)]) {
    for (_, handle) in allocations {
        unsafe { GlobalFree(*handle) };
    }
}

fn restore_open_clipboard(saved: &[SavedFormat]) -> Result<(), String> {
    if unsafe { EmptyClipboard() } == 0 {
        return Err("Windows could not clear the temporary clipboard contents.".to_string());
    }
    write_formats(saved)
}

fn html_clipboard_data(fragment: &str) -> Vec<u8> {
    let before_fragment = "<!DOCTYPE html><html><body><!--StartFragment-->";
    let after_fragment = "<!--EndFragment--></body></html>";
    let document = format!("{before_fragment}{fragment}{after_fragment}");
    let header_template = concat!(
        "Version:0.9\r\n",
        "StartHTML:0000000000\r\n",
        "EndHTML:0000000000\r\n",
        "StartFragment:0000000000\r\n",
        "EndFragment:0000000000\r\n",
        "SourceURL:https://mail.google.com/\r\n"
    );
    let start_html = header_template.len();
    let start_fragment = start_html + before_fragment.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = start_html + document.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\nSourceURL:https://mail.google.com/\r\n"
    );
    let mut result = header.into_bytes();
    result.extend_from_slice(document.as_bytes());
    result.push(0);
    result
}
