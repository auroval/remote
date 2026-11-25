//! The Mac's clipboard (the general pasteboard) during a session: text
//! copied on the Mac goes to the client, text from the client is put on it.
//!
//! Runs where the pasteboard belongs: in the user's session (the agent, or
//! `serve`), never at the login window. Only changes made during the
//! session are sent; what was on the clipboard before stays on the Mac.
//! The content is never logged, only its size.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use auroval_proto::{ClipboardEcho, MAX_CLIPBOARD};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString, NSURL};

/// How often the pasteboard is checked for a new copy. macOS has no change
/// notification; reading `changeCount` is cheap.
const POLL: Duration = Duration::from_millis(250);

/// A running watcher. Dropping it stops the thread.
pub struct Watcher {
    set: Sender<Put>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching; `copied` gets every text copied on the Mac from now
    /// on, up to [`MAX_CLIPBOARD`] bytes.
    pub fn start(copied: impl Fn(String) + Send + 'static) -> Self {
        let (set, requests) = channel();
        let thread = std::thread::Builder::new()
            .name("pasteboard".into())
            .spawn(move || watch(requests, copied))
            .ok();
        Self { set, thread }
    }

    /// Puts text from the client on the Mac's clipboard.
    pub fn set(&self, text: String) {
        let _ = self.set.send(Put::Text(text));
    }

    /// Puts files the client copied on the Mac's clipboard, by their paths
    /// (in our temp folder, see [`crate::files`]), from any thread: Finder
    /// pastes copies.
    pub fn files_putter(&self) -> impl Fn(Vec<PathBuf>) + Send + 'static {
        let set = self.set.clone();
        move |paths| {
            let _ = set.send(Put::Files(paths));
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Other senders (a putter) may live on: say it.
        let _ = self.set.send(Put::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the client sent for the Mac's clipboard.
enum Put {
    Text(String),
    Files(Vec<PathBuf>),
    Stop,
}

fn watch(requests: Receiver<Put>, copied: impl Fn(String)) {
    let board = NSPasteboard::generalPasteboard();
    let mut echo = ClipboardEcho::default();
    let mut seen = board.changeCount();
    // The clipboard's change count when it got our files, while it still
    // holds them: they're removed with the session, so they go off it too.
    let mut ours: Option<isize> = None;
    loop {
        match requests.recv_timeout(POLL) {
            Ok(Put::Files(paths)) => {
                autoreleasepool(|_| {
                    let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
                        .iter()
                        .filter_map(|p| p.to_str())
                        .map(|p| {
                            ProtocolObject::from_retained(NSURL::fileURLWithPath(
                                &NSString::from_str(p),
                            ))
                        })
                        .collect();
                    board.clearContents();
                    if board.writeObjects(&NSArray::from_retained_slice(&urls)) {
                        println!("  clipboard ← client ({} file(s) or folder(s))", urls.len());
                    } else {
                        eprintln!("  clipboard: macOS refused files from the client");
                    }
                });
                // Our own write: don't read it back as a copy.
                seen = board.changeCount();
                ours = Some(seen);
                continue;
            }
            Ok(Put::Text(text)) => {
                echo.noted(&text);
                autoreleasepool(|_| {
                    board.clearContents();
                    let ok = board.setString_forType(&NSString::from_str(&text), unsafe {
                        NSPasteboardTypeString
                    });
                    if ok {
                        println!("  clipboard ← client ({} bytes)", text.len());
                    } else {
                        eprintln!("  clipboard: macOS refused text from the client");
                    }
                });
                // Our own write: don't read it back as a copy.
                seen = board.changeCount();
                continue;
            }
            Ok(Put::Stop) | Err(RecvTimeoutError::Disconnected) => {
                if ours.is_some() && ours == Some(board.changeCount()) {
                    board.clearContents();
                }
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = board.changeCount();
        if now == seen {
            continue;
        }
        seen = now;
        let text = autoreleasepool(|_| {
            board
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|s| s.to_string())
        });
        // Not text (an image, a file): nothing to send.
        let Some(text) = text else { continue };
        if text.len() > MAX_CLIPBOARD {
            println!(
                "  clipboard: skipped a copy of {} bytes (over {} bytes)",
                text.len(),
                MAX_CLIPBOARD
            );
            continue;
        }
        if echo.is_new(&text) {
            println!("  clipboard → client ({} bytes)", text.len());
            copied(text);
        }
    }
}
