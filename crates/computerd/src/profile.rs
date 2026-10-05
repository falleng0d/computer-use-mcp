//! Chromium profiles. Each screen's Chromium runs on a scratch copy of a template profile in home.
//!
//! Only the files that hold what the user would miss move between the template and a scratch
//! profile. Cookies travel through the shared jar, and caches and lock files stay behind.

use std::{
    collections::BTreeMap,
    fs::{DirEntry, FileType},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;

const TEMPLATE_DIR: &str = ".local/share/computer-use/chromium/template";
const SCRATCH_DIR: &str = "/tmp/computer-use/chromium";
const DISCARD_TRIES: u32 = 10;
const DISCARD_WAIT: Duration = Duration::from_millis(300);
const PREFERENCES: &str = "Default/Preferences";
const LOCK: &str = "LOCK";

/// Entries, relative to the profile folder, that move between profiles. The entries of a group
/// move together, and an entry that is missing from the source is removed from the target.
///
/// `Preferences` moves without its extension records. They list installed extensions while the
/// extension files stay behind, and a clone that claimed an extension was installed would never
/// install it again.
///
/// A folder moves with everything in it except `LOCK` files and symlinks. The 1Password
/// entries carry the extension's saved account and encrypted vault cache, so a new screen opens
/// at the unlock prompt.
const GROUPS: [&[&str]; 5] = [
    &["Default/Bookmarks"],
    &[PREFERENCES],
    &[
        "Default/Login Data",
        "Default/Login Data-journal",
        "Default/Login Data-wal",
        "Default/Login Data-shm",
    ],
    &[
        "Default/Web Data",
        "Default/Web Data-journal",
        "Default/Web Data-wal",
        "Default/Web Data-shm",
    ],
    &[
        "Default/Local Extension Settings/aeblfdkhhhdcdjpifhhbdiojplfjncoa",
        "Default/IndexedDB/chrome-extension_aeblfdkhhhdcdjpifhhbdiojplfjncoa_0.indexeddb.leveldb",
        "Default/IndexedDB/chrome-extension_aeblfdkhhhdcdjpifhhbdiojplfjncoa_0.indexeddb.blob",
    ],
];

/// What each group of a scratch profile held right after the clone.
pub(crate) type Baseline = BTreeMap<usize, u64>;

/// Folder of the template profile that every new browser starts from.
pub(crate) fn template_dir(home: &Path) -> PathBuf {
    home.join(TEMPLATE_DIR)
}

/// Folder the Chromium of screen `number` runs its profile in. It starts fresh every time.
pub(crate) fn scratch_dir(number: u8) -> PathBuf {
    Path::new(SCRATCH_DIR).join(format!("screen-{number}"))
}

/// Removes the keys of `Preferences` that name installed extensions.
///
/// Returns `None` when the text is not a JSON object.
fn strip_preferences(text: &[u8]) -> Option<Vec<u8>> {
    let mut preferences: Value = serde_json::from_slice(text).ok()?;
    let fields = preferences.as_object_mut()?;
    fields.remove("extensions");
    if let Some(Value::Object(protection)) = fields.get_mut("protection") {
        remove_key(protection, "extensions");
    }
    serde_json::to_vec(&preferences).ok()
}

fn remove_key(fields: &mut serde_json::Map<String, Value>, key: &str) {
    fields.remove(key);
    for value in fields.values_mut() {
        if let Value::Object(inner) = value {
            remove_key(inner, key);
        }
    }
}

fn fingerprint(dir: &Path, group: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for name in group {
        let path = dir.join(name);
        if let Some(kind) = kind(&path) {
            hash_entry(&path, kind, Path::new(name), &mut hasher);
        }
    }
    hasher.finish()
}

fn hash_entry(path: &Path, kind: FileType, label: &Path, hasher: &mut DefaultHasher) {
    if kind.is_dir() {
        for entry in children(path).unwrap_or_default() {
            if let Ok(kind) = entry.file_type() {
                hash_entry(&entry.path(), kind, &label.join(entry.file_name()), hasher);
            }
        }
    } else if kind.is_file()
        && let Ok(bytes) = std::fs::read(path)
    {
        label.hash(hasher);
        bytes.hash(hasher);
    }
}

/// What `path` itself is, without following a symlink.
fn kind(path: &Path) -> Option<FileType> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|metadata| metadata.file_type())
}

