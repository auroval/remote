//! Files copied on one side, for the other side's clipboard
//! (docs/PRD-file-transfer.md).
//!
//! Each copy is a unidirectional QUIC stream of its own, from the side that
//! copied (the client's after its input stream, the host's after its video
//! stream): one [`FileOffer`] message, then the bytes of every
//! [`FileKind::File`] entry, in the offer's order, back to back. The end of
//! the stream is the end of the copy; a reset stream is a cancelled one,
//! and the receiver throws away what it wrote.
//!
//! The receiver writes the copy into an empty directory of its own with a
//! [`Writer`], which refuses names that would land outside it.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The most one copy may hold, so a broken peer can't fill the disk.
/// Bigger copies are refused before anything is sent.
pub const MAX_FILES_BYTES: u64 = 20_000_000_000;
/// Above this the side that would send asks first (F7): a big copy
/// shouldn't start by surprise.
pub const ASK_ABOVE_BYTES: u64 = 500_000_000;
/// The most entries (files and folders) one copy may hold.
pub const MAX_FILE_ENTRIES: usize = 10_000;
/// The longest a name in a path may be, in bytes (as on APFS and ext4).
pub const MAX_NAME_BYTES: usize = 255;

/// What a copy holds. Folders come before what's in them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct FileOffer {
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The names from the copy's top down: `["photos", "a.jpg"]`. A copy of
    /// several things has several top-level names.
    pub path: Vec<String>,
    pub kind: FileKind,
    /// Last modified, in seconds since 1970, when known.
    pub modified_s: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File { size: u64 },
}

/// One step of a copy, as it passes between daemon and agent: on its way
/// in to the agent that writes it, or out from the agent that reads it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Begin(FileOffer),
    Data(Vec<u8>),
    End,
    /// The sender cancelled or the stream broke: throw it away.
    Abort(String),
}

impl FileOffer {
    /// Bytes of all files together.
    pub fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| match e.kind {
                FileKind::File { size } => size,
                FileKind::Dir => 0,
            })
            .fold(0u64, u64::saturating_add)
    }

    /// Files (not folders) in the copy.
    pub fn files(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, FileKind::File { .. }))
            .count()
    }

    /// Whether the offer is one a receiver may write: within the limits,
    /// every name a plain one, no path twice, every folder before its
    /// contents.
    pub fn check(&self) -> Result<(), String> {
        if self.entries.is_empty() {
            return Err("nothing in it".into());
        }
        if self.entries.len() > MAX_FILE_ENTRIES {
            return Err(format!(
                "{} entries, more than {MAX_FILE_ENTRIES}",
                self.entries.len()
            ));
        }
        let total = self.total_bytes();
        if total > MAX_FILES_BYTES {
            return Err(format!(
                "{}, more than {}",
                megabytes(total),
                megabytes(MAX_FILES_BYTES)
            ));
        }
        let mut seen: HashSet<&[String]> = HashSet::new();
        let mut dirs: HashSet<&[String]> = HashSet::new();
        for e in &self.entries {
            if e.path.is_empty() {
                return Err("an entry without a name".into());
            }
            if let Some(bad) = e.path.iter().find(|n| !plain_name(n)) {
                return Err(format!("not a plain name: {bad:?}"));
            }
            let parent = &e.path[..e.path.len() - 1];
            if !parent.is_empty() && !dirs.contains(parent) {
                return Err(format!("{:?} comes before its folder", e.path.join("/")));
            }
            if !seen.insert(&e.path) {
                return Err(format!("{:?} twice", e.path.join("/")));
            }
            if e.kind == FileKind::Dir {
                dirs.insert(&e.path);
            }
        }
        Ok(())
    }
}

/// How the user reads a size: `12.3 MB`, `1.2 GB`.
pub fn megabytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}

