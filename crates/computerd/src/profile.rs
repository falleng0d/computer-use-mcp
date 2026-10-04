//! Chromium profiles. Each screen's Chromium runs on a scratch copy of a template profile in home.
//!
//! Only the files that hold what the user would miss move between the template and a scratch
//! profile. Cookies travel through the shared jar, and caches and lock files stay behind.

use std::{
    collections::BTreeMap,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::Value;

const TEMPLATE_DIR: &str = ".local/share/computer-use/chromium/template";
const LEGACY_DIR: &str = ".local/share/computer-use/chromium";
const SCRATCH_DIR: &str = "/tmp/computer-use/chromium";
const DISCARD_TRIES: u32 = 10;
const DISCARD_WAIT: Duration = Duration::from_millis(300);
const PREFERENCES: &str = "Default/Preferences";

/// Files, relative to the profile folder, that move between profiles. The files of a group move
/// together, and a file that is missing from the source is removed from the target.
///
/// `Preferences` moves without its extension records. They list installed extensions while the
/// extension files stay behind, and a clone that claimed an extension was installed would never
/// install it again.
const GROUPS: [&[&str]; 4] = [
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
];

/// What each group of a scratch profile held right after the clone.
pub type Baseline = BTreeMap<usize, u64>;

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
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            name.hash(&mut hasher);
            bytes.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Copies one file whole or not at all, going through a temporary name.
fn copy_file(from: &Path, to: &Path, tag: &str) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = to.to_path_buf().into_os_string();
    temporary.push(format!(".tmp-{tag}"));
    let temporary = PathBuf::from(temporary);
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

/// Copies one group from `from` to `to`. Nothing changes when `from` has none of its files.
fn copy_group(from: &Path, to: &Path, group: &[&str], tag: &str) -> io::Result<()> {
    if !group.iter().any(|name| from.join(name).is_file()) {
        return Ok(());
    }
    for name in group {
        let source = from.join(name);
        if source.is_file() {
            copy_file(&source, &to.join(name), tag)?;
        } else {
            match std::fs::remove_file(to.join(name)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Starts the template from the profile of screen 1 that earlier versions kept in home.
/// The template appears whole, or another start that got there first keeps it.
fn seed_template(home: &Path) -> io::Result<()> {
    let template = template_dir(home);
    let legacy = legacy_dir(home, 1);
    if template.exists() || !legacy.is_dir() {
        return Ok(());
    }
    let staging = template.with_file_name(unique("template.seed"));
    let copied = GROUPS
        .iter()
        .try_for_each(|group| copy_group(&legacy, &staging, group, "seed"));
    if let Err(error) = copied {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if std::fs::rename(&staging, &template).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    Ok(())
}

/// Fills a scratch profile with the template's shared files.
pub fn clone_template(home: &Path, scratch: &Path) -> io::Result<Baseline> {
    seed_template(home)?;
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
pub fn save_to_template(
    home: &Path,
    scratch: &Path,
    number: u8,
    baseline: &Baseline,
) -> io::Result<()> {
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
        let siblings: Vec<_> = std::fs::read_dir(template_dir(&home).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("template"))
            .collect();
        assert_eq!(siblings, vec!["template".to_owned()]);
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
