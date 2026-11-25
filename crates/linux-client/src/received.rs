//! Files copied on the Mac, for our clipboard (docs/PRD-file-transfer.md,
//! phase P2).
//!
//! The Mac sends each copy right away, on a stream of its own; it lands in
//! a folder of ours and goes on the clipboard as `text/uri-list` (Dolphin,
//! most others) and `x-special/gnome-copied-files` (Nautilus). Pasting
//! there makes the file manager copy it out.
//!
//! The folder is under `$XDG_CACHE_HOME`, because the file managers run
//! outside our sandbox and must read it by the path we give. In the
//! Flatpak that variable is `~/.var/app/<id>/cache`, and `~/.var/app/<id>`
//! is mounted at that same path inside the sandbox, so the path means the
//! same file on both sides with no extra permission (F3). The runtime
//! directory wouldn't do: the sandbox's `/run/user/<uid>` is another
//! folder on the host.
//!
//! Each run of the client has its own folder, held by a lock on a file in
//! it; it goes when the window closes, and a folder nobody holds (a crash)
//! goes at the next start (F8). Not by process ID: in the Flatpak every
//! instance has its own PID namespace, where IDs repeat.

use std::ffi::{CStr, c_char, c_void};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use auroval_proto::files::{FileOffer, Writer, names, summary};
use auroval_proto::quic;

use crate::copies::notify;
use crate::overlay::Tone;
use crate::transfer::{self, Way};

/// Completed copies kept on disk: the one on the clipboard and the one
/// before, which a slow paste may still be reading.
const KEEP: usize = 2;

/// Biggest piece of a copy read at once.
const CHUNK: usize = 256 * 1024;

/// This run's folder, while the window is open.
struct Folder {
    dir: PathBuf,
    /// Held while we run: tells a later start this folder is in use.
    _lock: File,
    next: u64,
    kept: Vec<PathBuf>,
}

static FOLDER: Mutex<Option<Folder>> = Mutex::new(None);
/// The latest complete copy, for the window to put on the clipboard.
static READY: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
/// When the Mac last said why a copy failed: a reset stream that follows
/// shouldn't say less.
static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Where received copies go: `$XDG_CACHE_HOME/auroval-files`, or
/// `~/.cache/auroval-files`.
fn base_dir() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("auroval-files"))
}

/// Our folder for copies, while it lives (the window's lifetime).
pub struct Session;

impl Session {
    /// Removes what crashed runs left, and makes this run's folder.
    pub fn start() -> Self {
        match base_dir().map(|b| open(&b)) {
            Some(Ok(folder)) => *FOLDER.lock().unwrap() = Some(folder),
            Some(Err(e)) => warn!("files: no folder for copies from the Mac: {e:#}"),
            None => warn!("files: no folder for copies from the Mac: no $HOME"),
        }
        Self
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(f) = FOLDER.lock().unwrap().take() {
            let _ = fs::remove_dir_all(&f.dir);
        }
    }
}

fn open(base: &Path) -> anyhow::Result<Folder> {
    fs::create_dir_all(base).with_context(|| format!("creating {}", base.display()))?;
    clean_leftovers(base);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = base.join(format!("run-{}-{nanos}", std::process::id()));
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock = File::create(dir.join(".lock"))?;
    lock.try_lock()
        .map_err(|e| anyhow!("locking {}: {e}", dir.display()))?;
    Ok(Folder {
        dir,
        _lock: lock,
        next: 0,
        kept: Vec::new(),
    })
}

