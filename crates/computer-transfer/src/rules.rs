//! Decisions about where entries land and which names are allowed. No file system access.

use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

/// The system entries are written to, which decides the names and links it can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

impl Platform {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// What exists at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    Link,
    Other,
}

/// What an archive entry wants to put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    File,
    Folder,
    Link,
}

/// What to do with an entry once its place is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Replace,
    Merge,
}

/// Decides what to do with an entry that wants `wanted` at `path`, where `existing` is what is there now.
///
/// # Errors
///
/// Refuses, naming the path, when something other than a folder exists and `overwrite` is off, or
/// when a file would meet a folder. A folder meeting a folder merges, since conflicts between
/// their contents are decided entry by entry.
pub fn check_existing(
    path: &Path,
    existing: Option<Kind>,
    wanted: Wanted,
    overwrite: bool,
) -> Result<Action, String> {
    let shown = path.display();
    let Some(existing) = existing else {
        return Ok(Action::Create);
    };
    match (existing, wanted) {
        (Kind::Folder, Wanted::Folder) => Ok(Action::Merge),
        (Kind::Folder, _) => Err(format!(
            "{shown} is a folder and a file would replace it, which is never done"
        )),
        (_, Wanted::Folder) => Err(format!(
            "{shown} is not a folder and a folder would replace it, which is never done"
        )),
        _ if !overwrite => Err(already_exists(path)),
        (Kind::Other, _) => Err(format!(
            "{shown} is a pipe, device, or socket and cannot be replaced"
        )),
        (Kind::File | Kind::Link, _) => Ok(Action::Replace),
    }
}

fn already_exists(path: &Path) -> String {
    format!(
        "{} already exists, set overwrite to true to replace it",
        path.display()
    )
}

/// Where the root of a transfer lands and what happens there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    pub action: Action,
}

/// Applies the destination rule of `cp -r`.
///
/// An existing folder at `dest` receives the source under its own `name`. Anything else makes
/// `dest` the new path. `dest_kind` is what `dest` is with links followed. `probe` tells what
/// exists at the final path without following a link.
///
/// # Errors
///
/// Refuses with the reason from [`check_existing`], and also when the final path is an existing
/// folder and `overwrite` is off.
pub fn destination(
    dest: &Path,
    dest_kind: Option<Kind>,
    name: &str,
    source: Wanted,
    overwrite: bool,
    probe: impl FnOnce(&Path) -> Option<Kind>,
) -> Result<Destination, String> {
    let (path, existing) = if dest_kind == Some(Kind::Folder) {
        let inside = dest.join(name);
        let existing = probe(&inside);
        (inside, existing)
    } else {
        (dest.to_path_buf(), probe(dest))
    };
    let action = check_existing(&path, existing, source, overwrite)?;
    if action == Action::Merge && !overwrite {
        return Err(already_exists(&path));
    }
    Ok(Destination { path, action })
}

/// Why a path inside an archive is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathProblem {
    Absolute,
    ParentDir,
    Empty,
}

/// The names of `path` when it stays inside its root: every part is a plain name or `.`.
///
/// # Errors
///
/// Fails for absolute paths, drive prefixes, `..`, and paths with no name.
pub fn inside(path: &Path) -> Result<Vec<&OsStr>, PathProblem> {
    let mut names = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(PathProblem::ParentDir),
            Component::RootDir | Component::Prefix(_) => return Err(PathProblem::Absolute),
        }
    }
    if names.is_empty() {
        Err(PathProblem::Empty)
    } else {
        Ok(names)
    }
}

const WINDOWS_BAD_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const WINDOWS_DEVICES: &[&str] = &["CON", "PRN", "AUX", "NUL"];