/// A folder's entries in name order, without `LOCK` files.
fn children(dir: &Path) -> io::Result<Vec<DirEntry>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() != LOCK {
            found.push(entry);
        }
    }
    found.sort_by_key(DirEntry::file_name);
    Ok(found)
}

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.to_path_buf().into_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Copies one file whole or not at all, going through a temporary name.
fn copy_file(from: &Path, to: &Path, tag: &str) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = with_suffix(to, &format!(".tmp-{tag}"));
    if from.ends_with(PREFERENCES) {
        let Some(stripped) = strip_preferences(&std::fs::read(from)?) else {
            return Ok(());
        };
        std::fs::write(&temporary, stripped)?;
    } else {
        std::fs::copy(from, &temporary)?;
    }
    if let Err(error) = std::fs::rename(&temporary, to) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Copies a folder's files and subfolders. Symlinks stay behind, so a link can't loop or pull in
/// files from outside the profile.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in children(from)? {
        let (kind, target) = (entry.file_type()?, to.join(entry.file_name()));
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Replaces the folder `to` with a copy of `from`. The old folder is moved aside first, so a
/// reader sees the old folder, no folder for a moment, or the new one, never a half-copied one.
fn copy_dir(from: &Path, to: &Path, tag: &str) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (staging, old) = (
        with_suffix(to, &format!(".tmp-{tag}")),
        with_suffix(to, &format!(".old-{tag}")),
    );
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&old);
    if let Err(error) = copy_tree(from, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let moved_aside = kind(to).is_some();
    if moved_aside && let Err(error) = std::fs::rename(to, &old) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, to) {
        if moved_aside {
            let _ = std::fs::rename(&old, to);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let removed = match kind(path) {
        Some(kind) if kind.is_dir() => std::fs::remove_dir_all(path),
        Some(_) => std::fs::remove_file(path),
        None => return Ok(()),
    };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Copies one group from `from` to `to`. Nothing changes when `from` has none of its entries.
fn copy_group(from: &Path, to: &Path, group: &[&str], tag: &str) -> io::Result<()> {
    if group.iter().all(|name| kind(&from.join(name)).is_none()) {
        return Ok(());
    }
    for name in group {
        let (source, target) = (from.join(name), to.join(name));
        match kind(&source) {
            Some(kind) if kind.is_dir() => copy_dir(&source, &target, tag)?,
            Some(kind) if kind.is_file() => copy_file(&source, &target, tag)?,
            _ => remove_entry(&target)?,
        }
    }
    Ok(())
}

/// Held while reading or writing the template, so a clone never copies a folder halfway through
/// another screen's save.
fn template_lock() -> MutexGuard<'static, ()> {
    static TEMPLATE: Mutex<()> = Mutex::new(());
    TEMPLATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Creates an empty template when home has none. The template appears whole, or another start
/// that got there first keeps its own.
fn ensure_template(home: &Path) -> io::Result<()> {
    let template = template_dir(home);
    if template.exists() {
        return Ok(());
    }
    let staging = template.with_file_name(unique("template.new"));
    std::fs::create_dir_all(&staging)?;
    if std::fs::rename(&staging, &template).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    Ok(())
}

/// Fills a scratch profile with the template's shared files.
pub(crate) fn clone_template(home: &Path, scratch: &Path) -> io::Result<Baseline> {
    let _template = template_lock();
    ensure_template(home)?;
    let template = template_dir(home);
    let mut baseline = Baseline::new();
    for (index, group) in GROUPS.iter().enumerate() {
        copy_group(&template, scratch, group, "clone")?;
        baseline.insert(index, fingerprint(scratch, group));
    }
    Ok(baseline)
}

/// Copies what a cleanly closed scratch profile changed since its clone into the template.
///
/// A file the browser did not change stays as the template has it, so a browser that was open
/// for a long time does not undo a newer change. A file that two browsers both changed goes to
/// whichever closes last.
pub(crate) fn save_to_template(
    home: &Path,
    scratch: &Path,
    number: u8,
    baseline: &Baseline,
) -> io::Result<()> {
    let _template = template_lock();
    let template = template_dir(home);
    for (index, group) in GROUPS.iter().enumerate() {
        if baseline.get(&index) != Some(&fingerprint(scratch, group)) {
            copy_group(scratch, &template, group, &format!("screen-{number}"))?;
        }
    }
    Ok(())
}

/// Deletes a scratch profile. Chromium's helper processes may still write into it for a moment
/// after the browser exits, so a failed delete is retried.
pub(crate) fn discard(scratch: &Path) {
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

    const TEMPLATE: &str = ".local/share/computer-use/chromium/template";

    #[test]
    fn a_closed_profile_updates_the_template_without_cookies_or_locks() {
        let (home, scratch) = (temp("home-save"), temp("scratch-save"));
        let template = template_dir(&home);
        write(&home, &format!("{TEMPLATE}/Default/Bookmarks"), "old");
        let baseline = clone_template(&home, &scratch).unwrap();
        write(&scratch, "Default/Bookmarks", "new");
        write(&scratch, "Default/Login Data", "pw");
        write(&scratch, "Default/Cookies", "secret");
        write(&scratch, "SingletonLock", "lock");
        save_to_template(&home, &scratch, 2, &baseline).unwrap();
        assert_eq!(read(&template, "Default/Bookmarks").as_deref(), Some("new"));
        assert_eq!(read(&template, "Default/Login Data").as_deref(), Some("pw"));
        assert_eq!(read(&template, "Default/Cookies"), None);
        assert_eq!(read(&template, "SingletonLock"), None);
        let leftovers: Vec<_> = std::fs::read_dir(template.join("Default"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert_eq!(leftovers, Vec::<std::ffi::OsString>::new());
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn a_browser_that_changed_nothing_does_not_undo_a_newer_change() {
        let (home, early, late) = (temp("home-stale"), temp("scratch-a"), temp("scratch-b"));
        let template = template_dir(&home);
        write(&home, &format!("{TEMPLATE}/Default/Bookmarks"), "v1");
        write(&home, &format!("{TEMPLATE}/Default/Login Data"), "pw1");
        let before_early = clone_template(&home, &early).unwrap();
        let before_late = clone_template(&home, &late).unwrap();
        write(&late, "Default/Login Data", "pw2");
        save_to_template(&home, &late, 2, &before_late).unwrap();
        write(&early, "Default/Bookmarks", "v2");
        save_to_template(&home, &early, 1, &before_early).unwrap();
        assert_eq!(
            read(&template, "Default/Login Data").as_deref(),
            Some("pw2")
        );
        assert_eq!(read(&template, "Default/Bookmarks").as_deref(), Some("v2"));
        for dir in [&home, &early, &late] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_new_profile_starts_from_the_template_and_ignores_its_cookies() {
        let (home, scratch) = (temp("home-clone"), temp("scratch-clone"));
        write(&home, &format!("{TEMPLATE}/Default/Bookmarks"), "marks");
        write(&home, &format!("{TEMPLATE}/Default/Cookies"), "secret");
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
    fn a_database_without_a_journal_removes_the_targets_stale_journal() {
        let (home, scratch) = (temp("home-journal"), temp("scratch-journal"));
        let template = template_dir(&home);
        write(&home, &format!("{TEMPLATE}/Default/Login Data"), "old");
        write(
            &home,
            &format!("{TEMPLATE}/Default/Login Data-journal"),
            "stale",
        );
        let baseline = Baseline::new();
        write(&scratch, "Default/Login Data", "new");
        save_to_template(&home, &scratch, 1, &baseline).unwrap();
        assert_eq!(
            read(&template, "Default/Login Data").as_deref(),
            Some("new")
        );
        assert_eq!(read(&template, "Default/Login Data-journal"), None);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn preferences_travel_without_their_extension_records() {
        let (home, scratch) = (temp("home-prefs"), temp("scratch-prefs"));
        let prefs = r#"{"extensions":{"settings":{"x":1},"pinned_extensions":["x"]},
            "protection":{"macs":{"extensions":{"settings":"m"},"homepage":"h"}},
            "homepage":"https://start.test/","partition":{"default_zoom_level":{"x":2.0}}}"#;
        write(&home, &format!("{TEMPLATE}/Default/Preferences"), prefs);
        clone_template(&home, &scratch).unwrap();
        let cloned: Value =
            serde_json::from_str(&read(&scratch, "Default/Preferences").unwrap()).unwrap();
        assert_eq!(
            cloned,
            serde_json::json!({
                "protection": {"macs": {"homepage": "h"}},
                "homepage": "https://start.test/",
                "partition": {"default_zoom_level": {"x": 2.0}}
            })
        );
        write(&scratch, "Default/Preferences", "not json");
        let baseline = Baseline::new();
        save_to_template(&home, &scratch, 1, &baseline).unwrap();
        assert!(
            read(&template_dir(&home), "Default/Preferences")
                .unwrap()
                .contains("start.test")
        );
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn extension_folders_travel_whole_without_lock_files_and_replace_the_old_folder() {
        const VAULT: &str = "Default/Local Extension Settings/aeblfdkhhhdcdjpifhhbdiojplfjncoa";
        let (home, scratch) = (temp("home-dir"), temp("scratch-dir"));
        let template = template_dir(&home);
        write(&home, &format!("{TEMPLATE}/{VAULT}/000003.log"), "account");
        write(&home, &format!("{TEMPLATE}/{VAULT}/LOCK"), "");
        let baseline = clone_template(&home, &scratch).unwrap();
        assert_eq!(
            read(&scratch, &format!("{VAULT}/000003.log")).as_deref(),
            Some("account")
        );
        assert_eq!(read(&scratch, &format!("{VAULT}/LOCK")), None);
        std::fs::remove_file(scratch.join(VAULT).join("000003.log")).unwrap();
        write(&scratch, &format!("{VAULT}/000005.ldb"), "vault");
        save_to_template(&home, &scratch, 1, &baseline).unwrap();
        let saved: Vec<_> = std::fs::read_dir(template.join(VAULT))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(saved, vec!["000005.ldb".to_owned()]);
        let beside: Vec<_> = std::fs::read_dir(template.join(VAULT).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(beside, vec!["aeblfdkhhhdcdjpifhhbdiojplfjncoa".to_owned()]);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_an_extension_folder_stay_behind() {
        const VAULT: &str = "Default/Local Extension Settings/aeblfdkhhhdcdjpifhhbdiojplfjncoa";
        let (home, scratch, outside) = (temp("home-link"), temp("scratch-link"), temp("outside"));
        write(&outside, "secret", "outside");
        let baseline = clone_template(&home, &scratch).unwrap();
        write(&scratch, &format!("{VAULT}/000003.log"), "vault");
        std::os::unix::fs::symlink("..", scratch.join(VAULT).join("loop")).unwrap();
        std::os::unix::fs::symlink(&outside, scratch.join(VAULT).join("outside")).unwrap();
        save_to_template(&home, &scratch, 1, &baseline).unwrap();
        let saved: Vec<_> = std::fs::read_dir(template_dir(&home).join(VAULT))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(saved, vec!["000003.log".to_owned()]);
        for dir in [&home, &scratch, &outside] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_first_start_with_nothing_saved_creates_an_empty_template_and_profile() {
        let (home, scratch) = (temp("home-empty"), temp("scratch-empty"));
        clone_template(&home, &scratch).unwrap();
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        let siblings: Vec<_> = std::fs::read_dir(template_dir(&home).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(siblings, vec!["template".to_owned()]);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
