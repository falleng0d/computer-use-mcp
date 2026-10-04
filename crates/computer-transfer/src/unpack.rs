//! Writes a tar stream made by `pack` to disk.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    io::{self, BufWriter, Read},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use computer_protocol::{SKIP_REPORT_ENTRY, SkipList};
use tar::{Archive, EntryType};
use uuid::Uuid;

use crate::{
    Failure, Progress,
    rules::{self, Action, Kind, PathProblem, Platform, Wanted},
};

const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const WRITE_BUFFER: usize = 128 * 1024;

/// What was written and where.
#[derive(Debug)]
pub struct Unpacked {
    /// Final path of the file or folder that was created or merged into.
    pub path: PathBuf,
    pub progress: Progress,
    /// Entries left out here, followed by the ones the sender left out.
    pub skipped: SkipList,
}

/// Removes its file when dropped before [`Temp::place`].
struct Temp(Option<PathBuf>);

impl Temp {
    fn new(dir: &Path) -> Self {
        Self(Some(dir.join(format!(
            ".computer-use-transfer-{}.tmp",
            Uuid::new_v4().simple()
        ))))
    }

    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("a temporary file has a path until it is placed")
    }

    fn place(mut self, to: &Path) -> io::Result<()> {
        fs::rename(self.path(), to)?;
        self.0 = None;
        Ok(())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Counts the bytes that pass through, so progress is right when a copy stops halfway.
struct Counted<'a, R> {
    inner: R,
    bytes: &'a mut u64,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        *self.bytes += n as u64;
        Ok(n)
    }
}

fn kind_at(path: &Path) -> Option<Kind> {
    let meta = fs::symlink_metadata(path).ok()?;
    let kind = meta.file_type();
    Some(if kind.is_symlink() {
        Kind::Link
    } else if kind.is_dir() {
        Kind::Folder
    } else if kind.is_file() {
        Kind::File
    } else {
        Kind::Other
    })
}

fn followed_kind(path: &Path) -> Option<Kind> {
    let meta = fs::metadata(path).ok()?;
    Some(if meta.is_dir() {
        Kind::Folder
    } else if meta.is_file() {
        Kind::File
    } else {
        Kind::Other
    })
}

/// Tells before any data is sent whether a root named `name` can land at `dest`.
///
/// Applies the same rules as [`unpack`] does with the first entry, and returns the final path.
///
/// # Errors
///
/// Refuses with a message naming the path that is in the way, or the name that cannot exist here.
pub fn check_destination(
    dest: &Path,
    name: &str,
    folder: bool,
    overwrite: bool,
    platform: Platform,
) -> Result<PathBuf, String> {
    if let Some(problem) = rules::name_problem(name, platform) {
        return Err(format!("{name} cannot be created here: {problem}"));
    }
    let wanted = if folder { Wanted::Folder } else { Wanted::File };
    rules::destination(dest, followed_kind(dest), name, wanted, overwrite, kind_at)
        .map(|found| found.path)
}

/// Writes the archive read from `reader` to `dest`, following the destination rule of `cp -r`.
///
/// The root entry decides everything before anything is written: the final path, the refusal
/// when it exists and `overwrite` is off, and the refusal of a file against a folder. The same
/// checks run again for every entry of a folder, so a merge replaces same-named files and keeps
/// the others. Files are written under a temporary name and renamed into place, and an
/// unfinished temporary file is removed on every failure.
///
/// Entries with an absolute path or `..` stop the transfer. An entry whose path goes through a
/// symbolic link or a file is refused, so links made by the archive cannot lead later entries
/// outside the destination. Names `platform` cannot hold, and links it cannot create, are
/// skipped and listed.
///
/// # Errors
///
/// Fails with the reason and how far the transfer got.
pub fn unpack<R: Read>(
    reader: R,
    dest: &Path,
    overwrite: bool,
    platform: Platform,
) -> Result<Unpacked, Failure> {
    let mut unpacker = Unpacker {
        platform,
        overwrite,
        root: PathBuf::new(),
        progress: Progress::default(),
        skipped: SkipList::default(),
        sender_skipped: SkipList::default(),
        skipped_dirs: Vec::new(),
        checked_parent: None,
        saw_end: false,
        written: HashMap::new(),
    };
    match unpacker.run(reader, dest) {
        Ok(()) => {
            let Unpacker {
                root,
                progress,
                mut skipped,
                sender_skipped,
                ..
            } = unpacker;
            skipped.merge(sender_skipped);
            Ok(Unpacked {
                path: root,
                progress,
                skipped,
            })
        }
        Err(message) => Err(Failure::new(message, unpacker.progress)),
    }
}

