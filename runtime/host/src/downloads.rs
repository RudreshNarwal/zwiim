//! Downloads in flight, so every client can show their progress: an agent's
//! adapter, the browser engine, a Host update (which brings `gh`). Whatever
//! downloads takes a [`Progress`] from [`Downloads::begin`] and passes it to
//! each [`crate::adapters::install::Fetch`]; the Host feed publishes the list
//! as `download.changed` / `download.removed`.
//!
//! One entry per thing the person would name, not per file: an adapter's
//! hundred packages are one "Claude Code".
//!
//! What the Host installs downloads one at a time ([`Downloads::queue`]):
//! an agent's adapter first, as a chat waits on it, then a Zwiim update
//! (which brings `gh`), then the browser engine; in the order asked within
//! each. The rest are listed as waiting, in the order they will go. A
//! download under way is never stopped for one that comes first. A file a
//! page downloads in the browser starts at once ([`Downloads::begin`]): the
//! browser starts it, not the Host.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{watch, Notify};

/// Progress is announced at most this often per download.
const NOTIFY_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Download {
    pub download_id: String,
    /// What is downloading, as the person knows it: "Claude Code",
    /// "Browser engine 157.0.8103.78", "Zwiim 0.6.0".
    pub label: String,
    /// Bytes received so far.
    pub received: u64,
    /// Bytes in all, when known.
    pub total: Option<u64>,
    /// Queued behind another download; nothing arrives until it ends.
    pub waiting: bool,
}

/// Which queued download goes first: the earliest here, then the earliest
/// asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// An agent's adapter: a chat cannot start without it.
    Adapter,
    /// A Zwiim update, which brings `gh`.
    Update,
    /// The browser engine.
    Browser,
}

#[derive(Default)]
struct Inner {
    next: u64,
    list: Vec<Download>,
    /// The queued download under way, by id.
    turn: Option<String>,
    /// Queued downloads waiting for their turn, in the order asked.
    waiters: Vec<(String, Priority)>,
}

impl Inner {
    /// The waiter that goes next: by priority, then the earliest asked.
    fn next_waiter(&self) -> Option<usize> {
        (0..self.waiters.len()).min_by_key(|&i| (self.waiters[i].1, i))
    }
}

pub struct Downloads {
    inner: Mutex<Inner>,
    /// Bumped on every announced change; the host feed watches it.
    notify: watch::Sender<u64>,
    /// Woken when the turn passes to another queued download.
    turn_passed: Notify,
}

/// The Host's downloads.
pub fn global() -> Arc<Downloads> {
    static GLOBAL: LazyLock<Arc<Downloads>> = LazyLock::new(Downloads::new);
    GLOBAL.clone()
}

