//! Files copied here (in Dolphin, Nautilus, …), for the Mac's clipboard
//! (docs/PRD-file-transfer.md, phase P1).
//!
//! A file manager's copy offers `text/uri-list`. Outside a sandbox those
//! paths can be read as they are. In the Flatpak they can't: there the
//! copy also offers a key for the FileTransfer portal, which hands us
//! paths under `/run/user/<uid>/doc/` that we may read (spike S, c).
//!
//! The copy goes to the Mac right away, on a stream of its own: macOS has
//! no way to fetch it at paste time (spike S, b).
//!
//! Files dropped on the window go the same way, with the point where they
//! were dropped (F10, [`crate::drops`]). SDL hands us their paths; in the
//! Flatpak it gets them from the same portal, for files (not folders).

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use auroval_proto::files::{FileDest, FileOffer, MAX_FILES_BYTES, scan};
use auroval_proto::quic;
use tokio::io::AsyncReadExt;

use crate::overlay::{Badge, Tone};

pub const URI_LIST: &str = "text/uri-list";
pub const PORTAL_KEY: &str = "application/vnd.portal.filetransfer";

/// What the clipboard holds after a file manager copied files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCopy {
    pub uris: Option<String>,
    /// The FileTransfer portal's key for the copy; used in the Flatpak.
    pub portal_key: Option<String>,
}

/// Files for the Mac.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    /// Copied here, for the Mac's clipboard.
    Copy(FileCopy),
    /// Dropped on the window, at `at` (`0.0..=1.0` over the picture).
    Drop { paths: Vec<PathBuf>, at: (f32, f32) },
}

impl Outgoing {
    fn is_drop(&self) -> bool {
        matches!(self, Outgoing::Drop { .. })
    }
}

/// The files on our clipboard, if it holds any. Call on SDL's thread.
pub fn on_clipboard() -> Option<FileCopy> {
    let uris = clipboard_data(URI_LIST);
    let portal_key = clipboard_data(PORTAL_KEY)
        .map(|k| {
            k.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_owned()
        })
        .filter(|k| !k.is_empty());
    let files = uris.as_deref().is_some_and(|u| !uri_paths(u).is_empty());
    (files || portal_key.is_some()).then_some(FileCopy { uris, portal_key })
}

fn clipboard_data(mime: &str) -> Option<String> {
    use sdl3::sys::clipboard::{SDL_GetClipboardData, SDL_HasClipboardData};
    use sdl3::sys::stdinc::SDL_free;
    let mime = CString::new(mime).ok()?;
    if !unsafe { SDL_HasClipboardData(mime.as_ptr()) } {
        return None;
    }
    let mut size = 0usize;
    let data = unsafe { SDL_GetClipboardData(mime.as_ptr(), &mut size) };
    if data.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec();
    unsafe { SDL_free(data) };
    String::from_utf8(bytes).ok()
}

/// Running as the Flatpak: host paths aren't ours to read.
pub fn in_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some()
}

/// The local paths in a `text/uri-list` (RFC 2483): `file:` URIs only,
/// percent-decoded; comments and other schemes are left out.
pub fn uri_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            // `file:///path` or `file://localhost/path`.
            let path = if rest.starts_with('/') {
                rest
            } else {
                rest.strip_prefix("localhost")?
            };
            let bytes = percent_decode(path)?;
            Some(PathBuf::from(OsString::from_vec(bytes)))
        })
        .collect()
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

/// The paths we may read for `copy`.
async fn resolve(copy: &FileCopy) -> anyhow::Result<Vec<PathBuf>> {
    if in_flatpak() {
        let key = copy.portal_key.as_deref().ok_or_else(|| {
            anyhow!("the file manager didn't hand the copy to the file transfer portal")
        })?;
        return retrieve(key).await;
    }
    Ok(copy.uris.as_deref().map(uri_paths).unwrap_or_default())
}

/// Whether we may read every dropped path. In the Flatpak SDL gets dropped
/// files from the portal, but not folders: for those it falls back to paths
/// on the host, which the sandbox can't see.
fn readable(paths: &[PathBuf]) -> anyhow::Result<()> {
    if let Some(p) = paths.iter().find(|p| std::fs::symlink_metadata(p).is_err()) {
        if in_flatpak() {
            bail!(
                "the Flatpak can't read {} (dropped folders don't come through the file \
                 portal): drop the files in it, or copy and paste the folder",
                p.display()
            );
        }
        bail!("can't read {}", p.display());
    }
    Ok(())
}

/// Asks the FileTransfer portal for the files behind `key`.
async fn retrieve(key: &str) -> anyhow::Result<Vec<PathBuf>> {
    let conn = zbus::Connection::session()
        .await
        .context("no session bus")?;
    let options = std::collections::HashMap::<&str, zbus::zvariant::Value>::new();
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            Some("org.freedesktop.portal.FileTransfer"),
            "RetrieveFiles",
            &(key, options),
        )
        .await
        .context("the file transfer portal")?;
    let (paths,): (Vec<String>,) = reply.body().deserialize()?;
    Ok(paths.into_iter().map(PathBuf::from).collect())
}

