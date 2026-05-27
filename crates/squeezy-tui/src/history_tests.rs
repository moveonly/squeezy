use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn render_cache_only_recomputes_on_width_change() {
    let calls = AtomicUsize::new(0);
    let mut cache = RenderCache::new();

    let _ = cache.ensure(80, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("hello")]
    });
    let _ = cache.ensure(80, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("hello")]
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second call should hit cache"
    );

    let _ = cache.ensure(40, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        vec![Line::from("hi"), Line::from("ho")]
    });
    assert_eq!(calls.load(Ordering::SeqCst), 2, "width change should miss");
}

#[test]
fn render_cache_height_reuses_lines() {
    let mut cache = RenderCache::new();
    let height = cache.height(80, |_| vec![Line::from("a"), Line::from("b")]);
    assert_eq!(height, 2);
    assert!(cache.is_warm());
}

#[test]
fn render_cache_invalidate_clears_state() {
    let mut cache = RenderCache::new();
    let _ = cache.ensure(80, |_| vec![Line::from("a")]);
    cache.invalidate();
    assert!(!cache.is_warm());
}

struct StubCell {
    cache: RenderCache,
    text: &'static str,
    recomputes: AtomicUsize,
}

impl StubCell {
    fn new(text: &'static str) -> Self {
        Self {
            cache: RenderCache::new(),
            text,
            recomputes: AtomicUsize::new(0),
        }
    }
}

impl HistoryCell for StubCell {
    fn desired_height(&self, width: u16) -> u16 {
        let mut cache = self.cache.clone();
        cache.height(width, |_| {
            self.recomputes.fetch_add(1, Ordering::SeqCst);
            vec![Line::from(self.text)]
        })
    }

    fn render(&self, _width: u16) -> Vec<Line<'static>> {
        vec![Line::from(self.text)]
    }
}

#[test]
fn history_cell_trait_default_is_not_animating() {
    let cell = StubCell::new("plain");
    assert!(!cell.is_animating());
    assert_eq!(cell.desired_height(80), 1);
    assert_eq!(cell.render(80).len(), 1);
}

#[test]
fn history_cell_default_has_no_subscription() {
    let cell = StubCell::new("plain");
    assert!(cell.subscribe().is_none());
}

struct StreamingStubCell {
    text: std::sync::Mutex<String>,
    tx: tokio::sync::watch::Sender<()>,
    rx: tokio::sync::watch::Receiver<()>,
}

impl StreamingStubCell {
    fn new(text: &str) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(());
        Self {
            text: std::sync::Mutex::new(text.to_string()),
            tx,
            rx,
        }
    }

    fn push_delta(&self, more: &str) {
        self.text.lock().unwrap().push_str(more);
        let _ = self.tx.send(());
    }
}

impl HistoryCell for StreamingStubCell {
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }

    fn render(&self, _width: u16) -> Vec<Line<'static>> {
        vec![Line::from(self.text.lock().unwrap().clone())]
    }

    fn subscribe(&self) -> Option<HistoryCellUpdateStream> {
        Some(self.rx.clone())
    }
}

#[test]
fn history_cell_subscription_ticks_on_source_change() {
    let cell = StreamingStubCell::new("initial");
    let mut rx = cell.subscribe().expect("streaming cell exposes a watcher");
    rx.mark_unchanged();

    cell.push_delta(" + delta");
    assert!(
        rx.has_changed().expect("watch sender still live"),
        "subscription must signal a tick after push_delta"
    );

    let rendered = cell.render(80);
    assert_eq!(rendered.len(), 1);
    let row = rendered[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert_eq!(row, "initial + delta");
}