/// Why `name` cannot exist as a file or folder name on `platform`, when it cannot.
#[must_use]
pub fn name_problem(name: &str, platform: Platform) -> Option<String> {
    if name.is_empty() {
        return Some("the name is empty".to_owned());
    }
    if name.contains('\0') {
        return Some("the name contains a null character".to_owned());
    }
    if platform == Platform::Unix {
        return None;
    }
    if let Some(bad) = name
        .chars()
        .find(|c| WINDOWS_BAD_CHARS.contains(c) || c.is_control())
    {
        return Some(format!(
            "Windows does not allow {} in names",
            bad.escape_debug()
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows does not allow names that end in a dot or a space".to_owned());
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
    };
    if WINDOWS_DEVICES.contains(&stem.as_str()) || numbered("COM") || numbered("LPT") {
        return Some(format!("{stem} is a reserved device name on Windows"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(
        dest_kind: Option<Kind>,
        existing: Option<Kind>,
        source: Wanted,
        overwrite: bool,
    ) -> Result<Destination, String> {
        destination(Path::new("/d"), dest_kind, "x", source, overwrite, |_| {
            existing
        })
    }

    #[test]
    fn an_existing_folder_receives_the_source_under_its_own_name() {
        let got = dest(Some(Kind::Folder), None, Wanted::File, false).unwrap();
        assert_eq!(got.path, Path::new("/d/x"));
        assert_eq!(got.action, Action::Create);
    }

    #[test]
    fn anything_else_makes_the_destination_the_new_name() {
        let got = dest(None, None, Wanted::Folder, false).unwrap();
        assert_eq!(got.path, Path::new("/d"));
        assert_eq!(got.action, Action::Create);
        let got = dest(Some(Kind::File), Some(Kind::File), Wanted::File, true).unwrap();
        assert_eq!(got.path, Path::new("/d"));
        assert_eq!(got.action, Action::Replace);
    }

    #[test]
    fn an_existing_final_path_is_refused_by_name_unless_overwrite_is_on() {
        let error = dest(Some(Kind::Folder), Some(Kind::File), Wanted::File, false).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        let got = dest(Some(Kind::Folder), Some(Kind::File), Wanted::File, true).unwrap();
        assert_eq!(got.action, Action::Replace);
        let got = dest(Some(Kind::Folder), Some(Kind::Link), Wanted::Link, true).unwrap();
        assert_eq!(got.action, Action::Replace);
    }

    #[test]
    fn a_root_folder_that_exists_is_refused_unless_overwrite_merges_into_it() {
        let existing = Some(Kind::Folder);
        let error = dest(Some(Kind::Folder), existing, Wanted::Folder, false).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        let got = dest(Some(Kind::Folder), existing, Wanted::Folder, true).unwrap();
        assert_eq!(got.action, Action::Merge);
        assert_eq!(
            check_existing(Path::new("/d/x"), existing, Wanted::Folder, false),
            Ok(Action::Merge)
        );
    }

    #[test]
    fn a_file_never_replaces_a_folder_or_the_other_way_round() {
        for overwrite in [false, true] {
            let error = dest(
                Some(Kind::Folder),
                Some(Kind::Folder),
                Wanted::File,
                overwrite,
            )
            .unwrap_err();
            assert!(error.contains("is a folder"), "{error}");
            for existing in [Kind::File, Kind::Link] {
                let error = dest(
                    Some(Kind::Folder),
                    Some(existing),
                    Wanted::Folder,
                    overwrite,
                )
                .unwrap_err();
                assert!(error.contains("not a folder"), "{error}");
            }
        }
    }

    #[test]
    fn pipes_and_devices_are_never_replaced() {
        let error = dest(Some(Kind::Folder), Some(Kind::Other), Wanted::File, true).unwrap_err();
        assert!(error.contains("pipe, device, or socket"), "{error}");
    }

    #[test]
    fn entry_paths_stay_inside_their_root() {
        let count = |p: &str| inside(Path::new(p)).map(|v| v.len());
        assert_eq!(count("a/b/c.txt"), Ok(3));
        assert_eq!(count("./a/b"), Ok(2));
        assert_eq!(count("/etc/passwd"), Err(PathProblem::Absolute));
        assert_eq!(count("a/../../b"), Err(PathProblem::ParentDir));
        assert_eq!(count(".."), Err(PathProblem::ParentDir));
        assert_eq!(count("."), Err(PathProblem::Empty));
    }

    #[test]
    fn windows_names_are_checked_and_unix_names_are_not() {
        let bad = |name: &str| name_problem(name, Platform::Windows).is_some();
        for name in [
            "a:b",
            "a?",
            "a*",
            "q\"q",
            "a<b",
            "a>b",
            "a|b",
            "a\\b",
            "dots.",
            "space ",
            "CON",
            "con.txt",
            "Nul",
            "aux.tar.gz",
            "COM1",
            "lpt9.log",
            "tab\tname",
        ] {
            assert!(bad(name), "{name} should be refused");
        }
        for name in [
            "file.txt", "COM0", "COM10", "CONSOLE", "lpt", ".hidden", "a b", "é",
        ] {
            assert!(!bad(name), "{name} should be allowed");
        }
        assert!(name_problem("a:b?", Platform::Unix).is_none());
        assert!(name_problem("", Platform::Unix).is_some());
        assert!(name_problem("a\0b", Platform::Unix).is_some());
    }
}