/// What a copy is, in a few words: `photos and 2 more (31 file(s), 12.0 MB)`.
pub fn summary(offer: &FileOffer) -> String {
    format!(
        "{} ({} file(s), {})",
        names(offer),
        offer.files(),
        megabytes(offer.total_bytes())
    )
}

/// The copy's top-level names, short: `a.txt`, `a.txt and 2 more`.
pub fn names(offer: &FileOffer) -> String {
    let roots: Vec<&str> = offer
        .entries
        .iter()
        .filter(|e| e.path.len() == 1)
        .map(|e| e.path[0].as_str())
        .collect();
    match roots.as_slice() {
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
        [] => String::new(),
    }
}

/// The sizes of the offer's files, in its order.
pub fn sizes(offer: &FileOffer) -> impl Iterator<Item = u64> + '_ {
    offer.entries.iter().filter_map(|e| match e.kind {
        FileKind::File { size } => Some(size),
        FileKind::Dir => None,
    })
}

/// A name that stays where it's put: not empty, not `.` or `..`, no path
/// separator (either kind) or NUL, not too long.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_NAME_BYTES
        && !name.contains(['/', '\\', '\0'])
}

/// Writes a copy into `dir` as its bytes come in.
pub struct Writer {
    dir: PathBuf,
    /// Files still to write, in order, with their size and time.
    pending: std::vec::IntoIter<(PathBuf, u64, Option<i64>)>,
    /// The file being written and the bytes it still needs.
    current: Option<(File, u64, Option<i64>)>,
    /// Folders, to give their times once their contents are in.
    dirs: Vec<(PathBuf, Option<i64>)>,
    /// The copy's top-level paths, in order.
    roots: Vec<PathBuf>,
}

impl Writer {
    /// Checks `offer` and creates its folders in `dir`, which must exist.
    /// Files are created as their turn comes, never over an existing one.
    pub fn new(dir: &Path, offer: &FileOffer) -> io::Result<Self> {
        offer
            .check()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let mut roots = Vec::new();
        for e in &offer.entries {
            let path: PathBuf = e.path.iter().fold(dir.to_path_buf(), |p, n| p.join(n));
            if e.path.len() == 1 {
                roots.push(path.clone());
            }
            match e.kind {
                FileKind::Dir => {
                    fs::create_dir(&path)?;
                    dirs.push((path, e.modified_s));
                }
                FileKind::File { size } => files.push((path, size, e.modified_s)),
            }
        }
        let mut w = Self {
            dir: dir.to_path_buf(),
            pending: files.into_iter(),
            current: None,
            dirs,
            roots,
        };
        w.advance()?;
        Ok(w)
    }

    /// Opens the next file that still needs bytes; empty ones on the way are
    /// created and done.
    fn advance(&mut self) -> io::Result<()> {
        while self.current.as_ref().is_none_or(|(_, left, _)| *left == 0) {
            if let Some((file, _, modified)) = self.current.take() {
                done(file, modified)?;
            }
            let Some((path, size, modified)) = self.pending.next() else {
                return Ok(());
            };
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            self.current = Some((file, size, modified));
        }
        Ok(())
    }

    /// The next bytes of the copy.
    pub fn write(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let Some((file, left, _)) = self.current.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "more bytes than the files hold",
                ));
            };
            let n = (*left).min(data.len() as u64) as usize;
            file.write_all(&data[..n])?;
            *left -= n as u64;
            data = &data[n..];
            self.advance()?;
        }
        Ok(())
    }

    /// The copy is complete: its top-level paths, to put on the clipboard.
    pub fn finish(mut self) -> io::Result<Vec<PathBuf>> {
        self.advance()?;
        if self.current.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the copy ended before its last file",
            ));
        }
        // Inner folders first: setting a folder's time after its contents
        // keeps it.
        for (path, modified) in self.dirs.iter().rev() {
            if let Some(t) = modified.and_then(time) {
                let _ = File::open(path).and_then(|d| d.set_modified(t));
            }
        }
        Ok(std::mem::take(&mut self.roots))
    }

    /// Where the copy is being written.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn done(file: File, modified: Option<i64>) -> io::Result<()> {
    if let Some(t) = modified.and_then(time) {
        file.set_modified(t)?;
    }
    Ok(())
}

