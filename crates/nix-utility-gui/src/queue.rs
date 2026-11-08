//! Which analyses are waiting, which one is running, and how far along the
//! queue is. Analyses run one at a time. Free of any widgets.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use nix_log_diff_analyzer::CancelToken;

use crate::analysis::Trigger;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub attr: String,
    pub trigger: Trigger,
    /// Send even though the diff is over the size limit.
    pub bypass: bool,
}

#[derive(Default)]
pub struct Queue {
    waiting: VecDeque<Item>,
    current: Option<String>,
    /// Shared with the window's close handler, which cancels it.
    token: Rc<RefCell<Option<CancelToken>>>,
    total: usize,
    done: usize,
    auto_started: u32,
    /// Sort the sidebar by verdict once the queue runs dry.
    pub sort_when_idle: bool,
    /// Tells apart the updates of a thread from before a reset.
    generation: u64,
}

impl Queue {
    pub fn token(&self) -> Rc<RefCell<Option<CancelToken>>> {
        Rc::clone(&self.token)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn active(&self) -> bool {
        self.current.is_some() || !self.waiting.is_empty()
    }

    pub fn is_current(&self, generation: u64, attr: &str) -> bool {
        generation == self.generation && self.current.as_deref() == Some(attr)
    }

    pub fn has(&self, attr: &str) -> bool {
        self.current.as_deref() == Some(attr) || self.waiting.iter().any(|i| i.attr == attr)
    }

    /// Manual requests go first: someone is waiting for the answer.
    pub fn push(&mut self, item: Item) {
        if self.has(&item.attr) {
            return;
        }
        if !self.active() {
            self.total = 0;
            self.done = 0;
        }
        self.total += 1;
        match item.trigger {
            Trigger::Manual => self.waiting.push_front(item),
            Trigger::Auto => self.waiting.push_back(item),
        }
    }

    pub fn pop(&mut self) -> Option<Item> {
        self.waiting.pop_front()
    }

    /// An item taken off the queue that was never started.
    pub fn dropped(&mut self) {
        self.total = self.total.saturating_sub(1);
    }

    pub fn begin(&mut self, attr: &str, token: CancelToken) {
        self.current = Some(attr.to_owned());
        *self.token.borrow_mut() = Some(token);
    }

    /// The current analysis is over. Returns whether the queue is now empty.
    pub fn finish(&mut self) -> bool {
        self.current = None;
        *self.token.borrow_mut() = None;
        self.done += 1;
        self.waiting.is_empty()
    }

    pub fn running(&self) -> bool {
        self.current.is_some()
    }

    /// `(done, total)` while a queue of more than one is working.
    pub fn progress(&self) -> Option<(usize, usize)> {
        (self.active() && self.total > 1).then_some((self.done, self.total))
    }

    /// Count one more automatic analysis against the cap of a run (0 for no
    /// cap). `false` when the cap is used up.
    pub fn take_auto_slot(&mut self, cap: u32) -> bool {
        if cap > 0 && self.auto_started >= cap {
            return false;
        }
        self.auto_started += 1;
        true
    }

    /// Stop the running analysis and forget the waiting ones, returning the
    /// attributes that were waiting.
    pub fn cancel_all(&mut self) -> Vec<String> {
        if let Some(token) = self.token.borrow().as_ref() {
            token.cancel();
        }
        let waiting = self.waiting.drain(..).map(|i| i.attr).collect();
        self.total = 0;
        self.done = 0;
        waiting
    }

    /// Stop `attr`, whether it is running or waiting. Returns whether it was
    /// waiting (a running one reports back when it has stopped).
    pub fn cancel_one(&mut self, attr: &str) -> bool {
        if self.current.as_deref() == Some(attr) {
            if let Some(token) = self.token.borrow().as_ref() {
                token.cancel();
            }
            return false;
        }
        let before = self.waiting.len();
        self.waiting.retain(|i| i.attr != attr);
        let removed = self.waiting.len() != before;
        if removed {
            self.dropped();
        }
        removed
    }

    /// For a new run or a loaded directory: nothing from before counts any
    /// more, and the cap starts over.
    pub fn reset(&mut self) {
        self.cancel_all();
        self.current = None;
        *self.token.borrow_mut() = None;
        self.auto_started = 0;
        self.sort_when_idle = false;
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(attr: &str, trigger: Trigger) -> Item {
        Item {
            attr: attr.into(),
            trigger,
            bypass: false,
        }
    }

    #[test]
    fn manual_requests_jump_the_queue() {
        let mut queue = Queue::default();
        queue.push(item("a", Trigger::Auto));
        queue.push(item("b", Trigger::Auto));
        queue.push(item("c", Trigger::Manual));
        let order: Vec<_> = std::iter::from_fn(|| queue.pop()).map(|i| i.attr).collect();
        assert_eq!(order, ["c", "a", "b"]);
    }

    #[test]
    fn an_attr_is_queued_once() {
        let mut queue = Queue::default();
        queue.push(item("a", Trigger::Auto));
        queue.push(item("a", Trigger::Manual));
        assert_eq!(queue.progress(), None);
        assert_eq!(queue.pop().map(|i| i.attr), Some("a".to_owned()));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn progress_counts_finished_analyses() {
        let mut queue = Queue::default();
        for attr in ["a", "b", "c"] {
            queue.push(item(attr, Trigger::Auto));
        }
        assert_eq!(queue.progress(), Some((0, 3)));
        let first = queue.pop().unwrap();
        queue.begin(&first.attr, CancelToken::new());
        assert!(!queue.finish());
        assert_eq!(queue.progress(), Some((1, 3)));
        let second = queue.pop().unwrap();
        queue.begin(&second.attr, CancelToken::new());
        queue.finish();
        let third = queue.pop().unwrap();
        queue.begin(&third.attr, CancelToken::new());
        assert!(queue.finish());
        assert!(!queue.active());
        queue.push(item("d", Trigger::Auto));
        assert_eq!(
            queue.progress(),
            None,
            "a fresh queue starts counting again"
        );
    }

    #[test]
    fn the_cap_is_per_run_and_zero_means_none() {
        let mut queue = Queue::default();
        assert!(queue.take_auto_slot(2));
        assert!(queue.take_auto_slot(2));
        assert!(!queue.take_auto_slot(2));
        queue.reset();
        assert!(queue.take_auto_slot(2));
        for _ in 0..100 {
            assert!(queue.take_auto_slot(0));
        }
    }

    #[test]
    fn cancelling_stops_the_running_one_and_returns_the_waiting() {
        let mut queue = Queue::default();
        queue.push(item("a", Trigger::Auto));
        queue.push(item("b", Trigger::Auto));
        let token = CancelToken::new();
        let first = queue.pop().unwrap();
        queue.begin(&first.attr, token.clone());
        assert_eq!(queue.cancel_all(), ["b"]);
        assert!(token.is_cancelled());
        assert!(queue.running(), "it reports back once it has stopped");
        assert!(queue.finish());
    }

    #[test]
    fn cancelling_one_waiting_item_leaves_the_rest() {
        let mut queue = Queue::default();
        queue.push(item("a", Trigger::Auto));
        queue.push(item("b", Trigger::Auto));
        assert!(queue.cancel_one("b"));
        assert!(!queue.cancel_one("missing"));
        assert_eq!(queue.pop().map(|i| i.attr), Some("a".to_owned()));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn a_reset_makes_earlier_updates_stale() {
        let mut queue = Queue::default();
        let before = queue.generation();
        queue.begin("a", CancelToken::new());
        assert!(queue.is_current(before, "a"));
        queue.reset();
        assert!(!queue.is_current(before, "a"));
        assert!(!queue.active());
    }
}
