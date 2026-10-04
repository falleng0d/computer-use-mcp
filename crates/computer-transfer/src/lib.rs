//! Packing and unpacking of files and folders for transfers between the host and the computer.
//!
//! Both sides speak tar. Packing and unpacking are synchronous, so callers run them with
//! `spawn_blocking` and connect them to async streams with the types in [`pipe`].

mod pack;
pub mod pipe;
pub mod rules;
mod unpack;

use std::fmt;

pub use pack::{Packed, pack};
pub use rules::Platform;
pub use unpack::{Unpacked, unpack};

/// What a transfer did before it stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// A transfer that stopped, with how far it got.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub progress: Progress,
}

impl Failure {
    pub(crate) fn new(message: impl Into<String>, progress: Progress) -> Self {
        Self {
            message: message.into(),
            progress,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Progress {
            files,
            folders,
            bytes,
        } = self.progress;
        if files == 0 && folders == 0 && bytes == 0 {
            write!(f, "{}. Nothing was copied.", self.message)
        } else {
            write!(
                f,
                "{}. Before it stopped, {files} files, {folders} folders and {bytes} bytes were copied.",
                self.message
            )
        }
    }
}

impl std::error::Error for Failure {}