fn time(seconds: i64) -> Option<SystemTime> {
    let s = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(s))
}

/// Seconds since 1970 of `t`, for [`FileEntry::modified_s`].
pub fn seconds(t: SystemTime) -> Option<i64> {
    i64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

/// Reads what was copied: every path with everything under it, as an offer
/// and the files to send, in its order. Symbolic links and special files
/// are left out and named in the second list, so the user hears why.
pub fn scan(paths: &[PathBuf]) -> io::Result<(FileOffer, Vec<PathBuf>, Vec<PathBuf>)> {
    let mut offer = FileOffer::default();
    let mut sources = Vec::new();
    let mut skipped = Vec::new();
    let mut names = HashSet::new();
    for p in paths {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            skipped.push(p.clone());
            continue;
        };
        if !names.insert(name.to_owned()) {
            // Two copied things with the same name (from different
            // folders): only the first, a folder can't hold both.
            skipped.push(p.clone());
            continue;
        }
        walk(
            p,
            vec![name.to_owned()],
            &mut offer,
            &mut sources,
            &mut skipped,
        )?;
        if offer.entries.len() > MAX_FILE_ENTRIES {
            break;
        }
    }
    Ok((offer, sources, skipped))
}

fn walk(
    path: &Path,
    name: Vec<String>,
    offer: &mut FileOffer,
    sources: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if offer.entries.len() > MAX_FILE_ENTRIES {
        return Ok(());
    }
    let meta = fs::symlink_metadata(path)?;
    let modified_s = meta.modified().ok().and_then(seconds);
    if meta.is_file() {
        offer.entries.push(FileEntry {
            path: name,
            kind: FileKind::File { size: meta.len() },
            modified_s,
        });
        sources.push(path.to_path_buf());
    } else if meta.is_dir() {
        offer.entries.push(FileEntry {
            path: name.clone(),
            kind: FileKind::Dir,
            modified_s,
        });
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|c| c.file_name());
        for c in children {
            let Some(n) = c.file_name().to_str().map(str::to_owned) else {
                skipped.push(c.path());
                continue;
            };
            let mut child = name.clone();
            child.push(n);
            walk(&c.path(), child, offer, sources, skipped)?;
        }
    } else {
        skipped.push(path.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &[&str], kind: FileKind) -> FileEntry {
        FileEntry {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            modified_s: Some(1_700_000_000),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("auroval-files-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sizes_read_as_megabytes_or_gigabytes() {
        assert_eq!(megabytes(12_340_000), "12.3 MB");
        assert_eq!(megabytes(1_240_000_000), "1.2 GB");
    }

    #[test]
    fn names_that_escape_are_refused() {
        for bad in ["", ".", "..", "a/b", "a\\b", "nul\0", &"x".repeat(256)] {
            let offer = FileOffer {
                entries: vec![entry(&[bad], FileKind::File { size: 0 })],
            };
            assert!(offer.check().is_err(), "{bad:?} passed");
        }
        let ok = FileOffer {
            entries: vec![
                entry(&["map"], FileKind::Dir),
                entry(&["map", ".hidden"], FileKind::File { size: 1 }),
                entry(&["map", "..ok"], FileKind::File { size: 1 }),
            ],
        };
        ok.check().unwrap();
    }

    #[test]
    fn offers_out_of_order_twice_or_too_big_are_refused() {
        let orphan = FileOffer {
            entries: vec![entry(&["d", "f"], FileKind::File { size: 1 })],
        };
        assert!(orphan.check().is_err());
        let twice = FileOffer {
            entries: vec![
                entry(&["f"], FileKind::File { size: 1 }),
                entry(&["f"], FileKind::File { size: 1 }),
            ],
        };
        assert!(twice.check().is_err());
        let big = FileOffer {
            entries: vec![
                entry(
                    &["a"],
                    FileKind::File {
                        size: MAX_FILES_BYTES,
                    },
                ),
                entry(&["b"], FileKind::File { size: 1 }),
            ],
        };
        assert!(big.check().is_err());
        assert!(FileOffer::default().check().is_err());
    }

    #[test]
    fn a_copy_is_written_across_chunk_boundaries() {
        let dir = tempdir("write");
        let offer = FileOffer {
            entries: vec![
                entry(&["one.txt"], FileKind::File { size: 5 }),
                entry(&["map"], FileKind::Dir),
                entry(&["map", "empty"], FileKind::File { size: 0 }),
                entry(&["map", "two.txt"], FileKind::File { size: 6 }),
            ],
        };
        let mut w = Writer::new(&dir, &offer).unwrap();
        for chunk in [&b"hel"[..], b"lo wo", b"rld"] {
            w.write(chunk).unwrap();
        }
        let roots = w.finish().unwrap();
        assert_eq!(roots, vec![dir.join("one.txt"), dir.join("map")]);
        assert_eq!(fs::read(dir.join("one.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dir.join("map/two.txt")).unwrap(), b" world");
        assert_eq!(fs::read(dir.join("map/empty")).unwrap(), b"");
        let t = fs::metadata(dir.join("one.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(seconds(t), Some(1_700_000_000));
        let t = fs::metadata(dir.join("map")).unwrap().modified().unwrap();
        assert_eq!(seconds(t), Some(1_700_000_000));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn too_many_or_too_few_bytes_fail() {
        let dir = tempdir("count");
        let offer = FileOffer {
            entries: vec![entry(&["f"], FileKind::File { size: 2 })],
        };
        let mut w = Writer::new(&dir, &offer).unwrap();
        assert!(w.write(b"abc").is_err());
        fs::remove_dir_all(&dir).unwrap();

        let dir = tempdir("short");
        let mut w = Writer::new(&dir, &offer).unwrap();
        w.write(b"a").unwrap();
        assert!(w.finish().is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let dir = tempdir("exists");
        fs::write(dir.join("f"), b"keep").unwrap();
        let offer = FileOffer {
            entries: vec![entry(&["f"], FileKind::File { size: 1 })],
        };
        assert!(Writer::new(&dir, &offer).is_err());
        assert_eq!(fs::read(dir.join("f")).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_reads_folders_in_order_and_skips_links() {
        let dir = tempdir("scan");
        fs::create_dir(dir.join("map")).unwrap();
        fs::write(dir.join("map/b"), b"bb").unwrap();
        fs::write(dir.join("map/a"), b"a").unwrap();
        std::os::unix::fs::symlink("a", dir.join("map/link")).unwrap();
        fs::write(dir.join("top"), b"top").unwrap();
        let (offer, sources, skipped) = scan(&[dir.join("map"), dir.join("top")]).unwrap();
        offer.check().unwrap();
        let paths: Vec<String> = offer.entries.iter().map(|e| e.path.join("/")).collect();
        assert_eq!(paths, ["map", "map/a", "map/b", "top"]);
        assert_eq!(
            sources,
            vec![dir.join("map/a"), dir.join("map/b"), dir.join("top")]
        );
        assert_eq!(skipped, vec![dir.join("map/link")]);
        assert_eq!(offer.total_bytes(), 6);
        assert_eq!(offer.files(), 3);
        assert_eq!(summary(&offer), "map and 1 more (3 file(s), 0.0 MB)");
        assert_eq!(sizes(&offer).collect::<Vec<_>>(), [1, 2, 3]);

        // And what was scanned writes back the same.
        let out = tempdir("scan-out");
        let mut w = Writer::new(&out, &offer).unwrap();
        for s in &sources {
            w.write(&fs::read(s).unwrap()).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(fs::read(out.join("map/b")).unwrap(), b"bb");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }
}