/// Removes the folders of runs that are gone: nobody holds their lock.
fn clean_leftovers(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let held = File::open(e.path().join(".lock")).is_ok_and(|f| f.try_lock().is_err());
        if !held {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

/// A fresh, empty folder for the next copy.
fn next_dir() -> anyhow::Result<PathBuf> {
    let mut folder = FOLDER.lock().unwrap();
    let f = folder
        .as_mut()
        .ok_or_else(|| anyhow!("no folder for copies (see the log)"))?;
    f.next += 1;
    let dir = f.dir.join(f.next.to_string());
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The copy in `dir` is complete: keep it, and the one before.
fn kept(dir: PathBuf) {
    if let Some(f) = FOLDER.lock().unwrap().as_mut() {
        f.kept.push(dir);
        while f.kept.len() > KEEP {
            let _ = fs::remove_dir_all(f.kept.remove(0));
        }
    }
}

/// The latest copy from the Mac that is complete, once: for the clipboard.
pub fn take_ready() -> Option<Vec<PathBuf>> {
    READY.lock().unwrap().take()
}

/// The Mac said why its copy can't come.
pub fn failed(why: String) {
    *FAILED_AT.lock().unwrap() = Some(Instant::now());
    say!("files: the Mac didn't send its copy: {why}");
    notify(
        Tone::Amber,
        "Not copied from the Mac",
        why,
        Duration::from_secs(10),
    );
}

/// Removes a copy that didn't complete, however it ends.
struct Partial(Option<PathBuf>);

impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// Receives one copy from the Mac on `stream`, and readies it for the
/// clipboard. Says how it went, in the log and the overlay (F9).
pub async fn receive(stream: quinn::RecvStream) {
    match try_receive(stream).await {
        Ok(Some(what)) => {
            say!("files: {what} from the Mac on the clipboard");
            notify(
                Tone::Blue,
                "Copied from the Mac",
                format!("{what}: paste with Ctrl+V"),
                Duration::from_secs(5),
            );
        }
        Ok(None) => {}
        Err(e) => {
            let recent = FAILED_AT
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < Duration::from_secs(3));
            say!("files: not copied from the Mac: {e:#}");
            if !recent {
                notify(
                    Tone::Amber,
                    "Not copied from the Mac",
                    format!("{e:#}"),
                    Duration::from_secs(8),
                );
            }
        }
    }
}

async fn try_receive(mut stream: quinn::RecvStream) -> anyhow::Result<Option<String>> {
    let offer: FileOffer = match quic::recv(&mut stream).await {
        Ok(Some(o)) => o,
        Ok(None) => return Ok(None),
        Err(e) => bail!("unreadable copy: {e}"),
    };
    if let Err(why) = offer.check() {
        let _ = stream.stop(0u32.into());
        bail!("refused it: {why}");
    }
    let what = summary(&offer);
    say!("files ← Mac: {what}");
    let dir = next_dir()?;
    let partial = Partial(Some(dir.clone()));
    let mut writer = tokio::task::block_in_place(|| Writer::new(&dir, &offer))
        .with_context(|| format!("writing into {}", dir.display()))?;
    let total = offer.total_bytes();
    let t = transfer::begin(Way::FromMac, &names(&offer), total);
    let mut got = 0u64;
    loop {
        let chunk = tokio::select! {
            c = stream.read_chunk(CHUNK, true) => c,
            () = t.cancelled() => {
                let _ = stream.stop(0u32.into());
                bail!("cancelled");
            }
        };
        match chunk {
            Ok(Some(chunk)) => {
                got += chunk.bytes.len() as u64;
                if got > total {
                    let _ = stream.stop(0u32.into());
                    bail!("the Mac sent more than it offered");
                }
                tokio::task::block_in_place(|| writer.write(&chunk.bytes))
                    .context("writing the copy")?;
                t.advance(chunk.bytes.len() as u64);
            }
            Ok(None) => break,
            Err(quinn::ReadError::Reset(_)) => bail!("the Mac stopped sending it"),
            Err(e) => bail!("{e}"),
        }
    }
    let roots = tokio::task::block_in_place(|| writer.finish()).context("the copy")?;
    let mut partial = partial;
    partial.0 = None;
    kept(dir);
    *READY.lock().unwrap() = Some(roots);
    Ok(Some(what))
}

/// `file://` URIs for `paths`, as a file manager's copy lists them.
pub fn uris(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| format!("file://{}", percent_encode(p.as_os_str().as_bytes())))
        .collect()
}

/// `text/uri-list` (RFC 2483): one URI per line, CRLF.
pub fn uri_list(uris: &[String]) -> String {
    uris.iter().map(|u| format!("{u}\r\n")).collect()
}

/// Nautilus's own format: `copy`, then one URI per line.
pub fn gnome_copied(uris: &[String]) -> String {
    let mut s = String::from("copy");
    for u in uris {
        s.push('\n');
        s.push_str(u);
    }
    s
}

/// Escapes all but the characters RFC 3986 leaves plain in a path.
fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What we offer on the clipboard, by MIME type. Lives until SDL's cleanup
/// callback: the data it hands out must stay valid until then.
struct Offer {
    uri_list: Vec<u8>,
    gnome: Vec<u8>,
}

/// Puts `paths` on our clipboard as files, offered by SDL when another app
/// asks. Call on SDL's thread (the window's). The text `uri-list` that
/// goes with it is returned, for [`crate::copies::CopyEcho`]: our own copy
/// mustn't go back to the Mac.
pub fn put_on_clipboard(paths: &[PathBuf]) -> anyhow::Result<String> {
    use sdl3::sys::clipboard::SDL_SetClipboardData;
    let uris = uris(paths);
    let list = uri_list(&uris);
    let offer = Box::new(Offer {
        uri_list: list.clone().into_bytes(),
        gnome: gnome_copied(&uris).into_bytes(),
    });
    let mimes = [
        c"text/uri-list".as_ptr(),
        c"x-special/gnome-copied-files".as_ptr(),
    ];
    let userdata = Box::into_raw(offer).cast::<c_void>();
    // SAFETY: SDL copies the MIME list; `userdata` is ours until `cleanup`.
    let ok = unsafe {
        SDL_SetClipboardData(
            Some(data),
            Some(cleanup),
            userdata,
            mimes.as_ptr(),
            mimes.len(),
        )
    };
    if !ok {
        // SAFETY: SDL didn't take it, so `cleanup` won't run.
        drop(unsafe { Box::from_raw(userdata.cast::<Offer>()) });
        bail!("SDL: {}", sdl3::get_error());
    }
    Ok(list)
}

unsafe extern "C" fn data(
    userdata: *mut c_void,
    mime: *const c_char,
    size: *mut usize,
) -> *const c_void {
    // SAFETY: `userdata` is the Offer from put_on_clipboard, alive until
    // cleanup; `mime` is a C string from SDL.
    let (offer, mime) = unsafe { (&*userdata.cast::<Offer>(), CStr::from_ptr(mime)) };
    let bytes: &[u8] = match mime.to_bytes() {
        b"text/uri-list" => &offer.uri_list,
        b"x-special/gnome-copied-files" => &offer.gnome,
        _ => &[],
    };
    // SAFETY: SDL passes a valid pointer for the size.
    unsafe { *size = bytes.len() };
    bytes.as_ptr().cast()
}

unsafe extern "C" fn cleanup(userdata: *mut c_void) {
    // SAFETY: the Offer from put_on_clipboard; SDL calls this once.
    drop(unsafe { Box::from_raw(userdata.cast::<Offer>()) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_become_file_uris_that_read_back() {
        let paths = vec![
            PathBuf::from("/home/u/.cache/auroval-files/run-1/1/a b#?.txt"),
            PathBuf::from("/tmp/ünï"),
        ];
        let uris = uris(&paths);
        assert_eq!(
            uris[0],
            "file:///home/u/.cache/auroval-files/run-1/1/a%20b%23%3F.txt"
        );
        assert_eq!(crate::copies::uri_paths(&uri_list(&uris)), paths);
        assert_eq!(
            gnome_copied(&uris),
            format!("copy\n{}\nfile:///tmp/%C3%BCn%C3%AF", uris[0])
        );
    }

    #[test]
    fn folders_of_runs_that_are_gone_are_removed() {
        let base = std::env::temp_dir().join(format!("auroval-received-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("run-old/1")).unwrap();
        fs::write(base.join("run-old/.lock"), b"").unwrap();
        fs::create_dir_all(base.join("stray")).unwrap();
        let mine = open(&base).unwrap();
        assert!(!base.join("run-old").exists() && !base.join("stray").exists());
        // Held by us: a second start leaves it.
        let other = open(&base).unwrap();
        assert!(mine.dir.exists() && other.dir.exists());
        drop(mine);
        let _third = open(&base).unwrap();
        assert!(other.dir.exists());
        fs::remove_dir_all(&base).unwrap();
    }
}
