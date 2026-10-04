use serde::{Deserialize, Serialize};

/// Largest `write_file` content, in bytes.
pub const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;

/// Largest image `read_file` returns, in bytes.
pub const MAX_IMAGE_BYTES: usize = 1024 * 1024;

/// Body of `POST /sessions/{id}/files/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilesRequest {
    /// Folder to list. `None` means the session's working folder.
    pub path: Option<String>,
}

/// What a folder entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Folder,
    Symlink,
    Other,
}

/// One entry of a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    /// Size in bytes. For a symlink, the size of the link itself.
    pub size: u64,
    /// Last change time as RFC 3339, when the system knows it.
    pub modified: Option<String>,
}

/// Reply to `POST /sessions/{id}/files/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilesReply {
    /// Absolute path of the folder that was listed.
    pub path: String,
    /// Folders first, then files, each group sorted by name.
    pub entries: Vec<FileEntry>,
    /// Entries left out because the folder holds more than the limit.
    pub omitted: u64,
}

/// Body of `POST /sessions/{id}/files/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
    /// First line to return, counting from 1. Text files only.
    pub offset: Option<u64>,
    /// Number of lines to return. Text files only.
    pub limit: Option<u64>,
}

/// Image formats `read_file` returns as images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    Png,
    Jpeg,
}

impl ImageType {
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// Reply to `POST /sessions/{id}/files/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadFileReply {
    Text {
        /// Absolute path that was read.
        path: String,
        text: String,
        /// How to read what was left out, when something was.
        note: Option<String>,
    },
    Image {
        path: String,
        image_type: ImageType,
        data_base64: String,
    },
}

/// Body of `POST /sessions/{id}/files/write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    pub content: String,
}

/// Reply to `POST /sessions/{id}/files/write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileReply {
    /// Absolute path that was written.
    pub path: String,
    pub bytes: u64,
}