impl Downloads {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::default(),
            notify: watch::Sender::new(0),
            turn_passed: Notify::new(),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.notify.subscribe()
    }

    /// Every download in flight, oldest first, except that the ones waiting
    /// come last, in the order they will go.
    pub fn list(&self) -> Vec<Download> {
        let inner = self.inner.lock().unwrap();
        let mut list = inner.list.clone();
        let place = |d: &Download| {
            let i = inner
                .waiters
                .iter()
                .position(|(id, _)| *id == d.download_id);
            i.map(|i| (inner.waiters[i].1, i))
        };
        // Stable: not waiting (None) first, as they were.
        list.sort_by_key(place);
        list
    }

    /// Start a download of `files` files named `label`. It is listed until
    /// the returned [`Progress`] is dropped. Its total is known only for a
    /// single file.
    pub fn begin(self: &Arc<Self>, label: impl Into<String>, files: usize) -> Progress {
        self.enter(label.into(), files, None)
    }

    /// Like [`Downloads::begin`], but waits its turn (see [`Priority`]),
    /// listed as waiting until then. Dropping the future while it waits
    /// takes it off the list.
    pub async fn queue(
        self: &Arc<Self>,
        label: impl Into<String>,
        files: usize,
        priority: Priority,
    ) -> Progress {
        let progress = self.enter(label.into(), files, Some(priority));
        let id = progress
            .to
            .as_ref()
            .map(|t| t.download_id.clone())
            .unwrap_or_default();
        loop {
            // Listen before looking, so a turn passed in between is not missed.
            let passed = self.turn_passed.notified();
            tokio::pin!(passed);
            passed.as_mut().enable();
            if self.inner.lock().unwrap().turn.as_deref() == Some(id.as_str()) {
                break;
            }
            passed.await;
        }
        self.update(&id, |d| d.waiting = false);
        self.changed();
        progress
    }

    /// List a download; a queued one takes the turn if it is free, else
    /// waits for it.
    fn enter(self: &Arc<Self>, label: String, files: usize, queued: Option<Priority>) -> Progress {
        let download_id = {
            let mut inner = self.inner.lock().unwrap();
            inner.next += 1;
            let download_id = format!("download-{}", inner.next);
            let waiting = match queued {
                Some(_) if inner.turn.is_none() => {
                    inner.turn = Some(download_id.clone());
                    false
                }
                Some(priority) => {
                    inner.waiters.push((download_id.clone(), priority));
                    true
                }
                None => false,
            };
            inner.list.push(Download {
                download_id: download_id.clone(),
                label,
                received: 0,
                total: None,
                waiting,
            });
            download_id
        };
        self.changed();
        Progress {
            to: Some(Tracked {
                downloads: self.clone(),
                download_id,
                single: files == 1,
                last: Mutex::new(Instant::now()),
            }),
        }
    }

    fn changed(&self) {
        self.notify.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn update(&self, download_id: &str, f: impl FnOnce(&mut Download)) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(d) = inner.list.iter_mut().find(|d| d.download_id == download_id) {
            f(d);
        }
    }
}

/// A download's progress, reported by the fetches that make it up.
pub struct Progress {
    to: Option<Tracked>,
}

struct Tracked {
    downloads: Arc<Downloads>,
    download_id: String,
    single: bool,
    last: Mutex<Instant>,
}

impl Progress {
    /// Progress nobody sees: small files (a manifest), tests.
    pub fn none() -> Progress {
        Progress { to: None }
    }

    /// A file of `size` bytes starts (`None` when the server did not say).
    pub fn file(&self, size: Option<u64>) {
        let Some(t) = &self.to else { return };
        if !t.single {
            return;
        }
        t.downloads.update(&t.download_id, |d| d.total = size);
        t.downloads.changed();
    }

