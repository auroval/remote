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

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use auroval_proto::files::{FileOffer, MAX_FILES_BYTES, scan};
use auroval_proto::quic;
use tokio::io::AsyncReadExt;

pub const URI_LIST: &str = "text/uri-list";
pub const PORTAL_KEY: &str = "application/vnd.portal.filetransfer";

/// What the clipboard holds after a file manager copied files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCopy {
    pub uris: Option<String>,
    /// The FileTransfer portal's key for the copy; used in the Flatpak.
    pub portal_key: Option<String>,
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

/// Biggest read from a file at once.
const CHUNK: usize = 256 * 1024;

/// Sends `copy` to the Mac on a stream of its own. Says how it went;
/// errors end up in the log, never as a silent nothing (F9).
pub async fn send(conn: quinn::Connection, copy: FileCopy) {
    if let Err(e) = try_send(&conn, &copy).await {
        say!("files: not sent to the Mac: {e:#}");
    }
}

async fn try_send(conn: &quinn::Connection, copy: &FileCopy) -> anyhow::Result<()> {
    let paths = resolve(copy).await?;
    if paths.is_empty() {
        bail!("no files in the copy");
    }
    let (offer, sources, skipped) = tokio::task::spawn_blocking(move || scan(&paths))
        .await?
        .context("reading the copied files")?;
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
    say!("files → Mac: {}", summary(&offer));
    let started = Instant::now();
    let mut stream = conn.open_uni().await?;
    // Below video, input and control: a big copy mustn't hold them up.
    let _ = stream.set_priority(-1);
    quic::send(&mut stream, &offer)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut buf = vec![0u8; CHUNK];
    for (source, size) in sources.iter().zip(sizes(&offer)) {
        let mut file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("opening {}", source.display()))?;
        let mut left = size;
        while left > 0 {
            let want = (left as usize).min(CHUNK);
            let n = file
                .read(&mut buf[..want])
                .await
                .with_context(|| format!("reading {}", source.display()))?;
            if n == 0 {
                let _ = stream.reset(0u32.into());
                bail!("{} got shorter while it was sent", source.display());
            }
            stream.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
    }
    stream.finish()?;
    // Wait until the Mac has it all, so the time says something.
    let _ = stream.stopped().await;
    say!(
        "files: {:.1} MB on the Mac's clipboard in {:.1} s",
        total as f64 / 1e6,
        started.elapsed().as_secs_f32()
    );
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

    /// The clipboard changed and holds `copy`: true when it's new.
    pub fn is_new(&mut self, copy: &FileCopy) -> bool {
        if self.last.as_ref() == Some(copy) {
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
    }
}
