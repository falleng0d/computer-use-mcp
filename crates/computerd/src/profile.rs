//! Chromium profiles. Each screen's Chromium runs on a scratch copy of a template profile in home.
//!
//! Only the files that hold what the user would miss move between the template and a scratch
//! profile. Cookies travel through the shared jar, and caches and lock files stay behind.

use std::{
    io,
    path::{Path, PathBuf},
};

const TEMPLATE_DIR: &str = ".local/share/computer-use/chromium/template";
const LEGACY_DIR: &str = ".local/share/computer-use/chromium";
const DISCARD_TRIES: u32 = 10;
const DISCARD_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
const SCRATCH_DIR: &str = "/tmp/computer-use/chromium";

/// Files, relative to the profile folder, that carry bookmarks and saved passwords.
///
/// `Preferences` stays out. It records which extensions are installed, and the extension files
/// do not travel, so a clone would never install uBlock again.
const SHARED_FILES: [&str; 5] = [
    "Default/Bookmarks",
    "Default/Login Data",
    "Default/Login Data-journal",
    "Default/Web Data",
    "Default/Web Data-journal",
];

/// Folder of the template profile that every new browser starts from.
pub fn template_dir(home: &Path) -> PathBuf {
    home.join(TEMPLATE_DIR)
}

/// Folder the Chromium of screen `number` runs its profile in. It starts fresh every time.
pub fn scratch_dir(number: u8) -> PathBuf {
    Path::new(SCRATCH_DIR).join(format!("screen-{number}"))
}

/// Profile folder of screen `number` from before profiles were cloned from a template.
fn legacy_dir(home: &Path, number: u8) -> PathBuf {
    home.join(LEGACY_DIR).join(format!("screen-{number}"))
}

/// The shared files that exist in the profile folder `dir`, relative to it.
fn shared_files(dir: &Path) -> Vec<PathBuf> {
    SHARED_FILES
        .iter()
        .map(PathBuf::from)
        .filter(|relative| dir.join(relative).is_file())
        .collect()
}

/// Copies the shared files of `from` into `to`. Each file appears whole or not at all.
///
/// `tag` keeps the temporary names of concurrent copies apart.
fn copy_shared(from: &Path, to: &Path, tag: &str) -> io::Result<()> {
    for relative in shared_files(from) {
        let target = to.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut temporary = target.clone().into_os_string();
        temporary.push(format!(".tmp-{tag}"));
        let temporary = PathBuf::from(temporary);
        std::fs::copy(from.join(&relative), &temporary)?;
        if let Err(error) = std::fs::rename(&temporary, &target) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    }
    Ok(())
}

/// Fills a scratch profile with the template's shared files.
///
/// When home has no template yet, the template starts from the profile of screen 1 that
/// earlier versions kept in home.
pub fn clone_template(home: &Path, scratch: &Path) -> io::Result<()> {
    let template = template_dir(home);
    if !template.exists() {
        let legacy = legacy_dir(home, 1);
        if legacy.is_dir() {
            copy_shared(&legacy, &template, "seed")?;
        }
    }
    copy_shared(&template, scratch, "clone")
}

/// Copies the shared files of a cleanly closed scratch profile into the template.
/// The last browser to close wins.
pub fn save_to_template(home: &Path, scratch: &Path, number: u8) -> io::Result<()> {
    copy_shared(scratch, &template_dir(home), &format!("screen-{number}"))
}

/// Deletes a scratch profile. Chromium's helper processes may still write into it for a moment
/// after the browser exits, so a failed delete is retried.
pub fn discard(scratch: &Path) {
    for _ in 0..DISCARD_TRIES {
        match std::fs::remove_dir_all(scratch) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) => std::thread::sleep(DISCARD_WAIT),
        }
    }
    for lock in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = std::fs::remove_file(scratch.join(lock));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("computerd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(root: &Path, relative: &str) -> Option<String> {
        std::fs::read_to_string(root.join(relative)).ok()
    }

    #[test]
    fn a_closed_profile_updates_the_template_without_cookies_or_locks() {
        let (home, scratch) = (temp("home-save"), temp("scratch-save"));
        write(
            &home,
            ".local/share/computer-use/chromium/template/Default/Bookmarks",
            "old",
        );
        write(&scratch, "Default/Bookmarks", "new");
        write(&scratch, "Default/Login Data", "pw");
        write(&scratch, "Default/Cookies", "secret");
        write(&scratch, "SingletonLock", "lock");
        save_to_template(&home, &scratch, 2).unwrap();
        let template = template_dir(&home);
        assert_eq!(read(&template, "Default/Bookmarks").as_deref(), Some("new"));
        assert_eq!(read(&template, "Default/Login Data").as_deref(), Some("pw"));
        assert_eq!(read(&template, "Default/Cookies"), None);
        assert_eq!(read(&template, "SingletonLock"), None);
        let leftovers: Vec<_> = std::fs::read_dir(template.join("Default"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn a_new_profile_starts_from_the_template_and_ignores_its_cookies() {
        let (home, scratch) = (temp("home-clone"), temp("scratch-clone"));
        let template = ".local/share/computer-use/chromium/template/Default";
        write(&home, &format!("{template}/Bookmarks"), "marks");
        write(&home, &format!("{template}/Cookies"), "secret");
        clone_template(&home, &scratch).unwrap();
        assert_eq!(
            read(&scratch, "Default/Bookmarks").as_deref(),
            Some("marks")
        );
        assert_eq!(read(&scratch, "Default/Cookies"), None);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn the_template_is_seeded_once_from_the_old_screen_1_profile() {
        let (home, scratch) = (temp("home-seed"), temp("scratch-seed"));
        let legacy = ".local/share/computer-use/chromium/screen-1/Default";
        write(&home, &format!("{legacy}/Bookmarks"), "from screen 1");
        write(&home, &format!("{legacy}/Cookies"), "secret");
        let other = ".local/share/computer-use/chromium/screen-2/Default";
        write(&home, &format!("{other}/Bookmarks"), "from screen 2");
        clone_template(&home, &scratch).unwrap();
        assert_eq!(
            read(&scratch, "Default/Bookmarks").as_deref(),
            Some("from screen 1")
        );
        assert_eq!(read(&template_dir(&home), "Default/Cookies"), None);
        write(&home, &format!("{legacy}/Bookmarks"), "changed later");
        let again = temp("scratch-seed-2");
        clone_template(&home, &again).unwrap();
        assert_eq!(
            read(&again, "Default/Bookmarks").as_deref(),
            Some("from screen 1")
        );
        for dir in [&home, &scratch, &again] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_first_start_with_nothing_saved_gives_an_empty_profile() {
        let (home, scratch) = (temp("home-empty"), temp("scratch-empty"));
        clone_template(&home, &scratch).unwrap();
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
