//! Listing, reading, and writing files for the agent's file tools.
//!
//! Everything here blocks on the file system, so callers run it with `spawn_blocking`.

use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    time::SystemTime,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use computer_protocol::{
    FileEntry, FileKind, ImageType, ListFilesReply, ListFilesRequest, MAX_IMAGE_BYTES,
    MAX_WRITE_BYTES, ReadFileReply, ReadFileRequest, WriteFileReply, WriteFileRequest,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    cap::{Capture, HEAD_BYTES, TAIL_BYTES},
    workdir,
};

const MAX_LIST_ENTRIES: usize = 1000;
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_LINK_HOPS: usize = 40;
const SNIFF_BYTES: usize = 8192;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8] = b"\xFF\xD8\xFF";
const SHELL_HINT: &str = "Inspect it with the shell tool, for example `file <path>` or `od -c <path> | head`, or copy it to the host with file_transfer.";

/// What the bytes of a file say it is.
#[derive(Debug, PartialEq, Eq)]
enum Content<'a> {
    Image(ImageType),
    Text(&'a str),
    Binary,
}

fn sniff(bytes: &[u8]) -> Content<'_> {
    if bytes.starts_with(PNG_MAGIC) {
        return Content::Image(ImageType::Png);
    }
    if bytes.starts_with(JPEG_MAGIC) {
        return Content::Image(ImageType::Jpeg);
    }
    let probe = &bytes[..bytes.len().min(SNIFF_BYTES)];
    match std::str::from_utf8(bytes) {
        Ok(text) if !probe.contains(&0) => Content::Text(text),
        _ => Content::Binary,
    }
}

/// A message the agent can act on for a failed file call on `path`.
fn io_message(error: &io::Error, path: &Path) -> String {
    let path = path.display();
    match error.kind() {
        io::ErrorKind::NotFound => format!("{path} does not exist"),
        io::ErrorKind::PermissionDenied => {
            format!("permission denied for {path}, the file tools act as the user computer")
        }
        io::ErrorKind::NotADirectory => {
            format!("{path} cannot be used because part of its path is a file, not a folder")
        }
        io::ErrorKind::IsADirectory => format!("{path} is a folder"),
        _ => format!("cannot use {path}: {error}"),
    }
}