    /// `bytes` more arrived.
    pub fn received(&self, bytes: u64) {
        let Some(t) = &self.to else { return };
        t.downloads.update(&t.download_id, |d| d.received += bytes);
        let mut last = t.last.lock().unwrap();
        if last.elapsed() >= NOTIFY_EVERY {
            *last = Instant::now();
            t.downloads.changed();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let Some(t) = &self.to else { return };
        let passed = {
            let mut inner = t.downloads.inner.lock().unwrap();
            inner.list.retain(|d| d.download_id != t.download_id);
            inner.waiters.retain(|(id, _)| *id != t.download_id);
            // Its turn, if it had it (even one given up just as it came),
            // goes to the next waiter.
            let had_turn = inner.turn.as_deref() == Some(t.download_id.as_str());
            if had_turn {
                inner.turn = inner.next_waiter().map(|i| inner.waiters.remove(i).0);
            }
            had_turn && inner.turn.is_some()
        };
        if passed {
            t.downloads.turn_passed.notify_waiters();
        }
        t.downloads.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_is_listed_until_it_ends() {
        let downloads = Downloads::new();
        let mut rx = downloads.subscribe();
        let one = downloads.begin("Browser engine 157", 1);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        one.file(Some(100));
        one.received(40);
        assert_eq!(
            downloads.list(),
            vec![Download {
                download_id: "download-1".into(),
                label: "Browser engine 157".into(),
                received: 40,
                total: Some(100),
                waiting: false,
            }]
        );
        drop(one);
        assert!(downloads.list().is_empty());
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn many_files_count_bytes_but_no_total() {
        let downloads = Downloads::new();
        let many = downloads.begin("Claude Code", 3);
        many.file(Some(10));
        many.received(10);
        many.file(Some(20));
        many.received(5);
        let list = downloads.list();
        assert_eq!((list[0].received, list[0].total), (15, None));
    }

    #[test]
    fn progress_is_announced_at_most_every_quarter_second() {
        let downloads = Downloads::new();
        let one = downloads.begin("Zwiim 0.6.0", 1);
        let mut rx = downloads.subscribe();
        rx.borrow_and_update();
        one.received(1);
        one.received(1);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(downloads.list()[0].received, 2);
        *one.to.as_ref().unwrap().last.lock().unwrap() -= NOTIFY_EVERY;
        one.received(1);
        assert!(rx.has_changed().unwrap());
    }

    #[tokio::test]
    async fn queued_downloads_go_adapters_then_updates_then_the_browser() {
        let downloads = Downloads::new();
        let first = downloads
            .queue("Browser engine 156", 1, Priority::Browser)
            .await;
        let ask = |label: &'static str, priority| {
            let downloads = downloads.clone();
            tokio::spawn(async move { downloads.queue(label, 1, priority).await })
        };
        let browser = ask("Browser engine 157", Priority::Browser);
        let update = ask("Zwiim 0.6.0", Priority::Update);
        let adapter = ask("Claude Code", Priority::Adapter);
        let shown = |downloads: &Downloads| -> Vec<(String, bool)> {
            downloads
                .list()
                .into_iter()
                .map(|d| (d.label, d.waiting))
                .collect()
        };
        while downloads.list().len() < 4 {
            tokio::task::yield_now().await;
        }
        // The one under way keeps going; the rest wait in the order they will go.
        assert_eq!(
            shown(&downloads),
            [
                ("Browser engine 156".into(), false),
                ("Claude Code".into(), true),
                ("Zwiim 0.6.0".into(), true),
                ("Browser engine 157".into(), true),
            ]
        );
        // A page's download is never held back.
        let page = downloads.begin("report.pdf", 1);
        assert_eq!(shown(&downloads)[1], ("report.pdf".into(), false));
        drop(page);

        drop(first);
        let adapter = adapter.await.unwrap();
        assert_eq!(shown(&downloads)[0], ("Claude Code".into(), false));
        drop(adapter);
        let update = update.await.unwrap();
        assert_eq!(shown(&downloads)[0], ("Zwiim 0.6.0".into(), false));
        drop(update);
        let browser = browser.await.unwrap();
        assert_eq!(shown(&downloads), [("Browser engine 157".into(), false)]);
        drop(browser);
        assert!(downloads.list().is_empty());
    }

    #[tokio::test]
    async fn the_same_priority_goes_in_the_order_asked() {
        let downloads = Downloads::new();
        let first = downloads.queue("Claude Code", 2, Priority::Adapter).await;
        let codex = downloads.clone();
        let codex = tokio::spawn(async move { codex.queue("Codex", 2, Priority::Adapter).await });
        while downloads.list().len() < 2 {
            tokio::task::yield_now().await;
        }
        let gemini = downloads.clone();
        let gemini =
            tokio::spawn(async move { gemini.queue("Gemini", 2, Priority::Adapter).await });
        while downloads.list().len() < 3 {
            tokio::task::yield_now().await;
        }
        drop(first);
        let codex = codex.await.unwrap();
        assert_eq!(downloads.list()[0].label, "Codex");
        drop(codex);
        drop(gemini.await.unwrap());
    }

    #[tokio::test]
    async fn a_queued_download_given_up_while_waiting_is_unlisted() {
        let downloads = Downloads::new();
        let first = downloads.queue("Claude Code", 2, Priority::Adapter).await;
        let given_up = tokio::time::timeout(
            Duration::from_millis(10),
            downloads.queue("Codex", 2, Priority::Adapter),
        )
        .await;
        assert!(given_up.is_err());
        assert_eq!(downloads.list().len(), 1);
        drop(first);
        // The turn passes on: the next one does not wait.
        let next = tokio::time::timeout(
            Duration::from_secs(1),
            downloads.queue("Codex", 2, Priority::Adapter),
        )
        .await;
        assert!(next.is_ok());
    }

    #[test]
    fn unreported_progress_lists_nothing() {
        let p = Progress::none();
        p.file(Some(1));
        p.received(1);
    }
}
