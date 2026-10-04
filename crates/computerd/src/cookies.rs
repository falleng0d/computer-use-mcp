//! The merge rules of the shared cookie jar. No I/O lives here.
//!
//! Cookies are `DevTools` cookie objects, kept whole so every field survives a round trip.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Chromium caps the expiry of a cookie it stores at this many seconds from the write.
const EXPIRY_CAP_SECS: f64 = 400.0 * 86_400.0;
/// How far below the cap an expiry may sit and still count as capped.
const EXPIRY_CAP_SLACK_SECS: f64 = 2.0 * 86_400.0;
/// How long the jar remembers that a cookie was deleted.
pub const TOMBSTONE_TTL_SECS: f64 = 30.0 * 86_400.0;
const JAR_VERSION: u32 = 1;
/// Fields `Storage.setCookies` accepts. The others `getCookies` reports are derived.
const PARAM_FIELDS: [&str; 10] = [
    "name",
    "value",
    "domain",
    "path",
    "secure",
    "httpOnly",
    "sameSite",
    "priority",
    "sourceScheme",
    "sourcePort",
];
/// An expiry in the past, which makes `Storage.setCookies` delete the cookie.
const DELETE_EXPIRES: f64 = 1.0;

/// Identity of a cookie. A value change keeps the key, a new partition makes a new cookie.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    name: String,
    domain: String,
    path: String,
    partition: String,
}

impl Key {
    /// The key of a `DevTools` cookie, or `None` when it lacks a name, domain, or path.
    pub fn of(cookie: &Value) -> Option<Self> {
        let text = |field: &str| cookie.get(field)?.as_str().map(str::to_owned);
        Some(Self {
            name: text("name")?,
            domain: text("domain")?,
            path: text("path")?,
            partition: cookie
                .get("partitionKey")
                .map(Value::to_string)
                .unwrap_or_default(),
        })
    }
}

/// The cookies one browser held at its last read, by key.
pub type Snapshot = BTreeMap<Key, Value>;

/// Keeps the cookies that can move between browsers.
///
/// Cookies with an opaque partition cannot be recreated, so they stay where they are.
pub fn snapshot(cookies: Vec<Value>) -> Snapshot {
    cookies
        .into_iter()
        .filter(|cookie| cookie.get("partitionKeyOpaque").and_then(Value::as_bool) != Some(true))
        .filter_map(|cookie| Some((Key::of(&cookie)?, cookie)))
        .collect()
}

fn expires(cookie: &Value) -> f64 {
    cookie
        .get("expires")
        .and_then(Value::as_f64)
        .unwrap_or(-1.0)
}

fn is_expired(cookie: &Value, now: f64) -> bool {
    let expires = expires(cookie);
    expires > 0.0 && expires <= now
}

/// Whether two cookies are the same for syncing. Expiries that both sit at the cap are equal.
pub fn same(a: &Value, b: &Value, now: f64) -> bool {
    let (left, right) = (expires(a), expires(b));
    let cap = now + EXPIRY_CAP_SECS - EXPIRY_CAP_SLACK_SECS;
    let same_expiry = (left - right).abs() < 1.0 || (left >= cap && right >= cap);
    if !same_expiry {
        return false;
    }
    let without_expiry = |cookie: &Value| {
        let mut fields: Map<String, Value> = cookie.as_object().cloned().unwrap_or_default();
        fields.remove("expires");
        fields
    };
    without_expiry(a) == without_expiry(b)
}

/// What changed in one browser between two reads.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// Cookies that are new or have a new value.
    pub upserts: Vec<Value>,
    /// Cookies the user or a page deleted, as the browser last held them.
    pub removed: Vec<Value>,
}

impl Changes {
    pub fn len(&self) -> usize {
        self.upserts.len() + self.removed.len()
    }
}

/// Compares a browser's `now_cookies` with its `previous` snapshot.
///
/// A cookie that vanished after its expiry passed expired on its own, which is not a deletion.
pub fn diff(previous: &Snapshot, now_cookies: &Snapshot, now: f64) -> Changes {
    let mut changes = Changes::default();
    for (key, cookie) in now_cookies {
        if previous
            .get(key)
            .is_none_or(|before| !same(before, cookie, now))
        {
            changes.upserts.push(cookie.clone());
        }
    }
    for (key, before) in previous {
        if !now_cookies.contains_key(key) && !is_expired(before, now) {
            changes.removed.push(before.clone());
        }
    }
    changes
}