struct Unpacker {
    platform: Platform,
    overwrite: bool,
    root: PathBuf,
    progress: Progress,
    skipped: SkipList,
    sender_skipped: SkipList,
    /// Folders that were skipped, so their contents are skipped without a report each.
    skipped_dirs: Vec<PathBuf>,
    /// Folder whose path was last checked for links, which most next entries share.
    checked_parent: Option<PathBuf>,
    /// Whether the sender's closing report entry arrived.
    saw_end: bool,
    /// On systems that ignore case, the lowercased path of every entry written, with the name it had.
    written: HashMap<String, String>,
}

fn read_error(error: &io::Error) -> String {
    format!("reading the transfer failed: {error}")
}

impl Unpacker {
    fn run<R: Read>(&mut self, reader: R, dest: &Path) -> Result<(), String> {
        let mut archive = Archive::new(reader);
        let mut entries = archive.entries().map_err(|e| read_error(&e))?;
        let mut first = entries
            .next()
            .ok_or("the transfer held no data")?
            .map_err(|e| read_error(&e))?;
        let name = self.root_name(&first)?;
        let wanted = match first.header().entry_type() {
            EntryType::Directory => Wanted::Folder,
            EntryType::Regular | EntryType::Continuous => Wanted::File,
            other => {
                return Err(format!(
                    "the transfer starts with an unexpected entry ({other:?})"
                ));
            }
        };
        let found = rules::destination(
            dest,
            followed_kind(dest),
            &name,
            wanted,
            self.overwrite,
            kind_at,
        )?;
        self.root = found.path;
        if let Some(parent) = self.root.parent() {
            fs::create_dir_all(parent).map_err(|e| io_message(&e, parent))?;
        }
        let root = self.root.clone();
        if wanted == Wanted::Folder {
            self.make_dir(&root, found.action)?;
        } else {
            self.write_file(&mut first, &root)?;
        }

        for entry in entries {
            let mut entry = entry.map_err(|e| read_error(&e))?;
            self.entry(&mut entry, &name, wanted)?;
        }
        if !self.saw_end {
            return Err(
                "the transfer ended before the sender finished, so files may be missing".to_owned(),
            );
        }
        Ok(())
    }

    fn root_name<R: Read>(&self, entry: &tar::Entry<'_, R>) -> Result<String, String> {
        let path = entry.path().map_err(|e| read_error(&e))?;
        let names = rules::inside(&path).map_err(|_| "the transfer starts with a bad path")?;
        let [name] = names.as_slice() else {
            return Err("the transfer must start with one file or folder".to_owned());
        };
        let name = name
            .to_str()
            .ok_or("the name of the first entry is not valid UTF-8")?;
        match rules::name_problem(name, self.platform) {
            Some(problem) => Err(format!("{name} cannot be created here: {problem}")),
            None => Ok(name.to_owned()),
        }
    }

