//! Working folders of sessions.

use std::path::{Component, Path, PathBuf};

/// Folder a session starts in and the meaning of `~`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map_or_else(|| PathBuf::from("/home/computer"), PathBuf::from)
}

/// Where `input` points when typed in `current`: `~` is `home`, a relative path starts at `current`.
pub fn resolve(current: &Path, home: &Path, input: &str) -> PathBuf {
    let input = input.trim();
    let path = if input == "~" {
        home.to_path_buf()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(input)
    };
    if path.has_root()
        || path
            .components()
            .next()
            .is_some_and(|part| matches!(part, Component::Prefix(_)))
    {
        path
    } else {
        current.join(path)
    }
}

/// Makes `input` the absolute, symlink-free path of an existing folder.
///
/// # Errors
///
/// Fails with a message for the agent when the path is not an existing folder.
pub async fn existing_dir(current: &Path, home: &Path, input: &str) -> Result<PathBuf, String> {
    let wanted = resolve(current, home, input);
    let shown = wanted.display();
    let real = tokio::fs::canonicalize(&wanted)
        .await
        .map_err(|error| format!("cannot use {shown} as the working folder: {error}"))?;
    match tokio::fs::metadata(&real).await {
        Ok(meta) if meta.is_dir() => Ok(real),
        _ => Err(format!("{shown} is not a folder")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_from_the_current_folder_and_home() {
        let current = Path::new("/home/computer/project");
        let home = Path::new("/home/computer");
        let at = |input| resolve(current, home, input);
        assert_eq!(at("src"), Path::new("/home/computer/project/src"));
        assert_eq!(at(".."), Path::new("/home/computer/project/.."));
        assert_eq!(at("/tmp"), Path::new("/tmp"));
        assert_eq!(at("~"), home);
        assert_eq!(at("~/docs"), Path::new("/home/computer/docs"));
        assert_eq!(at("~docs"), Path::new("/home/computer/project/~docs"));
    }

    #[tokio::test]
    async fn only_existing_folders_are_accepted_and_come_back_canonical() {
        let base = std::env::temp_dir().join(format!("computerd-workdir-{}", std::process::id()));
        let inner = base.join("inner");
        tokio::fs::create_dir_all(&inner).await.unwrap();
        tokio::fs::write(base.join("file"), b"x").await.unwrap();
        let home = &base;

        let found = existing_dir(&inner, home, "../inner/.").await.unwrap();
        assert_eq!(found, tokio::fs::canonicalize(&inner).await.unwrap());
        assert!(existing_dir(&base, home, "missing").await.is_err());
        let error = existing_dir(&base, home, "file").await.unwrap_err();
        assert!(error.ends_with("is not a folder"), "{error}");

        tokio::fs::remove_dir_all(&base).await.unwrap();
    }
}
