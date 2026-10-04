//! Writes a file or a folder as a tar stream.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use computer_protocol::{SKIP_REPORT_ENTRY, SkipList};
use tar::{Builder, EntryType, Header};

use crate::{Failure, Progress, rules::Platform};

/// What was sent.
#[derive(Debug)]
pub struct Packed {
    pub progress: Progress,
    pub skipped: SkipList,
}

/// Reads exactly `left` bytes of a file and fails if the file ends sooner.
///
/// A tar header states the size up front, so a file that shrinks must stop the transfer
/// instead of leaving a corrupt archive.
struct Exact<'a, R> {
    inner: R,
    left: u64,
    copied: &'a mut u64,
}

impl<R: Read> Read for Exact<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = usize::try_from(self.left).map_or(buf.len(), |left| left.min(buf.len()));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file got shorter while it was being copied",
            ));
        }
        self.left -= n as u64;
        *self.copied += n as u64;
        Ok(n)
    }
}

struct Packer<W: Write> {
    builder: Builder<W>,
    platform: Platform,
    progress: Progress,
    skipped: SkipList,
}

/// Sends `source` to `out` as a tar archive.
///
/// The archive's first entry is the root, named after the last part of `source`. A path that is
/// itself a link is followed. Links inside a folder are sent as links, and pipes, sockets, and
/// devices are skipped and listed in the closing report entry. `platform` is the system being
/// read from.
///
/// # Errors
///
/// Fails when the source cannot be read at all, when a file changes size while it is sent, or
/// when `out` fails.
pub fn pack<W: Write>(source: &Path, out: W, platform: Platform) -> Result<Packed, Failure> {
    let mut packer = Packer {
        builder: Builder::new(out),
        platform,
        progress: Progress::default(),
        skipped: SkipList::default(),
    };
    match packer.run(source) {
        Ok(()) => Ok(Packed {
            progress: packer.progress,
            skipped: packer.skipped,
        }),
        Err(error) => Err(Failure::new(
            format!("sending {} failed: {error}", source.display()),
            packer.progress,
        )),
    }
}

impl<W: Write> Packer<W> {
    fn run(&mut self, source: &Path) -> io::Result<()> {
        let meta = fs::metadata(source).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(error.kind(), "it does not exist")
            } else {
                error
            }
        })?;
        let name = root_name(source)?;
        let name = Path::new(&name);
        if meta.is_dir() {
            self.add_dir(source, name, &meta)?;
        } else if meta.is_file() {
            self.add_file(source, name)?;
        } else {
            return Err(io::Error::other(
                "it is a pipe, device, or socket, not a file or folder",
            ));
        }
        self.finish()
    }

    fn finish(&mut self) -> io::Result<()> {
        let json = serde_json::to_vec(&self.skipped).map_err(io::Error::other)?;
        let mut header = header(EntryType::Regular, 0o644, 0);
        header.set_size(json.len() as u64);
        self.builder
            .append_data(&mut header, SKIP_REPORT_ENTRY, json.as_slice())?;
        self.builder.finish()?;
        self.builder.get_mut().flush()
    }

    fn add_dir(&mut self, dir: &Path, rel: &Path, meta: &fs::Metadata) -> io::Result<()> {
        let mut head = header(EntryType::Directory, 0o755, mtime(meta));
        self.builder.append_data(&mut head, rel, io::empty())?;
        self.progress.folders += 1;

        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(error) => {
                self.skip(rel, format!("the folder cannot be read: {error}"));
                return Ok(());
            }
        };
        let mut children = Vec::new();
        for child in listing {
            match child {
                Ok(child) => children.push(child),
                Err(error) => self.skip(rel, format!("an entry cannot be read: {error}")),
            }
        }
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                self.skip(
                    &rel.join(child.file_name()),
                    "the name is not valid UTF-8".to_owned(),
                );
                continue;
            };
            let path = child.path();
            let rel = rel.join(&name);
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    self.skip(&rel, format!("it cannot be read: {error}"));
                    continue;
                }
            };
            let kind = meta.file_type();
            if kind.is_dir() {
                self.add_dir(&path, &rel, &meta)?;
            } else if kind.is_file() {
                self.add_file(&path, &rel)?;
            } else if kind.is_symlink() {
                self.add_link(&path, &rel, &meta)?;
            } else {
                self.skip(&rel, "it is a pipe, device, or socket".to_owned());
            }
        }
        Ok(())
    }

    fn add_file(&mut self, path: &Path, rel: &Path) -> io::Result<()> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if rel.components().count() > 1 => {
                self.skip(rel, format!("it cannot be opened: {error}"));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        let mut head = header(EntryType::Regular, file_mode(&meta), mtime(&meta));
        head.set_size(meta.len());
        let mut copied = 0;
        let data = Exact {
            inner: file,
            left: meta.len(),
            copied: &mut copied,
        };
        let sent = self.builder.append_data(&mut head, rel, data);
        self.progress.bytes += copied;
        sent.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", rel.display())))?;
        self.progress.files += 1;
        Ok(())
    }

    fn add_link(&mut self, path: &Path, rel: &Path, meta: &fs::Metadata) -> io::Result<()> {
        let target = match fs::read_link(path) {
            Ok(target) => target,
            Err(error) => {
                self.skip(rel, format!("the link cannot be read: {error}"));
                return Ok(());
            }
        };
        if self.platform == Platform::Windows && (target.has_root() || has_prefix(&target)) {
            self.skip(
                rel,
                "the link points to an absolute Windows path that does not exist on the computer"
                    .to_owned(),
            );
            return Ok(());
        }
        let mut head = header(EntryType::Symlink, 0o777, mtime(meta));
        self.builder.append_link(&mut head, rel, &target)?;
        self.progress.files += 1;
        Ok(())
    }

    fn skip(&mut self, rel: &Path, reason: String) {
        self.skipped.push(rel.display().to_string(), reason);
    }
}

fn has_prefix(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(std::path::Component::Prefix(_))
    )
}

/// The name the root is sent under: the last part of the path as given, else of its real path.
///
/// # Errors
///
/// Fails when the path has no name or the name is not valid UTF-8.
pub fn root_name(source: &Path) -> io::Result<String> {
    let given: Option<PathBuf> = source.file_name().map(PathBuf::from);
    let name = match given {
        Some(name) => name,
        None => fs::canonicalize(source)?
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("it has no name, give a path to a file or folder"))?,
    };
    name.into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("its name is not valid UTF-8"))
}

fn header(kind: EntryType, mode: u32, mtime: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_mtime(mtime);
    header.set_size(0);
    header
}

fn mtime(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn file_mode(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}