    fn entry<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        root_name: &str,
        root: Wanted,
    ) -> Result<(), String> {
        let raw = entry.path().map_err(|e| read_error(&e))?.into_owned();
        if raw == Path::new(SKIP_REPORT_ENTRY) {
            return self.read_report(entry);
        }
        let kind = entry.header().entry_type();
        if matches!(kind, EntryType::XGlobalHeader) {
            return Ok(());
        }
        if root == Wanted::File {
            return Err("the transfer holds more than one file where one was expected".to_owned());
        }
        let names = rules::inside(&raw).map_err(|problem| {
            let why = match problem {
                PathProblem::Absolute => "is an absolute path",
                PathProblem::ParentDir => "contains ..",
                PathProblem::Empty => "is empty",
            };
            format!(
                "the transfer holds an entry whose path {why}: {}",
                raw.display()
            )
        })?;
        if names[0] != root_name {
            return Err(format!(
                "the transfer holds an entry outside its root: {}",
                raw.display()
            ));
        }
        let mut rel = PathBuf::new();
        for name in &names[1..] {
            rel.push(name);
        }
        if rel.as_os_str().is_empty() {
            return Ok(());
        }
        if self.skipped_dirs.iter().any(|dir| rel.starts_with(dir)) {
            return Ok(());
        }
        if let Some(problem) = self.name_problem(&rel) {
            if kind == EntryType::Directory {
                self.skipped_dirs.push(rel.clone());
            }
            self.skipped.push(rel.display().to_string(), problem);
            return Ok(());
        }
        if let Some(first) = self.case_clash(&rel) {
            if kind == EntryType::Directory {
                self.skipped_dirs.push(rel.clone());
            }
            self.skipped.push(
                rel.display().to_string(),
                format!(
                    "it differs only by case from {first}, and this system treats them as one name"
                ),
            );
            return Ok(());
        }
        let target = self.prepare_parent(&rel)?;
        match kind {
            EntryType::Directory => {
                let action = rules::check_existing(
                    &target,
                    kind_at(&target),
                    Wanted::Folder,
                    self.overwrite,
                )?;
                self.make_dir(&target, action)
            }
            EntryType::Regular | EntryType::Continuous => self.write_file(entry, &target),
            EntryType::Symlink => self.write_link(entry, &rel, &target),
            other => {
                self.skipped.push(
                    rel.display().to_string(),
                    format!("it is not a file, folder, or link ({other:?})"),
                );
                Ok(())
            }
        }
    }

    fn read_report<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<(), String> {
        let mut json = Vec::new();
        entry
            .take(MAX_REPORT_BYTES)
            .read_to_end(&mut json)
            .map_err(|e| read_error(&e))?;
        if let Ok(list) = serde_json::from_slice::<SkipList>(&json) {
            self.sender_skipped = list;
        }
        self.saw_end = true;
        Ok(())
    }

    /// The earlier entry that `rel` collides with on a system that ignores case, remembering `rel` otherwise.
    fn case_clash(&mut self, rel: &Path) -> Option<String> {
        if !self.platform.ignores_case() {
            return None;
        }
        let shown = rel.display().to_string();
        match self.written.entry(shown.to_lowercase()) {
            Entry::Occupied(known) if *known.get() != shown => Some(known.get().clone()),
            Entry::Occupied(_) => None,
            Entry::Vacant(slot) => {
                slot.insert(shown);
                None
            }
        }
    }

    fn name_problem(&self, rel: &Path) -> Option<String> {
        for part in rel.components() {
            let name = part.as_os_str();
            let Some(name) = name.to_str() else {
                return Some("the name is not valid UTF-8".to_owned());
            };
            if let Some(problem) = rules::name_problem(name, self.platform) {
                return Some(problem);
            }
        }
        None
    }

    /// The path for `rel` under the root, after making sure every folder above it is a real folder.
    fn prepare_parent(&mut self, rel: &Path) -> Result<PathBuf, String> {
        let target = self.root.join(rel);
        let Some(parent_rel) = rel.parent().filter(|p| !p.as_os_str().is_empty()) else {
            return Ok(target);
        };
        let parent = self.root.join(parent_rel);
        if self.checked_parent.as_deref() == Some(parent.as_path()) {
            return Ok(target);
        }
        let mut current = self.root.clone();
        for part in parent_rel.components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() => {}
                Ok(meta) if meta.is_symlink() => {
                    return Err(format!(
                        "{} is a symbolic link, so entries under it are refused",
                        current.display()
                    ));
                }
                Ok(_) => {
                    return Err(format!(
                        "{} is a file where a folder is needed",
                        current.display()
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|e| io_message(&e, &current))?;
                    self.progress.folders += 1;
                }
                Err(error) => return Err(io_message(&error, &current)),
            }
        }
        self.checked_parent = Some(parent);
        Ok(target)
    }

    fn make_dir(&mut self, path: &Path, action: Action) -> Result<(), String> {
        if action != Action::Merge {
            fs::create_dir_all(path).map_err(|e| io_message(&e, path))?;
        }
        self.progress.folders += 1;
        Ok(())
    }

    fn write_file<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        target: &Path,
    ) -> Result<(), String> {
        rules::check_existing(target, kind_at(target), Wanted::File, self.overwrite)?;
        let parent = target
            .parent()
            .ok_or_else(|| format!("{} has no folder to write in", target.display()))?;
        let temp = Temp::new(parent);
        let mode = entry.header().mode().unwrap_or(0o644);
        let modified = entry.header().mtime().ok();
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp.path())
            .map_err(|e| io_message(&e, target))?;
        let mut bytes = 0;
        let copied = {
            let mut out = BufWriter::with_capacity(WRITE_BUFFER, file);
            let mut input = Counted {
                inner: &mut *entry,
                bytes: &mut bytes,
            };
            io::copy(&mut input, &mut out)
                .and_then(|_| out.into_inner().map_err(io::IntoInnerError::into_error))
        };
        self.progress.bytes += bytes;
        let file = copied.map_err(|e| format!("writing {} failed: {e}", target.display()))?;
        if bytes != entry.size() {
            return Err(format!(
                "the transfer ended in the middle of {}",
                target.display()
            ));
        }
        set_mode(&file, mode).map_err(|e| io_message(&e, target))?;
        if let Some(secs) = modified.filter(|secs| *secs > 0) {
            let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
        }
        drop(file);
        temp.place(target).map_err(|e| io_message(&e, target))?;
        self.progress.files += 1;
        Ok(())
    }

    fn write_link<R: Read>(
        &mut self,
        entry: &tar::Entry<'_, R>,
        rel: &Path,
        target: &Path,
    ) -> Result<(), String> {
        let link = entry
            .link_name()
            .map_err(|e| read_error(&e))?
            .ok_or("a link in the transfer has no target")?
            .into_owned();
        let action = rules::check_existing(target, kind_at(target), Wanted::Link, self.overwrite)?;
        if action == Action::Replace {
            fs::remove_file(target).map_err(|e| io_message(&e, target))?;
        }
        match make_link(&link, target) {
            Ok(()) => self.progress.files += 1,
            Err(error) if self.platform == Platform::Windows => self.skipped.push(
                rel.display().to_string(),
                format!("Windows would not create the link: {error}"),
            ),
            Err(error) => return Err(io_message(&error, target)),
        }
        Ok(())
    }
}