fn kind_of(file_type: fs::FileType) -> FileKind {
    if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_dir() {
        FileKind::Folder
    } else if file_type.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

fn rfc3339(time: SystemTime) -> Option<String> {
    OffsetDateTime::from(time).format(&Rfc3339).ok()
}

/// Lists one folder without descending into it.
///
/// # Errors
///
/// Fails with a message for the agent when the path is missing, unreadable, or not a folder.
pub(crate) fn list(
    cwd: &Path,
    home: &Path,
    request: &ListFilesRequest,
) -> Result<ListFilesReply, String> {
    let wanted = workdir::resolve(cwd, home, request.path.as_deref().unwrap_or("."));
    let dir = fs::canonicalize(&wanted).map_err(|error| io_message(&error, &wanted))?;
    if !dir.is_dir() {
        return Err(format!("{} is a file, not a folder", dir.display()));
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| io_message(&error, &dir))? {
        let entry = entry.map_err(|error| io_message(&error, &dir))?;
        if let Ok(file_type) = entry.file_type() {
            found.push((
                entry.file_name().to_string_lossy().into_owned(),
                kind_of(file_type),
                entry,
            ));
        }
    }
    found.sort_by(|a, b| (a.1 != FileKind::Folder, &a.0).cmp(&(b.1 != FileKind::Folder, &b.0)));
    let omitted = found.len().saturating_sub(MAX_LIST_ENTRIES) as u64;
    found.truncate(MAX_LIST_ENTRIES);
    let entries = found
        .into_iter()
        .map(|(name, kind, entry)| {
            let meta = entry.metadata().ok();
            FileEntry {
                name,
                kind,
                size: meta.as_ref().map_or(0, fs::Metadata::len),
                modified: meta.and_then(|meta| meta.modified().ok()).and_then(rfc3339),
            }
        })
        .collect();
    Ok(ListFilesReply {
        path: dir.display().to_string(),
        entries,
        omitted,
    })
}

/// Reads a text file or an image.
///
/// # Errors
///
/// Fails with a message for the agent when the path is missing, unreadable, a folder, other
/// binary data, too big, or when `offset` or `limit` is out of range.
pub(crate) fn read(
    cwd: &Path,
    home: &Path,
    request: &ReadFileRequest,
) -> Result<ReadFileReply, String> {
    let wanted = workdir::resolve(cwd, home, &request.path);
    let real = fs::canonicalize(&wanted).map_err(|error| io_message(&error, &wanted))?;
    let meta = fs::metadata(&real).map_err(|error| io_message(&error, &real))?;
    if meta.is_dir() {
        return Err(format!(
            "{} is a folder, use list_files to see what is in it",
            real.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file (a pipe, device, or socket), read_file cannot open it",
            real.display()
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(&real)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| io_message(&error, &real))?;
    interpret(&real.display().to_string(), &bytes, request)
}

fn interpret(path: &str, bytes: &[u8], request: &ReadFileRequest) -> Result<ReadFileReply, String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{path} is larger than {} MiB. Read part of it with the shell tool, for example `head`, `tail`, or `sed -n 100,200p`.",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    match sniff(bytes) {
        Content::Image(image_type) if bytes.len() <= MAX_IMAGE_BYTES => Ok(ReadFileReply::Image {
            path: path.to_owned(),
            image_type,
            data_base64: STANDARD.encode(bytes),
        }),
        Content::Image(_) => Err(format!(
            "{path} is an image of {} bytes, over the {MAX_IMAGE_BYTES} byte limit for reading images. Make a smaller copy with a shell command, for example `convert in.png -resize 50% out.png`, and read that.",
            bytes.len()
        )),
        Content::Text(text) => {
            let (text, note) = window(text, request.offset, request.limit)?;
            Ok(ReadFileReply::Text {
                path: path.to_owned(),
                text,
                note,
            })
        }
        Content::Binary => Err(format!(
            "{path} is a binary file of {} bytes, not UTF-8 text, PNG, or JPEG. {SHELL_HINT}",
            bytes.len()
        )),
    }
}

/// The lines asked for, shortened in the middle when long, and a note on how to read the rest.
fn window(
    text: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<(String, Option<String>), String> {
    let first = offset.unwrap_or(1);
    if first == 0 {
        return Err("offset counts lines from 1".to_owned());
    }
    if limit == Some(0) {
        return Err("limit must be at least 1".to_owned());
    }
    if text.is_empty() {
        return Ok((String::new(), Some("the file is empty".to_owned())));
    }
    let total = text.split_inclusive('\n').count() as u64;
    if first > total {
        return Err(format!(
            "the file has {total} lines, offset {first} is past the end"
        ));
    }
    let skip = usize::try_from(first - 1).unwrap_or(usize::MAX);
    let take = limit.map_or(usize::MAX, |limit| {
        usize::try_from(limit).unwrap_or(usize::MAX)
    });
    let skipped: usize = text.split_inclusive('\n').take(skip).map(str::len).sum();
    let rest = &text[skipped..];
    let chosen = || rest.split_inclusive('\n').take(take);
    let selected = &rest[..chosen().map(str::len).sum()];
    let last = first - 1 + chosen().count() as u64;

    let mut notes = Vec::new();
    if last < total {
        notes.push(format!(
            "showing lines {first} to {last} of {total}, call read_file again with offset {} to read on",
            last + 1
        ));
    }
    let shown = if selected.len() > HEAD_BYTES + TAIL_BYTES {
        notes.push(format!(
            "the middle was left out because the text is long, use offset and limit (in lines) to read a range, for example offset {first} and limit 200"
        ));
        let mut capture = Capture::default();
        capture.push(selected.as_bytes());
        capture.into_text()
    } else {
        selected.to_owned()
    };
    Ok((shown, (!notes.is_empty()).then(|| notes.join(". "))))
}

/// Writes a text file and creates missing folders. A reader sees the old file or the new one, never half.
///
/// # Errors
///
/// Fails with a message for the agent when the content is too big, the path is a folder or
/// cannot be created, or the system refuses.
pub(crate) fn write(
    cwd: &Path,
    home: &Path,
    request: &WriteFileRequest,
) -> Result<WriteFileReply, String> {
    if request.content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "the content is {} bytes, over the {MAX_WRITE_BYTES} byte limit for write_file. Write it in pieces, create it with the shell tool, or copy it from the host with file_transfer.",
            request.content.len()
        ));
    }
    if request.path.trim().is_empty() {
        return Err("path must not be empty".to_owned());
    }
    let wanted = workdir::resolve(cwd, home, &request.path);
    let (target, existing) = match fs::canonicalize(&wanted) {
        Ok(real) => {
            let meta = fs::metadata(&real).map_err(|error| io_message(&error, &real))?;
            if meta.is_dir() {
                return Err(format!(
                    "{} is a folder, choose a file path inside it",
                    real.display()
                ));
            }
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", real.display()));
            }
            if meta.permissions().readonly() {
                return Err(format!(
                    "permission denied for {}, the file is read-only",
                    real.display()
                ));
            }
            (real, Some(meta.permissions()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (dangling_target(&wanted), None),
        Err(error) => return Err(io_message(&error, &wanted)),
    };
    let Some(parent) = target.parent().filter(|_| target.file_name().is_some()) else {
        return Err(format!("{} is not a file path", target.display()));
    };
    fs::create_dir_all(parent).map_err(|error| io_message(&error, parent))?;

    let temp = parent.join(format!(".computerd-{}.tmp", Uuid::new_v4().simple()));
    let placed = write_temp(&temp, request.content.as_bytes(), existing)
        .and_then(|()| fs::rename(&temp, &target));
    if let Err(error) = placed {
        let _ = fs::remove_file(&temp);
        return Err(io_message(&error, &target));
    }
    let shown = fs::canonicalize(&target).unwrap_or(target);
    Ok(WriteFileReply {
        path: shown.display().to_string(),
        bytes: request.content.len() as u64,
    })
}

/// Where a write to `path` lands when nothing exists there: the end of a chain of dangling symlinks, or `path` itself.
fn dangling_target(path: &Path) -> std::path::PathBuf {
    let mut current = lexical(path);
    for _ in 0..MAX_LINK_HOPS {
        let Ok(link) = fs::read_link(&current) else {
            break;
        };
        let base = current.parent().unwrap_or(Path::new("/"));
        current = lexical(&base.join(link));
    }
    current
}

/// Removes `.` and `..` from a path that does not exist yet.
fn lexical(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn write_temp(temp: &Path, content: &[u8], permissions: Option<fs::Permissions>) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)?;
    file.write_all(content)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn read_request(
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> ReadFileRequest {
        ReadFileRequest {
            path: path.to_owned(),
            offset,
            limit,
        }
    }

    #[test]
    fn content_is_told_apart_by_its_bytes_not_its_name() {
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\nrest"),
            Content::Image(ImageType::Png)
        );
        assert_eq!(
            sniff(b"\xFF\xD8\xFF\xE0jfif"),
            Content::Image(ImageType::Jpeg)
        );
        assert_eq!(sniff("héllo\n".as_bytes()), Content::Text("héllo\n"));
        assert_eq!(sniff(b""), Content::Text(""));
        assert_eq!(sniff(b"ELF\0\x01\x02"), Content::Binary);
        assert_eq!(sniff(b"caf\xE9"), Content::Binary);
    }

    #[test]
    fn a_line_window_says_where_to_continue() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(window(text, None, None).unwrap(), (text.to_owned(), None));
        let (shown, note) = window(text, Some(2), Some(2)).unwrap();
        assert_eq!(shown, "b\nc\n");
        let note = note.unwrap();
        assert!(note.contains("lines 2 to 3 of 5"), "{note}");
        assert!(note.contains("offset 4"), "{note}");
        assert_eq!(
            window(text, Some(5), Some(9)).unwrap(),
            ("e".to_owned(), None)
        );
        assert!(window(text, Some(6), None).unwrap_err().contains("5 lines"));
        assert!(window(text, Some(0), None).is_err());
        assert!(window(text, None, Some(0)).is_err());
    }

    #[test]
    fn long_text_keeps_both_ends_and_names_offset_and_limit() {
        let text = format!("first\n{}last\n", "xxxxxxxxx\n".repeat(10_000));
        let (shown, note) = window(&text, None, None).unwrap();
        assert!(shown.starts_with("first\n"));
        assert!(shown.ends_with("last\n"));
        assert!(shown.contains("bytes omitted"));
        let note = note.unwrap();
        assert!(note.contains("offset") && note.contains("limit"), "{note}");
    }

    #[test]
    fn images_come_back_as_images_and_other_binary_is_refused_with_a_hint() {
        let request = read_request("x", None, None);
        let png = [PNG_MAGIC, b"data"].concat();
        assert_eq!(
            interpret("/x", &png, &request).unwrap(),
            ReadFileReply::Image {
                path: "/x".to_owned(),
                image_type: ImageType::Png,
                data_base64: STANDARD.encode(&png),
            }
        );
        let big = [PNG_MAGIC, &vec![0; MAX_IMAGE_BYTES]].concat();
        let error = interpret("/x", &big, &request).unwrap_err();
        assert!(error.contains("smaller copy"), "{error}");
        let binary = interpret("/x", b"\0\x01", &request).unwrap_err();
        assert!(binary.contains("shell tool"), "{binary}");
    }
}