/// What the overlay says about the latest copy, and until when.
static NOTICE: Mutex<Option<(Badge, Instant)>> = Mutex::new(None);

fn notify(tone: Tone, body: &str, detail: String, shown: Duration) {
    let badge = Badge {
        tag: "FILES".into(),
        tone,
        body: body.into(),
        detail,
        hints: Vec::new(),
    };
    *NOTICE.lock().unwrap() = Some((badge, Instant::now() + shown));
}

/// The overlay's badge about files, while there is one to show.
pub fn notice() -> Option<Badge> {
    let mut n = NOTICE.lock().unwrap();
    match n.as_ref() {
        Some((badge, until)) if Instant::now() < *until => Some(badge.clone()),
        _ => {
            *n = None;
            None
        }
    }
}

/// The copy on its way, since when, and whether it's a drop: for the
/// Mac's answer.
static SENDING: Mutex<Option<(String, Instant, bool)>> = Mutex::new(None);
/// Counts the Mac's refusals; a copy being sent stops at the next one.
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// The Mac's answer to our latest copy ([`HostControl::FilesTaken`],
/// [`HostControl::FilesRefused`]).
///
/// [`HostControl::FilesTaken`]: auroval_proto::HostControl::FilesTaken
/// [`HostControl::FilesRefused`]: auroval_proto::HostControl::FilesRefused
pub fn answered(answer: Result<(), String>) {
    let sending = SENDING.lock().unwrap().take();
    let what = sending
        .as_ref()
        .map(|(w, _, _)| w.clone())
        .unwrap_or_default();
    let dropped = sending.as_ref().is_some_and(|(_, _, d)| *d);
    match answer {
        Ok(()) if dropped => {
            say!("files: {what} dropped on the Mac");
            notify(
                Tone::Blue,
                "Dropped on the Mac",
                what,
                Duration::from_secs(5),
            );
        }
        Err(why) if dropped => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            say!("files: {what} not dropped on the Mac: {why}");
            not_sent(true, why);
        }
        Ok(()) => {
            let secs = sending
                .map(|(_, t, _)| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            say!("files: {what} on the Mac's clipboard after {secs:.1} s");
            notify(
                Tone::Blue,
                "On the Mac's clipboard",
                format!("{what}: paste with ⌘V"),
                Duration::from_secs(5),
            );
        }
        Err(why) => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            say!("files: the Mac didn't take {what}: {why}");
            notify(
                Tone::Amber,
                "Not copied to the Mac",
                why,
                Duration::from_secs(8),
            );
        }
    }
}

/// Biggest read from a file at once.
const CHUNK: usize = 256 * 1024;

/// Sends `files` to the Mac on a stream of its own. Says how it went, in
/// the log and the overlay; never a silent nothing (F9).
pub async fn send(conn: quinn::Connection, files: Outgoing) {
    if let Err(e) = try_send(&conn, &files).await {
        say!("files: not sent to the Mac: {e:#}");
        not_sent(files.is_drop(), format!("{e:#}"));
    }
}

/// Says in the overlay why files didn't go to the Mac.
fn not_sent(drop: bool, why: String) {
    let body = if drop {
        "Not dropped on the Mac"
    } else {
        "Not copied to the Mac"
    };
    notify(Tone::Amber, body, why, Duration::from_secs(8));
}

/// A drop that can't go to the Mac, and why.
pub fn not_dropped(why: &str) {
    say!("files: drop not sent to the Mac: {why}");
    not_sent(true, why.into());
}

/// Resets its stream when dropped unfinished (a newer copy, an error), so
/// the Mac throws the copy away instead of taking it for complete.
struct Resetting(Option<quinn::SendStream>);

impl Drop for Resetting {
    fn drop(&mut self) {
        if let Some(mut s) = self.0.take() {
            let _ = s.reset(0u32.into());
        }
    }
}

