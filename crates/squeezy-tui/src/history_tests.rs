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