/// The `Storage.setCookies` parameter that writes `cookie`.
pub fn to_param(cookie: &Value) -> Value {
    let mut param = Map::new();
    for field in PARAM_FIELDS {
        if let Some(value) = cookie.get(field) {
            param.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(partition) = cookie.get("partitionKey") {
        param.insert("partitionKey".to_owned(), partition.clone());
    }
    let expires = expires(cookie);
    if expires > 0.0 {
        param.insert("expires".to_owned(), expires.into());
    }
    Value::Object(param)
}

/// The `Storage.setCookies` parameter that deletes `cookie`.
pub fn to_delete_param(cookie: &Value) -> Value {
    let mut param = to_param(cookie);
    if let Some(fields) = param.as_object_mut() {
        fields.insert("expires".to_owned(), DELETE_EXPIRES.into());
    }
    param
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Entry {
    cookie: Value,
    changed_at: f64,
    deleted: bool,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    entries: Vec<Entry>,
}

/// What a browser must receive to match the jar.
#[derive(Debug, Default, PartialEq)]
pub struct Pending {
    pub set: Vec<Value>,
    pub delete: Vec<Value>,
}

impl Pending {
    pub fn len(&self) -> usize {
        self.set.len() + self.delete.len()
    }
}

/// Every cookie the browsers share, with a mark for each deletion.
#[derive(Debug, Default)]
pub struct Jar {
    entries: BTreeMap<Key, Entry>,
    dirty: bool,
}

impl Jar {
    /// Parses the saved jar.
    ///
    /// # Errors
    ///
    /// Fails when the text is not a jar of a known version.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let file: File = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if file.version != JAR_VERSION {
            return Err(format!("unknown cookie jar version {}", file.version));
        }
        let entries = file
            .entries
            .into_iter()
            .filter_map(|entry| Some((Key::of(&entry.cookie)?, entry)))
            .collect();
        Ok(Self {
            entries,
            dirty: false,
        })
    }

    /// The jar as the text saved in home.
    pub fn to_json(&self) -> String {
        let file = File {
            version: JAR_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        serde_json::to_string(&file).expect("a jar is plain JSON data")
    }

    /// Whether something changed since the last [`Jar::mark_saved`].
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Folds the changes one browser made at time `at` into the jar. The newest change wins.
    pub fn apply(&mut self, changes: &Changes, at: f64) {
        for cookie in &changes.upserts {
            self.put(cookie, at, false);
        }
        for cookie in &changes.removed {
            self.put(cookie, at, true);
        }
    }

    fn put(&mut self, cookie: &Value, at: f64, deleted: bool) {
        let Some(key) = Key::of(cookie) else { return };
        if let Some(entry) = self.entries.get(&key) {
            if entry.changed_at > at {
                return;
            }
            if entry.deleted == deleted && same(&entry.cookie, cookie, at) {
                return;
            }
        }
        self.entries.insert(
            key,
            Entry {
                cookie: cookie.clone(),
                changed_at: at,
                deleted,
            },
        );
        self.dirty = true;
    }

    /// Drops cookies that expired and deletion marks that are old enough to forget.
    pub fn prune(&mut self, now: f64) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            if entry.deleted {
                now - entry.changed_at < TOMBSTONE_TTL_SECS
            } else {
                !is_expired(&entry.cookie, now)
            }
        });
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// What a browser holding `snapshot` needs to match the jar.
    pub fn pending(&self, snapshot: &Snapshot, now: f64) -> Pending {
        let mut pending = Pending::default();
        for (key, entry) in &self.entries {
            let held = snapshot.get(key);
            if entry.deleted {
                if held.is_some() {
                    pending.delete.push(entry.cookie.clone());
                }
            } else if !is_expired(&entry.cookie, now)
                && held.is_none_or(|held| !same(held, &entry.cookie, now))
            {
                pending.set.push(entry.cookie.clone());
            }
        }
        pending
    }

    /// The cookies a new browser starts with.
    pub fn live(&self, now: f64) -> Vec<Value> {
        self.entries
            .values()
            .filter(|entry| !entry.deleted && !is_expired(&entry.cookie, now))
            .map(|entry| entry.cookie.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: f64 = 1_800_000_000.0;
    const DAY: f64 = 86_400.0;

    fn cookie(name: &str, value: &str, expires: f64) -> Value {
        json!({"name": name, "value": value, "domain": "a.test", "path": "/",
               "expires": expires, "session": expires < 0.0, "httpOnly": true,
               "secure": true, "size": 4})
    }

    fn snap(cookies: Vec<Value>) -> Snapshot {
        snapshot(cookies)
    }

    #[test]
    fn cookies_in_different_partitions_are_separate_cookies() {
        let mut partitioned = cookie("id", "1", -1.0);
        partitioned["partitionKey"] =
            json!({"topLevelSite": "https://top.test", "hasCrossSiteAncestor": false});
        let all = snap(vec![cookie("id", "1", -1.0), partitioned.clone()]);
        assert_eq!(all.len(), 2);
        let only_plain = snap(vec![cookie("id", "1", -1.0)]);
        let changes = diff(&only_plain, &all, NOW);
        assert_eq!(changes.upserts, vec![partitioned]);
        assert_eq!(changes.removed, Vec::<Value>::new());
    }

    #[test]
    fn opaque_partitions_stay_out_of_the_snapshot() {
        let mut opaque = cookie("id", "1", -1.0);
        opaque["partitionKeyOpaque"] = json!(true);
        assert_eq!(snap(vec![opaque]), Snapshot::new());
    }

    #[test]
    fn a_new_value_is_an_upsert_and_a_missing_cookie_is_a_removal() {
        let before = snap(vec![
            cookie("a", "1", NOW + DAY),
            cookie("b", "1", NOW + DAY),
        ]);
        let after = snap(vec![cookie("a", "2", NOW + DAY)]);
        let changes = diff(&before, &after, NOW);
        assert_eq!(changes.upserts, vec![cookie("a", "2", NOW + DAY)]);
        assert_eq!(changes.removed, vec![cookie("b", "1", NOW + DAY)]);
    }

    #[test]
    fn a_cookie_that_expired_is_not_a_deletion_but_a_live_one_that_vanished_is() {
        let before = snap(vec![
            cookie("old", "1", NOW - 5.0),
            cookie("live", "1", NOW + 5.0),
        ]);
        let changes = diff(&before, &Snapshot::new(), NOW);
        assert_eq!(changes.removed, vec![cookie("live", "1", NOW + 5.0)]);
    }

    #[test]
    fn a_session_cookie_that_vanished_is_a_deletion() {
        let before = snap(vec![cookie("sid", "1", -1.0)]);
        assert_eq!(diff(&before, &Snapshot::new(), NOW).removed.len(), 1);
    }

    #[test]
    fn an_expiry_capped_by_chromium_is_not_a_change() {
        let asked = NOW + 900.0 * DAY;
        let capped = NOW + 399.0 * DAY;
        let before = snap(vec![cookie("a", "1", asked)]);
        let after = snap(vec![cookie("a", "1", capped)]);
        assert_eq!(diff(&before, &after, NOW), Changes::default());
        let shorter = snap(vec![cookie("a", "1", NOW + 30.0 * DAY)]);
        assert_eq!(diff(&before, &shorter, NOW).upserts.len(), 1);
    }

    #[test]
    fn a_different_value_with_the_same_expiry_is_a_change() {
        assert!(!same(
            &cookie("a", "1", NOW + DAY),
            &cookie("a", "2", NOW + DAY),
            NOW
        ));
        assert!(!same(
            &cookie("a", "1", -1.0),
            &cookie("a", "1", NOW + DAY),
            NOW
        ));
    }

    #[test]
    fn the_newest_change_wins_whatever_order_it_arrives_in() {
        let mut jar = Jar::default();
        let set = Changes {
            upserts: vec![cookie("a", "new", NOW + DAY)],
            removed: vec![],
        };
        let delete = Changes {
            upserts: vec![],
            removed: vec![cookie("a", "old", NOW + DAY)],
        };
        jar.apply(&set, NOW + 10.0);
        jar.apply(&delete, NOW + 5.0);
        assert_eq!(jar.live(NOW), vec![cookie("a", "new", NOW + DAY)]);
        jar.apply(&delete, NOW + 20.0);
        assert_eq!(jar.live(NOW), Vec::<Value>::new());
        jar.apply(&set, NOW + 15.0);
        assert_eq!(jar.live(NOW), Vec::<Value>::new());
        jar.apply(&set, NOW + 30.0);
        assert_eq!(jar.live(NOW).len(), 1);
    }

    #[test]
    fn a_deletion_is_pushed_only_to_browsers_that_still_hold_the_cookie() {
        let mut jar = Jar::default();
        jar.apply(
            &Changes {
                upserts: vec![],
                removed: vec![cookie("a", "1", NOW + DAY)],
            },
            NOW,
        );
        let holding = snap(vec![cookie("a", "1", NOW + DAY)]);
        assert_eq!(jar.pending(&holding, NOW).delete.len(), 1);
        assert_eq!(jar.pending(&Snapshot::new(), NOW), Pending::default());
    }

    #[test]
    fn a_browser_receives_only_the_cookies_it_lacks_or_holds_differently() {
        let mut jar = Jar::default();
        jar.apply(
            &Changes {
                upserts: vec![
                    cookie("same", "1", NOW + DAY),
                    cookie("newer", "2", NOW + DAY),
                    cookie("missing", "1", NOW + DAY),
                    cookie("gone", "1", NOW - DAY),
                ],
                removed: vec![],
            },
            NOW,
        );
        let held = snap(vec![
            cookie("same", "1", NOW + DAY),
            cookie("newer", "1", NOW + DAY),
        ]);
        let pending = jar.pending(&held, NOW);
        let names: Vec<_> = pending.set.iter().map(|c| c["name"].clone()).collect();
        assert_eq!(names, vec![json!("missing"), json!("newer")]);
        assert_eq!(pending.delete, Vec::<Value>::new());
    }

    #[test]
    fn pruning_forgets_old_deletions_and_expired_cookies_but_not_recent_ones() {
        let mut jar = Jar::default();
        jar.apply(
            &Changes {
                upserts: vec![
                    cookie("short", "1", NOW + 10.0),
                    cookie("long", "1", NOW + 100.0 * DAY),
                ],
                removed: vec![cookie("old", "1", NOW + DAY)],
            },
            NOW,
        );
        jar.apply(
            &Changes {
                upserts: vec![],
                removed: vec![cookie("recent", "1", NOW + DAY)],
            },
            NOW + 29.0 * DAY,
        );
        jar.prune(NOW + 31.0 * DAY);
        let tombstones = jar.entries.values().filter(|entry| entry.deleted).count();
        assert_eq!(tombstones, 1);
        let live: Vec<_> = jar
            .live(NOW + 31.0 * DAY)
            .iter()
            .map(|c| c["name"].clone())
            .collect();
        assert_eq!(live, vec![json!("long")]);
        assert_eq!(jar.entries.len(), 2);
    }

    #[test]
    fn the_jar_round_trips_through_its_file_with_deletions() {
        let mut jar = Jar::default();
        let mut partitioned = cookie("p", "1", -1.0);
        partitioned["partitionKey"] =
            json!({"topLevelSite": "https://top.test", "hasCrossSiteAncestor": true});
        jar.apply(
            &Changes {
                upserts: vec![cookie("a", "1", NOW + DAY), partitioned.clone()],
                removed: vec![cookie("b", "1", NOW + DAY)],
            },
            NOW,
        );
        let loaded = Jar::from_json(&jar.to_json()).unwrap();
        assert_eq!(loaded.entries, jar.entries);
        assert!(!loaded.is_dirty());
        assert!(loaded.live(NOW).contains(&partitioned));
        assert!(Jar::from_json("{\"version\": 9, \"entries\": []}").is_err());
        assert!(Jar::from_json("nonsense").is_err());
    }

    #[test]
    fn parameters_carry_what_setcookies_accepts_and_deletions_expire_in_the_past() {
        let mut full = cookie("a", "1", NOW + DAY);
        full["sameSite"] = json!("Strict");
        full["partitionKey"] =
            json!({"topLevelSite": "https://t.test", "hasCrossSiteAncestor": false});
        let param = to_param(&full);
        assert_eq!(
            param,
            json!({"name": "a", "value": "1", "domain": "a.test", "path": "/",
                   "secure": true, "httpOnly": true, "sameSite": "Strict",
                   "expires": NOW + DAY,
                   "partitionKey": {"topLevelSite": "https://t.test", "hasCrossSiteAncestor": false}})
        );
        let session = to_param(&cookie("s", "1", -1.0));
        assert!(session.get("expires").is_none() && session.get("session").is_none());
        assert_eq!(to_delete_param(&full)["expires"], json!(1.0));
        assert_eq!(to_delete_param(&full)["value"], json!("1"));
    }
}
