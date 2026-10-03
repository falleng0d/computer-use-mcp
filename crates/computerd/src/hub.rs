use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::{broadcast, watch};

const SHOW_BACKLOG: usize = 16;

/// Who is watching the computer, and the channels that tell viewer pages what changed.
#[derive(Clone)]
pub struct Hub(Arc<Inner>);

struct Inner {
    host_base: u16,
    viewers: Mutex<Viewers>,
    pages: AtomicUsize,
    changed: watch::Sender<u64>,
    show: broadcast::Sender<u8>,
}

/// Viewer counts per screen, and the screens that accept no new viewers.
#[derive(Default)]
struct Viewers {
    counts: BTreeMap<u8, usize>,
    /// Screens whose session ended. Existing viewers stay until they leave.
    ended: BTreeSet<u8>,
}

impl Hub {
    /// `host_base` is the host port the viewer page is published on. Raw VNC for screen N is on `host_base + N`.
    pub fn new(host_base: u16) -> Self {
        Self(Arc::new(Inner {
            host_base,
            viewers: Mutex::default(),
            pages: AtomicUsize::new(0),
            changed: watch::channel(0).0,
            show: broadcast::channel(SHOW_BACKLOG).0,
        }))
    }

    pub fn host_base(&self) -> u16 {
        self.0.host_base
    }

    pub fn host_vnc_port(&self, screen: u8) -> u16 {
        self.0.host_base + u16::from(screen)
    }

    fn counts(&self) -> std::sync::MutexGuard<'_, Viewers> {
        self.0
            .viewers
            .lock()
            .expect("the viewer counts are only held for short updates")
    }

    /// Viewers attached to `screen`, browser or native.
    pub fn viewers(&self, screen: u8) -> usize {
        self.counts().counts.get(&screen).copied().unwrap_or(0)
    }

    /// Counts a viewer on `screen` until the guard drops.
    ///
    /// Returns `None` when the screen's session has ended.
    pub fn attach(&self, screen: u8) -> Option<ViewerGuard> {
        {
            let mut viewers = self.counts();
            if viewers.ended.contains(&screen) {
                return None;
            }
            *viewers.counts.entry(screen).or_insert(0) += 1;
        }
        self.notify();
        Some(ViewerGuard {
            hub: self.clone(),
            screen,
        })
    }

    /// Stops new viewers on `screen` because its session ended. Returns true while a viewer is attached.
    pub fn end_screen(&self, screen: u8) -> bool {
        let mut viewers = self.counts();
        viewers.ended.insert(screen);
        viewers.counts.contains_key(&screen)
    }

    /// Accepts viewers on `screen` again, once a new session owns it.
    pub fn reopen_screen(&self, screen: u8) {
        self.counts().ended.remove(&screen);
    }

    /// Counts an open viewer page until the guard drops.
    pub fn attach_page(&self) -> PageGuard {
        self.0.pages.fetch_add(1, Ordering::SeqCst);
        PageGuard(self.clone())
    }

    /// Viewer pages open now.
    pub fn pages(&self) -> usize {
        self.0.pages.load(Ordering::SeqCst)
    }

    /// Tells pages that the sessions or viewer counts changed.
    pub fn notify(&self) {
        self.0.changed.send_modify(|version| *version += 1);
    }

    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }

    /// Asks every open page to switch to `screen` and returns how many pages are open.
    pub fn show(&self, screen: u8) -> usize {
        let _ = self.0.show.send(screen);
        self.pages()
    }

    pub fn subscribe_show(&self) -> broadcast::Receiver<u8> {
        self.0.show.subscribe()
    }

    /// Waits until no viewer is attached to `screen`.
    pub async fn detached(&self, screen: u8) {
        let mut changes = self.subscribe_changes();
        while self.viewers(screen) > 0 {
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

pub struct ViewerGuard {
    hub: Hub,
    screen: u8,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        let mut viewers = self.hub.counts();
        if let Some(count) = viewers.counts.get_mut(&self.screen) {
            *count -= 1;
            if *count == 0 {
                viewers.counts.remove(&self.screen);
            }
        }
        drop(viewers);
        self.hub.notify();
    }
}

pub struct PageGuard(Hub);

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.0.0.pages.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn detached_waits_for_the_last_viewer_of_that_screen_only() {
        let hub = Hub::new(20900);
        let first = hub.attach(3).unwrap();
        let second = hub.attach(3).unwrap();
        let other = hub.attach(4).unwrap();
        assert_eq!(hub.viewers(3), 2);

        let waiting = tokio::spawn({
            let hub = hub.clone();
            async move { hub.detached(3).await }
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(first);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!waiting.is_finished());
        drop(second);
        waiting.await.unwrap();
        assert_eq!(hub.viewers(4), 1);
        drop(other);
        assert_eq!(hub.viewers(4), 0);
    }

    #[test]
    fn an_ended_screen_keeps_its_viewers_but_takes_no_new_ones_until_reopened() {
        let hub = Hub::new(20900);
        let first = hub.attach(2).unwrap();
        assert!(hub.end_screen(2));
        assert!(hub.attach(2).is_none());
        assert_eq!(hub.viewers(2), 1);
        assert!(hub.attach(3).is_some());
        drop(first);
        assert!(!hub.end_screen(2));
        assert!(hub.attach(2).is_none());
        hub.reopen_screen(2);
        assert!(hub.attach(2).is_some());
    }

    #[test]
    fn open_pages_are_counted_and_told_which_screen_to_show() {
        let hub = Hub::new(21900);
        let mut shown = hub.subscribe_show();
        assert_eq!(hub.show(2), 0);
        let page = hub.attach_page();
        assert_eq!(hub.show(5), 1);
        assert_eq!(shown.try_recv().unwrap(), 2);
        assert_eq!(shown.try_recv().unwrap(), 5);
        drop(page);
        assert_eq!(hub.pages(), 0);
        assert_eq!(hub.host_vnc_port(3), 21903);
    }
}
