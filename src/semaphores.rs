use crate::types::Clock;

/// Named counting semaphores whose holders keep their slot only while their lease is renewed.
pub struct Semaphores {
    /// Insertion-ordered like a JS Map: names, and holders within a name.
    held: Vec<(String, Vec<(String, i64)>)>,
    now: Clock,
}

impl Semaphores {
    pub fn new(now: Clock) -> Self {
        Semaphores { held: Vec::new(), now }
    }

    pub fn try_acquire(&mut self, name: &str, holder: &str, capacity: usize, lease_ms: i64) -> bool {
        let now = (self.now)();
        let idx = match self.held.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                self.held.push((name.to_string(), Vec::new()));
                self.held.len() - 1
            }
        };
        let h = &mut self.held[idx].1;
        let live = h.iter().filter(|(k, until)| k != holder && *until > now).count();
        if live >= capacity {
            return false;
        }
        match h.iter_mut().find(|(k, _)| k == holder) {
            Some(e) => e.1 = now + lease_ms,
            None => h.push((holder.to_string(), now + lease_ms)),
        }
        true
    }

    pub fn renew(&mut self, holder: &str, lease_ms: i64) {
        let now = (self.now)();
        for (_, h) in &mut self.held {
            for e in h.iter_mut() {
                if e.0 == holder && e.1 > now {
                    e.1 = now + lease_ms;
                }
            }
        }
    }

    pub fn release(&mut self, holder: &str) {
        for (_, h) in &mut self.held {
            h.retain(|(k, _)| k != holder);
        }
    }

    pub fn holders(&self, name: &str) -> Vec<String> {
        let now = (self.now)();
        self.held
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h.iter().filter(|(_, until)| *until > now).map(|(k, _)| k.clone()).collect())
            .unwrap_or_default()
    }

    /// Removes and returns every holder whose lease has run out.
    pub fn expire(&mut self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, h) in &mut self.held {
            let mut i = 0;
            while i < h.len() {
                if h[i].1 <= (self.now)() {
                    let (holder, _) = h.remove(i);
                    if !out.contains(&holder) {
                        out.push(holder);
                    }
                } else {
                    i += 1;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct TestClock(Arc<AtomicI64>);

    impl TestClock {
        fn new() -> Self {
            TestClock(Arc::new(AtomicI64::new(0)))
        }
        fn now(&self) -> Clock {
            let t = self.0.clone();
            Arc::new(move || t.load(Ordering::SeqCst))
        }
        fn advance(&self, ms: i64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn capacity_limits_holders_re_acquire_by_the_same_holder_is_idempotent() {
        let mut sem = Semaphores::new(TestClock::new().now());
        assert!(sem.try_acquire("workers", "w1", 2, 100));
        assert!(sem.try_acquire("workers", "w1", 2, 100));
        assert!(sem.try_acquire("workers", "w2", 2, 100));
        assert!(!sem.try_acquire("workers", "w3", 2, 100));
        assert_eq!(sem.holders("workers"), s(&["w1", "w2"]));
    }

    #[test]
    fn release_frees_the_slot_under_every_name() {
        let mut sem = Semaphores::new(TestClock::new().now());
        sem.try_acquire("workers", "w1", 1, 100);
        sem.try_acquire("merge", "w1", 1, 100);
        sem.release("w1");
        assert!(sem.holders("workers").is_empty());
        assert!(sem.holders("merge").is_empty());
    }

    #[test]
    fn expire_removes_holders_whose_lease_ran_out_renew_extends_it() {
        let c = TestClock::new();
        let mut sem = Semaphores::new(c.now());
        sem.try_acquire("workers", "w1", 4, 100);
        sem.try_acquire("workers", "w2", 4, 100);
        c.advance(60);
        sem.renew("w2", 100);
        c.advance(50);
        assert_eq!(sem.expire(), s(&["w1"]));
        assert_eq!(sem.holders("workers"), s(&["w2"]));
    }

    #[test]
    fn expired_holders_do_not_count_toward_capacity_but_are_still_reported_by_expire() {
        let c = TestClock::new();
        let mut sem = Semaphores::new(c.now());
        assert!(sem.try_acquire("workers", "w1", 1, 100));
        c.advance(100);
        assert!(sem.try_acquire("workers", "w2", 1, 100));
        assert_eq!(sem.expire(), s(&["w1"]));
        assert_eq!(sem.holders("workers"), s(&["w2"]));
    }

    #[test]
    fn renew_does_not_revive_an_expired_lease() {
        let c = TestClock::new();
        let mut sem = Semaphores::new(c.now());
        sem.try_acquire("workers", "w1", 1, 100);
        c.advance(100);
        sem.renew("w1", 100);
        assert_eq!(sem.expire(), s(&["w1"]));
    }

    #[test]
    fn holders_lists_only_live_leases() {
        let c = TestClock::new();
        let mut sem = Semaphores::new(c.now());
        sem.try_acquire("workers", "w1", 2, 100);
        c.advance(100);
        assert!(sem.holders("workers").is_empty());
    }
}
