use serde::{Deserialize, Serialize};

/// Skipped entries a reply or an archive lists before it only counts the rest.
pub const MAX_LISTED_SKIPS: usize = 50;

/// Name of the last archive entry, which carries the sender's [`SkipList`] as JSON.
pub const SKIP_REPORT_ENTRY: &str = ".computerd-skipped";

/// Query of `POST /sessions/{id}/files/upload`. The body is a tar archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadQuery {
    /// Where the archive's root lands on the computer.
    pub path: String,
    /// Replace files that already exist. Folders merge.
    #[serde(default)]
    pub overwrite: bool,
}

/// Body of `POST /sessions/{id}/files/download`. The reply body is a tar archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRequest {
    /// File or folder on the computer to send.
    pub path: String,
}

/// One entry a transfer left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// The first skipped entries and how many more there were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipList {
    pub entries: Vec<Skipped>,
    pub omitted: u64,
}

impl SkipList {
    pub fn push(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        if self.entries.len() < MAX_LISTED_SKIPS {
            self.entries.push(Skipped {
                path: path.into(),
                reason: reason.into(),
            });
        } else {
            self.omitted += 1;
        }
    }

    /// Adds the entries of `other` after this list's own.
    pub fn merge(&mut self, other: Self) {
        self.omitted += other.omitted;
        for entry in other.entries {
            self.push(entry.path, entry.reason);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.omitted == 0
    }
}

/// What a finished transfer put in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferReply {
    /// Final path of the file or folder that was created or merged into.
    pub path: String,
    pub files: u64,
    pub folders: u64,
    /// File content bytes written.
    pub bytes: u64,
    pub skipped: SkipList,
}
