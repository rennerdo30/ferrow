use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) struct Admission {
    active: Arc<AtomicUsize>,
    limit: usize,
}
pub(crate) struct Permit(Arc<AtomicUsize>);
impl Admission {
    pub fn new(limit: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }
    pub fn acquire(&self) -> Option<Permit> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.limit).then_some(active + 1)
            })
            .ok()?;
        Some(Permit(self.active.clone()))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_connection_budget_refuses_third_and_releases_capacity() {
        let budget = Admission::new(2);
        let first = budget.acquire().unwrap();
        let second = budget.acquire().unwrap();
        assert!(budget.acquire().is_none());
        drop(first);
        assert!(budget.acquire().is_some());
        drop(second);
    }
}