fn io_message(error: &io::Error, path: &Path) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the signature of the Unix version"
)]
fn set_mode(_file: &fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_link(link: &Path, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(link, at)
}

#[cfg(windows)]
fn make_link(link: &Path, at: &Path) -> io::Result<()> {
    let points_at_folder = at.parent().is_some_and(|parent| parent.join(link).is_dir());
    if points_at_folder {
        std::os::windows::fs::symlink_dir(link, at)
    } else {
        std::os::windows::fs::symlink_file(link, at)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tar::{Builder, Header};

    use super::*;
    use crate::pack::pack;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("computer-transfer-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("temp dir is creatable");
            Self(fs::canonicalize(path).expect("temp dir exists"))
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn packed(source: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        pack(source, &mut out, Platform::current()).unwrap();
        out
    }

    fn put(dest: &Path, archive: &[u8], overwrite: bool) -> Result<Unpacked, Failure> {
        unpack(Cursor::new(archive), dest, overwrite, Platform::current())
    }

    fn raw_archive(entries: &[(&str, EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (path, kind, data, link) in entries {
            let mut header = Header::new_gnu();
            header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            if let Some(link) = link {
                header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        if !entries.iter().any(|(path, ..)| *path == SKIP_REPORT_ENTRY) {
            let empty = serde_json::to_vec(&SkipList::default()).unwrap();
            let mut header = Header::new_gnu();
            header.as_old_mut().name[..SKIP_REPORT_ENTRY.len()]
                .copy_from_slice(SKIP_REPORT_ENTRY.as_bytes());
            header.set_size(empty.len() as u64);
            header.set_cksum();
            builder.append(&header, empty.as_slice()).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                let shown = path.strip_prefix(dir).unwrap().to_string_lossy();
                found.push(shown.replace('\\', "/"));
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    stack.push(path);
                }
            }
        }
        found.sort();
        found
    }

    fn source_tree(base: &Dir) -> PathBuf {
        let root = base.join("tree");
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("deep/er")).unwrap();
        let binary: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        fs::write(root.join("blob.bin"), &binary).unwrap();
        fs::write(root.join("deep/er/note.txt"), "héllo\n").unwrap();
        root
    }

    #[test]
    fn a_folder_arrives_with_its_bytes_and_empty_folders() {
        let base = Dir::new();
        let root = source_tree(&base);
        let out = Dir::new();
        let done = put(&out.0, &packed(&root), false).unwrap();

        assert_eq!(done.path, out.join("tree"));
        assert_eq!(
            done.progress,
            Progress {
                files: 2,
                folders: 4,
                bytes: 300_000 + "héllo\n".len() as u64
            }
        );
        assert_eq!(
            fs::read(out.join("tree/blob.bin")).unwrap(),
            fs::read(root.join("blob.bin")).unwrap()
        );
        assert_eq!(
            fs::read_to_string(out.join("tree/deep/er/note.txt")).unwrap(),
            "héllo\n"
        );
        assert!(out.join("tree/empty").is_dir());
        assert!(done.skipped.is_empty());
    }

    #[test]
    fn a_file_goes_inside_an_existing_folder_or_takes_the_new_name() {
        let base = Dir::new();
        let file = base.join("report.txt");
        fs::write(&file, "data").unwrap();
        let archive = packed(&file);

        let out = Dir::new();
        let inside = put(&out.0, &archive, false).unwrap();
        assert_eq!(inside.path, out.join("report.txt"));

        let renamed = put(&out.join("new/name.md"), &archive, false).unwrap();
        assert_eq!(renamed.path, out.join("new/name.md"));
        assert_eq!(fs::read_to_string(out.join("new/name.md")).unwrap(), "data");
    }

    #[test]
    fn an_existing_folder_is_refused_by_name_and_nothing_in_it_is_touched() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let out = Dir::new();
        fs::create_dir(out.join("tree")).unwrap();
        fs::write(out.join("tree/keep.txt"), "mine").unwrap();
        fs::write(out.join("tree/blob.bin"), "old").unwrap();

        let error = put(&out.0, &archive, false).unwrap_err();
        assert_eq!(
            files_under(&out.0),
            ["tree", "tree/blob.bin", "tree/keep.txt"]
        );
        assert_eq!(
            fs::read_to_string(out.join("tree/blob.bin")).unwrap(),
            "old"
        );
        assert!(
            error.message.contains("tree") && error.message.contains("overwrite"),
            "{error}"
        );
        assert_eq!(error.progress, Progress::default());
    }

    #[test]
    fn overwrite_merges_a_folder_replacing_same_named_files_and_keeping_others() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let out = Dir::new();
        fs::create_dir_all(out.join("tree/deep")).unwrap();
        fs::write(out.join("tree/keep.txt"), "mine").unwrap();
        fs::write(out.join("tree/blob.bin"), "old").unwrap();

        put(&out.0, &archive, true).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("tree/keep.txt")).unwrap(),
            "mine"
        );
        assert_eq!(fs::read(out.join("tree/blob.bin")).unwrap().len(), 300_000);
        assert!(out.join("tree/deep/er/note.txt").is_file());
        assert!(
            !files_under(&out.0)
                .iter()
                .any(|name| name.contains(".computer-use-transfer-"))
        );
    }

    #[test]
    fn a_file_never_replaces_a_folder_during_a_merge() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let out = Dir::new();
        fs::create_dir_all(out.join("tree/blob.bin/inner")).unwrap();

        let error = put(&out.0, &archive, true).unwrap_err();
        assert!(error.message.contains("is a folder"), "{error}");
        assert!(out.join("tree/blob.bin/inner").is_dir());
    }

    #[test]
    fn a_folder_cannot_replace_a_file_at_the_root() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let out = Dir::new();
        fs::write(out.join("target"), "file").unwrap();
        let error = put(&out.join("target"), &archive, true).unwrap_err();
        assert!(error.message.contains("not a folder"), "{error}");
        assert_eq!(fs::read_to_string(out.join("target")).unwrap(), "file");
    }

    #[test]
    fn a_cut_off_stream_leaves_no_half_file_and_no_temporary_file() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let cut = &archive[..archive.len() / 2];
        let out = Dir::new();

        let error = put(&out.0, cut, false).unwrap_err();
        assert!(
            !files_under(&out.0)
                .iter()
                .any(|name| name.contains(".computer-use-transfer-"))
        );
        assert!(!out.join("tree/blob.bin").exists());
        assert!(error.progress.bytes < 300_000, "{error:?}");
    }

    #[test]
    fn absolute_and_parent_paths_are_refused() {
        for path in ["tree/../../escape", "/etc/escape"] {
            let archive = raw_archive(&[
                ("tree/", EntryType::Directory, b"", None),
                (path, EntryType::Regular, b"x", None),
            ]);
            let out = Dir::new();
            let error = put(&out.join("dest"), &archive, false).unwrap_err();
            assert!(
                error.message.contains("..") || error.message.contains("absolute"),
                "{path}: {error}"
            );
            assert_eq!(files_under(&out.0), ["dest"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_cannot_go_through_a_link_made_earlier_in_the_archive() {
        let outside = Dir::new();
        let archive = raw_archive(&[
            ("tree/", EntryType::Directory, b"", None),
            (
                "tree/out",
                EntryType::Symlink,
                b"",
                Some(outside.0.to_str().unwrap()),
            ),
            ("tree/out/pwned", EntryType::Regular, b"x", None),
        ]);
        let out = Dir::new();
        let error = put(&out.0, &archive, false).unwrap_err();
        assert!(error.message.contains("symbolic link"), "{error}");
        assert!(files_under(&outside.0).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn executables_links_and_pipes_are_handled_like_cp_would() {
        use std::os::unix::fs::PermissionsExt;

        let base = Dir::new();
        let root = source_tree(&base);
        fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("blob.bin", root.join("link")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/absolute", root.join("dangling")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .unwrap();
        assert!(made.success());

        let out = Dir::new();
        let done = put(&out.0, &packed(&root), false).unwrap();
        let mode = |name: &str| fs::metadata(out.join(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("tree/run.sh"), 0o755);
        assert_eq!(mode("tree/blob.bin"), 0o644);
        assert_eq!(
            fs::read_link(out.join("tree/link")).unwrap(),
            Path::new("blob.bin")
        );
        assert_eq!(
            fs::read_link(out.join("tree/dangling")).unwrap(),
            Path::new("/nonexistent/absolute")
        );
        assert!(!out.join("tree/pipe").exists());
        assert_eq!(done.skipped.entries.len(), 1);
        assert_eq!(done.skipped.entries[0].path, "tree/pipe");

        let again = put(&out.0, &packed(&root), true).unwrap();
        assert_eq!(again.progress.files, done.progress.files);
        let refused = put(&out.0, &packed(&root), false).unwrap_err();
        assert!(refused.message.contains("already exists"), "{refused}");
    }

    #[test]
    fn names_the_target_cannot_hold_are_skipped_with_their_contents_and_listed() {
        let archive = raw_archive(&[
            ("tree/", EntryType::Directory, b"", None),
            ("tree/fine.txt", EntryType::Regular, b"ok", None),
            ("tree/a:b.txt", EntryType::Regular, b"x", None),
            ("tree/CON/", EntryType::Directory, b"", None),
            ("tree/CON/inner.txt", EntryType::Regular, b"x", None),
        ]);
        let out = Dir::new();
        let done = unpack(Cursor::new(archive), &out.0, false, Platform::Windows).unwrap();
        assert_eq!(files_under(&out.0), ["tree", "tree/fine.txt"]);
        let skipped: Vec<_> = done
            .skipped
            .entries
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(skipped, ["a:b.txt", "CON"]);
    }

    #[test]
    fn the_senders_skips_follow_the_receivers_own() {
        let mut sent = SkipList::default();
        sent.push("tree/pipe", "it is a pipe");
        let report = serde_json::to_vec(&sent).unwrap();
        let archive = raw_archive(&[
            ("tree/", EntryType::Directory, b"", None),
            ("tree/a:b", EntryType::Regular, b"x", None),
            (SKIP_REPORT_ENTRY, EntryType::Regular, &report, None),
        ]);
        let out = Dir::new();
        let done = unpack(Cursor::new(archive), &out.0, false, Platform::Windows).unwrap();
        let skipped: Vec<_> = done
            .skipped
            .entries
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(skipped, ["a:b", "tree/pipe"]);
    }

    #[test]
    fn a_stream_cut_exactly_between_entries_is_not_a_finished_transfer() {
        let base = Dir::new();
        let archive = packed(&source_tree(&base));
        let report_and_end = 512 + 512 + 1024;
        let cut = &archive[..archive.len() - report_and_end];
        let out = Dir::new();

        let error = put(&out.0, cut, false).unwrap_err();
        assert!(error.message.contains("ended before"), "{error}");
        assert_eq!(
            error.progress.files, 2,
            "everything before the cut was written"
        );
        assert!(put(&out.join("again"), &archive, false).is_ok());
    }

    #[test]
    fn names_that_differ_only_by_case_are_skipped_where_case_is_ignored() {
        let archive = raw_archive(&[
            ("tree/", EntryType::Directory, b"", None),
            ("tree/A.txt", EntryType::Regular, b"first", None),
            ("tree/a.txt", EntryType::Regular, b"second", None),
            ("tree/Dir/", EntryType::Directory, b"", None),
            ("tree/dir/", EntryType::Directory, b"", None),
            ("tree/dir/inner", EntryType::Regular, b"x", None),
        ]);
        let out = Dir::new();
        let done = unpack(Cursor::new(archive), &out.0, true, Platform::Windows).unwrap();
        assert_eq!(files_under(&out.0), ["tree", "tree/A.txt", "tree/Dir"]);
        assert_eq!(fs::read_to_string(out.join("tree/A.txt")).unwrap(), "first");
        let skipped: Vec<_> = done
            .skipped
            .entries
            .iter()
            .map(|s| {
                (
                    s.path.as_str(),
                    s.reason.contains("differs only by case from"),
                )
            })
            .collect();
        assert_eq!(skipped, [("a.txt", true), ("dir", true)]);
    }

    #[test]
    fn check_destination_applies_the_root_rules_before_any_data_moves() {
        let out = Dir::new();
        fs::create_dir(out.join("taken")).unwrap();
        fs::write(out.join("file"), "x").unwrap();
        let check = |dest: &str, name: &str, folder, overwrite| {
            check_destination(&out.join(dest), name, folder, overwrite, Platform::Unix)
        };
        assert_eq!(
            check(".", "new", true, false),
            Ok(out.join(".").join("new"))
        );
        let error = check(".", "taken", true, false).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(check(".", "taken", true, true).is_ok());
        assert!(
            check("file", "x", true, true)
                .unwrap_err()
                .contains("not a folder")
        );
        let windows = check_destination(&out.0, "a:b", false, false, Platform::Windows);
        assert!(windows.unwrap_err().contains("cannot be created"));
    }
}