async fn try_send(conn: &quinn::Connection, files: &Outgoing) -> anyhow::Result<()> {
    let (paths, to) = match files {
        Outgoing::Copy(copy) => (resolve(copy).await?, FileDest::Clipboard),
        Outgoing::Drop { paths, at: (x, y) } => {
            readable(paths)?;
            (paths.clone(), FileDest::Drop { x: *x, y: *y })
        }
    };
    if paths.is_empty() {
        bail!("no files in the copy");
    }
    let (mut offer, sources, skipped) = tokio::task::spawn_blocking(move || scan(&paths))
        .await?
        .context("reading the copied files")?;
    offer.to = to;
    for s in &skipped {
        say!("files: left out {} (not a file or folder)", s.display());
    }
    let total = offer.total_bytes();
    if total > MAX_FILES_BYTES {
        bail!(
            "{:.0} MB is more than the {} MB a copy may be",
            total as f64 / 1e6,
            MAX_FILES_BYTES / 1_000_000
        );
    }
    offer.check().map_err(|e| anyhow!(e))?;
    let what = summary(&offer);
    let drop = files.is_drop();
    say!(
        "files → Mac{}: {what}",
        if drop { " (dropped)" } else { "" }
    );
    let body = if drop {
        "Dropping on the Mac"
    } else {
        "Copying to the Mac"
    };
    notify(Tone::Blue, body, what.clone(), Duration::from_secs(3600));
    *SENDING.lock().unwrap() = Some((what.clone(), Instant::now(), drop));
    let refusals = REFUSALS.load(Ordering::Relaxed);
    let mut guard = Resetting(Some(conn.open_uni().await?));
    let stream = guard.0.as_mut().expect("just set");
    // Below video, input and control: a big copy mustn't hold them up.
    let _ = stream.set_priority(-1);
    quic::send(stream, &offer)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut buf = vec![0u8; CHUNK];
    for (source, size) in sources.iter().zip(sizes(&offer)) {
        let mut file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("opening {}", source.display()))?;
        let mut left = size;
        while left > 0 {
            if REFUSALS.load(Ordering::Relaxed) != refusals {
                // The Mac said no (and the overlay says why): stop here.
                return Ok(());
            }
            let want = (left as usize).min(CHUNK);
            let n = file
                .read(&mut buf[..want])
                .await
                .with_context(|| format!("reading {}", source.display()))?;
            if n == 0 {
                bail!("{} got shorter while it was sent", source.display());
            }
            stream.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
    }
    stream.finish()?;
    // Wait until the Mac has it all, so the time says something.
    let _ = stream.stopped().await;
    // Complete: nothing to reset. The Mac says when it's on its
    // clipboard ([`answered`]).
    guard.0 = None;
    Ok(())
}

/// The sizes of the offer's files, in its order.
fn sizes(offer: &FileOffer) -> impl Iterator<Item = u64> + '_ {
    offer.entries.iter().filter_map(|e| match e.kind {
        auroval_proto::files::FileKind::File { size } => Some(size),
        auroval_proto::files::FileKind::Dir => None,
    })
}

fn summary(offer: &FileOffer) -> String {
    let roots: Vec<&str> = offer
        .entries
        .iter()
        .filter(|e| e.path.len() == 1)
        .map(|e| e.path[0].as_str())
        .collect();
    let names = match roots.as_slice() {
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
        [] => String::new(),
    };
    format!(
        "{names} ({} file(s), {:.1} MB)",
        offer.files(),
        offer.total_bytes() as f64 / 1e6
    )
}

/// Keeps the copy that was on the clipboard before the session, and
/// repeats of one copy, from being sent (F5).
#[derive(Debug, Default)]
pub struct CopyEcho {
    last: Option<FileCopy>,
}

impl CopyEcho {
    /// What was on the clipboard already when the session started.
    pub fn noted(&mut self, copy: Option<FileCopy>) {
        self.last = copy;
    }

    /// The clipboard changed and holds `copy`: true when it's new. The
    /// same files are the same copy, whatever portal key comes with them: a
    /// clipboard manager (Klipper) offers a copy again after the file
    /// manager, with or without the key.
    pub fn is_new(&mut self, copy: &FileCopy) -> bool {
        let same = match (&self.last, &copy.uris) {
            (Some(last), Some(uris)) => last.uris.as_ref() == Some(uris),
            (Some(last), None) => last == copy,
            (None, _) => false,
        };
        if same {
            return false;
        }
        self.last = Some(copy.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_give_local_paths() {
        let list = "# from Dolphin\r\nfile:///home/u/Documents/a%20b.txt\r\n\
                    file://localhost/tmp/x\r\nhttps://example.com/y\r\nfile://other/z\r\n\
                    file:///bad%zz\r\n";
        assert_eq!(
            uri_paths(list),
            vec![
                PathBuf::from("/home/u/Documents/a b.txt"),
                PathBuf::from("/tmp/x")
            ]
        );
        // Not UTF-8 after decoding is still a path.
        let raw = uri_paths("file:///tmp/%FF");
        assert_eq!(raw.len(), 1);
    }

    #[test]
    fn a_copy_is_sent_once() {
        let copy = FileCopy {
            uris: Some("file:///a".into()),
            portal_key: Some("k".into()),
        };
        let mut echo = CopyEcho::default();
        echo.noted(Some(copy.clone()));
        assert!(!echo.is_new(&copy));
        let other = FileCopy {
            uris: Some("file:///b".into()),
            portal_key: None,
        };
        assert!(echo.is_new(&other));
        assert!(!echo.is_new(&other));
        assert!(echo.is_new(&copy));
        // Offered again without the key (or with another): the same copy.
        let again = FileCopy {
            uris: copy.uris.clone(),
            portal_key: None,
        };
        assert!(!echo.is_new(&again));
    }
}