#[cfg(all(test, unix))]
mod fs_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{tests::read_request, *};

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("computerd-files-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("temp dir is creatable");
            Self(fs::canonicalize(path).expect("temp dir exists"))
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_request(path: &str, content: &str) -> WriteFileRequest {
        WriteFileRequest {
            path: path.to_owned(),
            content: content.to_owned(),
        }
    }

    fn list_request(path: Option<&str>) -> ListFilesRequest {
        ListFilesRequest {
            path: path.map(str::to_owned),
        }
    }

    #[test]
    fn folders_list_first_then_files_by_name_with_types_and_sizes() {
        let dir = Dir::new();
        fs::create_dir(dir.0.join("zdir")).unwrap();
        fs::write(dir.0.join("b.txt"), "12345").unwrap();
        fs::write(dir.0.join("a.txt"), "").unwrap();
        std::os::unix::fs::symlink("b.txt", dir.0.join("link")).unwrap();

        let reply = list(&dir.0, &dir.0, &list_request(None)).unwrap();
        let shape: Vec<_> = reply
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.modified.is_some()))
            .collect();
        assert_eq!(
            shape,
            [
                ("zdir", FileKind::Folder, true),
                ("a.txt", FileKind::File, true),
                ("b.txt", FileKind::File, true),
                ("link", FileKind::Symlink, true),
            ]
        );
        assert_eq!(reply.entries[2].size, 5);
        assert_eq!(reply.path, dir.0.display().to_string());
        assert_eq!(reply.omitted, 0);

        let error = list(&dir.0, &dir.0, &list_request(Some("b.txt"))).unwrap_err();
        assert!(error.ends_with("is a file, not a folder"), "{error}");
        let error = list(&dir.0, &dir.0, &list_request(Some("nope"))).unwrap_err();
        assert!(error.ends_with("nope does not exist"), "{error}");
    }

    #[test]
    fn a_folder_with_too_many_entries_reports_how_many_are_left_out() {
        let dir = Dir::new();
        for n in 0..MAX_LIST_ENTRIES + 3 {
            fs::write(dir.0.join(format!("f{n:05}")), "").unwrap();
        }
        let reply = list(&dir.0, &dir.0, &list_request(None)).unwrap();
        assert_eq!(reply.entries.len(), MAX_LIST_ENTRIES);
        assert_eq!(reply.omitted, 3);
    }

    #[test]
    fn write_creates_folders_resolves_from_the_working_folder_and_replaces_whole() {
        let dir = Dir::new();
        let reply = write(&dir.0, &dir.0, &write_request("a/x/../b/c.txt", "one")).unwrap();
        let file = dir.0.join("a/b/c.txt");
        assert_eq!(reply.path, file.display().to_string());
        assert_eq!(reply.bytes, 3);
        assert!(!dir.0.join("a/x").exists());

        fs::set_permissions(&file, fs::Permissions::from_mode(0o750)).unwrap();
        write(&dir.0, &dir.0, &write_request("a/b/c.txt", "two!")).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two!");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o750
        );
        let leftovers = fs::read_dir(file.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file stays behind");

        let back = read(
            &dir.0.join("a"),
            &dir.0,
            &read_request("b/c.txt", None, None),
        )
        .unwrap();
        assert_eq!(
            back,
            ReadFileReply::Text {
                path: file.display().to_string(),
                text: "two!".to_owned(),
                note: None
            }
        );
    }

    #[test]
    fn write_refuses_folders_files_in_the_path_and_oversized_content() {
        let dir = Dir::new();
        fs::create_dir(dir.0.join("d")).unwrap();
        fs::write(dir.0.join("f"), "x").unwrap();
        let refused = |path: &str| write(&dir.0, &dir.0, &write_request(path, "y")).unwrap_err();
        assert!(refused("d").contains("is a folder"));
        assert!(refused("f/inside").contains("is a file, not a folder"));
        assert_eq!(fs::read_to_string(dir.0.join("f")).unwrap(), "x");
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        let error = write(&dir.0, &dir.0, &write_request("big", &big)).unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
        assert!(!dir.0.join("big").exists());
    }

    #[test]
    fn tilde_paths_start_at_home_and_missing_files_say_so() {
        let dir = Dir::new();
        fs::write(dir.0.join("note"), "hi").unwrap();
        let elsewhere = Path::new("/");
        let reply = read(elsewhere, &dir.0, &read_request("~/note", None, None)).unwrap();
        assert!(matches!(reply, ReadFileReply::Text { text, .. } if text == "hi"));
        let error = read(&dir.0, &dir.0, &read_request("missing", None, None)).unwrap_err();
        assert!(error.ends_with("missing does not exist"), "{error}");
        let error = read(&dir.0, &dir.0, &read_request(".", None, None)).unwrap_err();
        assert!(error.contains("is a folder"), "{error}");
    }

    #[test]
    fn pipes_are_refused_instead_of_blocking_the_read() {
        let dir = Dir::new();
        let fifo = dir.0.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let error = read(&dir.0, &dir.0, &read_request("pipe", None, None)).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn writing_through_a_dangling_symlink_creates_its_target_and_keeps_the_link() {
        let dir = Dir::new();
        std::os::unix::fs::symlink("sub/../real.txt", dir.0.join("link")).unwrap();
        write(&dir.0, &dir.0, &write_request("link", "data")).unwrap();
        assert_eq!(fs::read_to_string(dir.0.join("real.txt")).unwrap(), "data");
        assert!(
            fs::symlink_metadata(dir.0.join("link"))
                .unwrap()
                .is_symlink()
        );
        write(&dir.0, &dir.0, &write_request("link", "more")).unwrap();
        assert_eq!(fs::read_to_string(dir.0.join("real.txt")).unwrap(), "more");
    }

    #[test]
    fn read_only_files_are_refused_and_left_alone() {
        let dir = Dir::new();
        let file = dir.0.join("locked");
        fs::write(&file, "keep").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        let error = write(&dir.0, &dir.0, &write_request("locked", "new")).unwrap_err();
        assert!(error.contains("permission denied"), "{error}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    }
}
