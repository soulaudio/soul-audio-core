//! Storage abstraction for the Up-Next queue and history: bounded `heapless`
//! on the DAP, growable `alloc::VecDeque` on the desktop (feature `alloc`).

/// The bounded buffer was full (only returned by the bounded `heapless` backing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full;

/// A double-ended queue the [`TwoLevelQueue`](crate::TwoLevelQueue) uses for its
/// Up-Next list and `prev` history.
pub trait QueueBuf<T> {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// `None` = unbounded (adds never fail).
    fn capacity(&self) -> Option<usize>;
    /// Append; `Err(Full)` only when bounded and full.
    fn push_back(&mut self, v: T) -> Result<(), Full>;
    /// Prepend; `Err(Full)` only when bounded and full.
    fn push_front(&mut self, v: T) -> Result<(), Full>;
    fn pop_front(&mut self) -> Option<T>;
    fn pop_back(&mut self) -> Option<T>;
    /// Append, dropping the OLDEST (front) element first if full — for a history
    /// ring that never rejects.
    fn push_back_evicting(&mut self, v: T);
    fn get(&self, i: usize) -> Option<&T>;
    fn clear(&mut self);
}

impl<T, const N: usize> QueueBuf<T> for heapless::Deque<T, N> {
    fn len(&self) -> usize {
        heapless::Deque::len(self)
    }
    fn capacity(&self) -> Option<usize> {
        Some(N)
    }
    fn push_back(&mut self, v: T) -> Result<(), Full> {
        heapless::Deque::push_back(self, v).map_err(|_| Full)
    }
    fn push_front(&mut self, v: T) -> Result<(), Full> {
        heapless::Deque::push_front(self, v).map_err(|_| Full)
    }
    fn pop_front(&mut self) -> Option<T> {
        heapless::Deque::pop_front(self)
    }
    fn pop_back(&mut self) -> Option<T> {
        heapless::Deque::pop_back(self)
    }
    fn push_back_evicting(&mut self, v: T) {
        if self.len() == N {
            let _ = heapless::Deque::pop_front(self);
        }
        // Cannot be full now.
        let _ = heapless::Deque::push_back(self, v);
    }
    fn get(&self, i: usize) -> Option<&T> {
        self.iter().nth(i)
    }
    fn clear(&mut self) {
        heapless::Deque::clear(self);
    }
}

#[cfg(feature = "alloc")]
impl<T> QueueBuf<T> for alloc::collections::VecDeque<T> {
    fn len(&self) -> usize {
        alloc::collections::VecDeque::len(self)
    }
    fn capacity(&self) -> Option<usize> {
        None
    }
    fn push_back(&mut self, v: T) -> Result<(), Full> {
        alloc::collections::VecDeque::push_back(self, v);
        Ok(())
    }
    fn push_front(&mut self, v: T) -> Result<(), Full> {
        alloc::collections::VecDeque::push_front(self, v);
        Ok(())
    }
    fn pop_front(&mut self) -> Option<T> {
        alloc::collections::VecDeque::pop_front(self)
    }
    fn pop_back(&mut self) -> Option<T> {
        alloc::collections::VecDeque::pop_back(self)
    }
    fn push_back_evicting(&mut self, v: T) {
        alloc::collections::VecDeque::push_back(self, v);
    }
    fn get(&self, i: usize) -> Option<&T> {
        alloc::collections::VecDeque::get(self, i)
    }
    fn clear(&mut self) {
        alloc::collections::VecDeque::clear(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heapless_push_front_back_and_full() {
        let mut q: heapless::Deque<u32, 2> = heapless::Deque::new();
        assert_eq!(QueueBuf::capacity(&q), Some(2));
        assert!(QueueBuf::push_back(&mut q, 1).is_ok());
        assert!(QueueBuf::push_front(&mut q, 0).is_ok()); // [0,1]
        assert_eq!(QueueBuf::push_back(&mut q, 2), Err(Full));
        assert_eq!(QueueBuf::get(&q, 0), Some(&0));
        assert_eq!(QueueBuf::get(&q, 1), Some(&1));
        assert_eq!(QueueBuf::pop_front(&mut q), Some(0));
    }

    #[test]
    fn heapless_evicting_ring_drops_oldest() {
        let mut q: heapless::Deque<u32, 2> = heapless::Deque::new();
        q.push_back_evicting(1);
        q.push_back_evicting(2);
        q.push_back_evicting(3); // drops 1
        assert_eq!(QueueBuf::len(&q), 2);
        assert_eq!(QueueBuf::get(&q, 0), Some(&2));
        assert_eq!(QueueBuf::pop_back(&mut q), Some(3));
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn alloc_is_unbounded() {
        extern crate alloc;
        let mut q: alloc::collections::VecDeque<u32> = alloc::collections::VecDeque::new();
        assert_eq!(QueueBuf::capacity(&q), None);
        for i in 0..1000 {
            assert!(QueueBuf::push_back(&mut q, i).is_ok());
        }
        assert_eq!(QueueBuf::len(&q), 1000);
    }
}
