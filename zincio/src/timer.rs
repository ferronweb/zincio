use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::BinaryHeap,
    task::Waker,
    time::{Duration, Instant},
};

use slab::Slab;

/// Number of levels in the hierarchical timing wheel
const NUM_LEVELS: usize = 7;
/// Number of slots per level
const SLOTS_PER_LEVEL: usize = 64;

/// Unique identifier for a timer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerHandle {
    slab_index: usize,
    generation: u64,
}

/// Entry stored in the slab for a timer
struct TimerEntry {
    waker: Waker,
    expiration_tick: u64,
    generation: u64,
    level: usize,
    slot: usize,
}

/// A hierarchical hashed timing wheel with 7 levels and 64 slots each.
/// One tick is equivalent to 1 millisecond.
///
/// The timing wheel uses a hierarchical design where:
/// - Level 0: 64 slots, each representing 1ms (covers 0-64ms)
/// - Level 1: 64 slots, each representing 64ms (covers 64ms-4.1s)
/// - Level 2: 64 slots, each representing 4.1s (covers 4.1s-4.4min)
/// - Level 3: 64 slots, each representing 4.4min (covers 4.4min-4.7h)
/// - Level 4: 64 slots, each representing 4.7h (covers 4.7h-12.6days)
/// - Level 5: 64 slots, each representing 12.6days (covers 12.6days-2.2years)
/// - Level 6: 64 slots, each representing 2.2years (covers 2.2years-~139years)
struct TimingWheel {
    /// Slab storage for all timer entries
    entries: Slab<TimerEntry>,
    /// The wheels for each level
    wheels: [Vec<Vec<usize>>; NUM_LEVELS],
    /// Current tick count (milliseconds since creation)
    current_tick: u64,
    /// Generation counter for handling timer reuse
    generation_counter: u64,
    /// Binary heap of (`expiration_tick`, `slab_index`, generation) for O(log n)
    /// min-expiration tracking instead of O(n) scan on remove/expire.
    expiration_heap: BinaryHeap<Reverse<(u64, usize, u64)>>,
}

impl TimingWheel {
    #[inline]
    pub fn new() -> Self {
        Self {
            entries: Slab::new(),
            wheels: std::array::from_fn(|_| vec![Vec::new(); SLOTS_PER_LEVEL]),
            current_tick: 0,
            generation_counter: 0,
            expiration_heap: BinaryHeap::new(),
        }
    }

    /// Check if the wheel is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the current tick
    #[inline]
    pub fn now(&self) -> u64 {
        self.current_tick
    }

    /// Get the nearest wakeup tick (absolute), if any timer is scheduled
    /// This is amortized O(1) because stale heap entries are lazily cleaned.
    #[inline]
    pub fn nearest_wakeup(&mut self) -> Option<std::num::NonZeroU64> {
        self.refresh_min_expiration();
        self.expiration_heap
            .peek()
            .map(|&Reverse((tick, _, _))| tick)
            .and_then(std::num::NonZeroU64::new)
    }

    /// Lazily refresh the minimum expiration tick by popping stale entries
    /// from the binary heap. Amortized O(1), because only the heap top is inspected.
    #[inline]
    fn refresh_min_expiration(&mut self) {
        while let Some(&Reverse((_tick, idx, gen))) = self.expiration_heap.peek() {
            if let Some(entry) = self.entries.get(idx) {
                if entry.generation == gen {
                    return;
                }
            }
            self.expiration_heap.pop();
        }
    }

    /// Insert a timer that expires at the given tick
    #[inline]
    pub fn insert(&mut self, waker: Waker, expiration_tick: u64) -> TimerHandle {
        let delay = expiration_tick.saturating_sub(self.current_tick);
        let (level, slot) = Self::calculate_level_and_slot(delay);

        self.generation_counter += 1;
        let generation = self.generation_counter;

        let slab_index = self.entries.insert(TimerEntry {
            waker,
            expiration_tick,
            generation,
            level,
            slot,
        });

        self.wheels[level][slot].push(slab_index);
        self.expiration_heap
            .push(Reverse((expiration_tick, slab_index, generation)));

        TimerHandle {
            slab_index,
            generation,
        }
    }

    /// Remove a timer by its handle
    #[inline]
    pub fn remove(&mut self, handle: TimerHandle) {
        if let Some(entry) = self.entries.get(handle.slab_index) {
            if entry.generation == handle.generation {
                let level = entry.level;
                let slot = entry.slot;
                let slab_index = handle.slab_index;

                if let Some(pos) = self.wheels[level][slot]
                    .iter()
                    .position(|&idx| idx == slab_index)
                {
                    self.wheels[level][slot].remove(pos);
                }

                self.entries.remove(slab_index);
                self.refresh_min_expiration();
                self.compact_expiration_heap();
            }
        }
    }

    /// Drop the heap entries of cancelled timers once they outnumber the live
    /// ones. The lazy cleanup only pops stale entries from the top, so without
    /// this a cancelled timer stays in the heap until its deadline would have
    /// passed, and a timer cancelled and resubmitted on every wakeup grows the
    /// heap without bound. Rebuilding costs O(live) and leaves at least that
    /// much slack, so a removal stays amortized O(1).
    #[inline]
    fn compact_expiration_heap(&mut self) {
        if self.expiration_heap.len() <= 2 * self.entries.len() + SLOTS_PER_LEVEL {
            return;
        }
        let entries = &self.entries;
        self.expiration_heap.retain(|&Reverse((_tick, idx, gen))| {
            entries
                .get(idx)
                .is_some_and(|entry| entry.generation == gen)
        });
    }

    /// Advance the timing wheel by the given number of ticks
    /// Returns a vector of wakers that have expired
    /// This is optimized to only process slots that have timers, not every tick
    pub fn advance(&mut self, ticks: u64) -> Vec<Waker> {
        if ticks == 0 {
            return Vec::new();
        }

        let mut expired_wakers = Vec::new();
        let start_tick = self.current_tick;
        self.current_tick += ticks;

        // First, cascade timers from higher levels down to level 0
        // This must happen before processing level 0 slots
        self.cascade_all_levels(&mut expired_wakers);

        let start_slot =
            usize::try_from(start_tick).expect("start_tick fits in usize") & (SLOTS_PER_LEVEL - 1);
        let end_slot = usize::try_from(self.current_tick).expect("current_tick fits in usize")
            & (SLOTS_PER_LEVEL - 1);

        if ticks >= SLOTS_PER_LEVEL as u64 {
            // We've wrapped around at least once, process all slots
            for slot in 0..SLOTS_PER_LEVEL {
                self.process_slot_at_level_0(slot, &mut expired_wakers);
            }
        } else if start_slot <= end_slot {
            // No wrap around, process slots in range
            for slot in start_slot..=end_slot {
                self.process_slot_at_level_0(slot, &mut expired_wakers);
            }
        } else {
            // Wrapped around, process from start to end, then 0 to end
            for slot in start_slot..SLOTS_PER_LEVEL {
                self.process_slot_at_level_0(slot, &mut expired_wakers);
            }
            for slot in 0..=end_slot {
                self.process_slot_at_level_0(slot, &mut expired_wakers);
            }
        }

        // The binary heap lazily cleans stale entries in O(log n).
        self.refresh_min_expiration();

        expired_wakers
    }

    /// Cascade all timers from higher levels down through the hierarchy
    /// This processes all slots at each level and moves timers to appropriate lower levels
    fn cascade_all_levels(&mut self, expired_wakers: &mut Vec<Waker>) {
        for level in (1..NUM_LEVELS).rev() {
            self.cascade_level(level, expired_wakers);
        }
    }

    /// Process timers at a specific level in the timer wheel
    #[inline]
    fn process_slot(&mut self, timers_to_process: Vec<usize>, expired_wakers: &mut Vec<Waker>) {
        for slab_index in timers_to_process {
            if let Some(entry) = self.entries.get(slab_index) {
                if entry.expiration_tick <= self.current_tick {
                    // Timer has expired
                    let entry = self.entries.remove(slab_index);
                    expired_wakers.push(entry.waker);
                } else {
                    // Re-calculate the appropriate level and slot based on remaining delay
                    let delay = entry.expiration_tick - self.current_tick;
                    let (new_level, new_slot) = Self::calculate_level_and_slot(delay);

                    if let Some(entry) = self.entries.get_mut(slab_index) {
                        entry.level = new_level;
                        entry.slot = new_slot;
                    }
                    self.wheels[new_level][new_slot].push(slab_index);
                }
            }
        }
    }

    /// Cascade all timers from a specific level down to lower levels
    fn cascade_level(&mut self, level: usize, expired_wakers: &mut Vec<Waker>) {
        for slot in 0..SLOTS_PER_LEVEL {
            let timers_to_cascade: Vec<usize> = std::mem::take(&mut self.wheels[level][slot]);
            self.process_slot(timers_to_cascade, expired_wakers);
        }
    }

    /// Process a specific slot at level 0
    fn process_slot_at_level_0(&mut self, slot: usize, expired_wakers: &mut Vec<Waker>) {
        let timers_to_process: Vec<usize> = std::mem::take(&mut self.wheels[0][slot]);
        self.process_slot(timers_to_process, expired_wakers);
    }

    /// Calculate the appropriate level and slot for a given delay
    #[inline]
    fn calculate_level_and_slot(delay: u64) -> (usize, usize) {
        let mut level = 0;
        loop {
            let level_shift = 6 * level;

            if level == NUM_LEVELS - 1 {
                // Top level: use the highest bits
                let slot = usize::try_from((delay >> level_shift) & (SLOTS_PER_LEVEL as u64 - 1))
                    .expect("slot index fits in usize");
                return (level, slot);
            }

            // Check if delay fits in this level
            let max_for_level = ((SLOTS_PER_LEVEL as u64) << level_shift) - 1;
            if delay <= max_for_level || level == NUM_LEVELS - 1 {
                let slot = usize::try_from((delay >> level_shift) & (SLOTS_PER_LEVEL as u64 - 1))
                    .expect("slot index fits in usize");
                return (level, slot);
            }

            level += 1;
        }
    }
}

impl Default for TimingWheel {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Timer {
    // The timer wheel will overflow after ~139 years of waiting,
    // but running for that long is very unlikely.
    wheel: RefCell<TimingWheel>,
    instant: RefCell<Instant>,
}

impl Timer {
    #[inline]
    pub fn new() -> Self {
        Self {
            wheel: RefCell::new(TimingWheel::new()),
            instant: RefCell::new(Instant::now()),
        }
    }

    #[inline]
    pub fn submit(&self, deadline: Instant, waker: Waker) -> Option<TimerHandle> {
        let millis = u64::try_from(
            deadline
                .saturating_duration_since(*self.instant.borrow())
                .as_millis(),
        )
        .ok()?; // If conversion fails, deadline is too long, just return None.
        if millis < 1 {
            // If the duration is less than 1 millisecond, wake the task immediately
            // One tick is equivalent to 1 millisecond.
            waker.wake();
            return None;
        }
        let mut wheel = self.wheel.borrow_mut();
        let now = wheel.now();
        Some(wheel.insert(waker, now + millis))
    }

    #[inline]
    pub fn cancel(&self, handle: TimerHandle) {
        let mut wheel = self.wheel.borrow_mut();
        wheel.remove(handle);
    }

    #[inline]
    pub fn spin_and_get_deadline(&self) -> (Option<Duration>, bool) {
        if self.wheel.borrow().is_empty() {
            // Just reset the instant in the fast path with empty timer wheel,
            // to bypass the timing wheel in the next call.
            //
            // Also, this would prevent wasting the cursor.
            *self.instant.borrow_mut() = Instant::now();
            return (None, false);
        }

        let mut instant = self.instant.borrow_mut();
        let mut wheel = self.wheel.borrow_mut();
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(*instant);
        *instant = now;
        let mut woken_up = false;
        // Advance the timer wheel
        for waker in wheel
            .advance(u64::try_from(elapsed.as_millis()).expect("elapsed milliseconds fit in u64"))
        {
            // Wake the pending tasks
            waker.wake();
            woken_up = true;
        }

        // The deadline is absolute, so we need to subtract the current time to get the relative deadline
        // If there's a deadline, we need to ensure it's at least 1ms to avoid busy looping
        let now_tick = wheel.now();
        (
            wheel.nearest_wakeup().map(|deadline| {
                Duration::from_millis(deadline.get().saturating_sub(now_tick).max(1))
            }),
            woken_up,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::Waker;
    use std::time::Duration;

    struct MockWakerInner(Arc<Mutex<u32>>);

    impl std::task::Wake for MockWakerInner {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            *self.0.lock().unwrap() += 1;
        }
    }

    fn mock_waker(counter: Arc<Mutex<u32>>) -> Waker {
        Waker::from(Arc::new(MockWakerInner(counter)))
    }

    #[test]
    fn test_timer_submit_and_deadline() {
        let timer = Timer::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = timer.submit(Instant::now() + Duration::from_millis(50), waker);

        let (deadline, _woken_up) = timer.spin_and_get_deadline();
        assert!(deadline.is_some());
        let ms = deadline.unwrap().as_millis();
        assert!(
            (40..=50).contains(&ms),
            "Deadline should be between 45ms and 50ms, got {ms}"
        );
    }

    #[test]
    fn test_timer_cancel() {
        let timer = Timer::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let handle = timer
            .submit(Instant::now() + Duration::from_millis(100), waker)
            .expect("Failed to submit timer");
        timer.cancel(handle);

        let (deadline, _woken_up) = timer.spin_and_get_deadline();
        assert!(deadline.is_none());
    }

    #[test]
    fn test_timing_wheel_empty() {
        let mut wheel = TimingWheel::new();
        assert!(wheel.is_empty());
        assert_eq!(wheel.now(), 0);
        assert!(wheel.nearest_wakeup().is_none());
    }

    #[test]
    fn test_timing_wheel_insert_and_expire_single() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = wheel.insert(waker, 10);
        assert!(!wheel.is_empty());
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(10).unwrap())
        );

        let expired = wheel.advance(9);
        assert!(expired.is_empty());
        assert_eq!(wheel.now(), 9);

        let expired = wheel.advance(1);
        assert_eq!(expired.len(), 1);
        assert!(wheel.is_empty());
        assert!(wheel.nearest_wakeup().is_none());
    }

    #[test]
    fn test_timing_wheel_insert_and_expire_immediate() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = wheel.insert(waker, 0);
        // Note: nearest_wakeup returns None for tick 0 because NonZeroU64 can't represent 0
        // But the timer is still in the wheel
        assert!(!wheel.is_empty());

        let expired = wheel.advance(1);
        assert_eq!(expired.len(), 1);
        assert!(wheel.is_empty());
        assert!(wheel.nearest_wakeup().is_none());
    }

    #[test]
    fn test_timing_wheel_cancel() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let handle = wheel.insert(waker, 100);
        assert!(!wheel.is_empty());

        wheel.remove(handle);
        assert!(wheel.is_empty());
        assert!(wheel.nearest_wakeup().is_none());
    }

    #[test]
    fn test_timing_wheel_cancel_wrong_generation() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let first_handle = wheel.insert(waker, 100);
        wheel.remove(first_handle);

        // Same slab_index but different generation
        let waker2 = mock_waker(counter.clone());
        let second_handle = wheel.insert(waker2, 200);

        wheel.remove(first_handle);
        assert!(!wheel.is_empty());
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(200).unwrap())
        );

        wheel.remove(second_handle);
        assert!(wheel.is_empty());
    }

    #[test]
    fn test_timing_wheel_multiple_timers_same_slot() {
        let mut wheel = TimingWheel::new();
        let counter1 = Arc::new(Mutex::new(0));
        let counter2 = Arc::new(Mutex::new(0));
        let counter3 = Arc::new(Mutex::new(0));

        let _h1 = wheel.insert(mock_waker(counter1.clone()), 50);
        let _h2 = wheel.insert(mock_waker(counter2.clone()), 50);
        let _h3 = wheel.insert(mock_waker(counter3.clone()), 50);

        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(50).unwrap())
        );

        let expired = wheel.advance(50);
        assert_eq!(expired.len(), 3);

        for waker in expired {
            waker.wake();
        }

        assert_eq!(*counter1.lock().unwrap(), 1);
        assert_eq!(*counter2.lock().unwrap(), 1);
        assert_eq!(*counter3.lock().unwrap(), 1);
    }

    #[test]
    fn test_timing_wheel_multiple_timers_different_levels() {
        let mut wheel = TimingWheel::new();
        let counter1 = Arc::new(Mutex::new(0));
        let counter2 = Arc::new(Mutex::new(0));
        let counter3 = Arc::new(Mutex::new(0));

        // Level 0: 1-63ms
        let _h1 = wheel.insert(mock_waker(counter1.clone()), 10);
        // Level 1: 64-4095ms
        let _h2 = wheel.insert(mock_waker(counter2.clone()), 100);
        // Level 2: 4096-262143ms
        let _h3 = wheel.insert(mock_waker(counter3.clone()), 5000);

        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(10).unwrap())
        );

        // 4900 + 90 + 10 = 5000
        let expired = wheel.advance(10);
        assert_eq!(expired.len(), 1);
        expired.into_iter().for_each(std::task::Waker::wake);
        assert_eq!(*counter1.lock().unwrap(), 1);

        let expired = wheel.advance(90);
        assert_eq!(expired.len(), 1);
        expired.into_iter().for_each(std::task::Waker::wake);
        assert_eq!(*counter2.lock().unwrap(), 1);

        let expired = wheel.advance(4900);
        assert_eq!(expired.len(), 1);
        expired.into_iter().for_each(std::task::Waker::wake);
        assert_eq!(*counter3.lock().unwrap(), 1);
    }

    #[test]
    fn test_timing_wheel_large_advance() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = wheel.insert(waker, 1000);

        let expired = wheel.advance(1000);
        assert_eq!(expired.len(), 1);
        assert_eq!(wheel.now(), 1000);
    }

    #[test]
    fn test_timing_wheel_wrap_around_level_0() {
        let mut wheel = TimingWheel::new();
        let counter1 = Arc::new(Mutex::new(0));
        let counter2 = Arc::new(Mutex::new(0));

        // Insert timer at slot 63 (near wrap)
        let _h1 = wheel.insert(mock_waker(counter1.clone()), 63);
        // Insert timer that wraps around
        let _h2 = wheel.insert(mock_waker(counter2.clone()), 65);

        let expired = wheel.advance(65);
        assert_eq!(expired.len(), 2);
    }

    #[test]
    fn test_timing_wheel_nearest_wakeup_updates() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));

        let _h1 = wheel.insert(mock_waker(counter.clone()), 100);
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(100).unwrap())
        );

        let _h2 = wheel.insert(mock_waker(counter.clone()), 50);
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(50).unwrap())
        );

        let _h3 = wheel.insert(mock_waker(counter.clone()), 200);
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(50).unwrap())
        );

        wheel.advance(50);
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(100).unwrap())
        );
    }

    #[test]
    fn test_timing_wheel_boundary_values() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));

        // Test boundary: exactly 63 (last slot of level 0)
        let _h1 = wheel.insert(mock_waker(counter.clone()), 63);
        let expired = wheel.advance(63);
        assert_eq!(expired.len(), 1);

        // Test boundary: exactly 64 (first slot of level 1)
        let _h2 = wheel.insert(mock_waker(counter.clone()), 64);
        let expired = wheel.advance(64);
        assert_eq!(expired.len(), 1);

        // Test boundary: 4095 (last slot of level 1)
        let _h3 = wheel.insert(mock_waker(counter.clone()), 4095);
        let expired = wheel.advance(4095);
        assert_eq!(expired.len(), 1);

        // Test boundary: 4096 (first slot of level 2)
        let _h4 = wheel.insert(mock_waker(counter.clone()), 4096);
        let expired = wheel.advance(4096);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn test_timing_wheel_zero_advance() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = wheel.insert(waker, 100);
        let expired = wheel.advance(0);
        assert!(expired.is_empty());
        assert_eq!(wheel.now(), 0);
    }

    #[test]
    fn test_timing_wheel_cascade_from_higher_levels() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));

        // Insert timer at level 2 (4096+)
        let _handle = wheel.insert(mock_waker(counter.clone()), 5000);

        let expired = wheel.advance(5000);
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn test_timing_wheel_many_timers() {
        let mut wheel = TimingWheel::new();
        let mut handles = Vec::new();
        let counter = Arc::new(Mutex::new(0));

        for i in 1..=100 {
            let waker = mock_waker(counter.clone());
            let handle = wheel.insert(waker, i * 10);
            handles.push(handle);
        }

        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(10).unwrap())
        );

        for (i, handle) in handles.iter().enumerate() {
            if i % 2 == 0 {
                wheel.remove(*handle);
            }
        }

        let expired = wheel.advance(1000);
        assert_eq!(expired.len(), 50);
    }

    #[test]
    fn test_timing_wheel_cancelled_timers_do_not_pile_up() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));

        // A live timer due soon keeps the heap's top live, so the lazy cleanup
        // never reaches the cancelled entries below it.
        let _live = wheel.insert(mock_waker(counter.clone()), 10);

        // A `Sleep` polled again before its deadline cancels and resubmits its
        // timer: a busy connection with an hour-long timeout does this on every
        // wakeup.
        for _ in 0..10_000 {
            let handle = wheel.insert(mock_waker(counter.clone()), 3_600_000);
            wheel.remove(handle);
        }

        assert!(
            wheel.expiration_heap.len() < 100,
            "the heap holds {} entries for 1 live timer",
            wheel.expiration_heap.len()
        );
        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(10).unwrap())
        );
    }

    #[test]
    fn test_timing_wheel_reinsert_on_early_wake() {
        let mut wheel = TimingWheel::new();
        let counter = Arc::new(Mutex::new(0));
        let waker = mock_waker(counter.clone());

        let _handle = wheel.insert(waker, 100);

        let expired = wheel.advance(50);
        assert!(expired.is_empty());
        assert!(!wheel.is_empty());

        assert_eq!(
            wheel.nearest_wakeup(),
            Some(std::num::NonZeroU64::new(100).unwrap())
        );

        let expired = wheel.advance(50);
        assert_eq!(expired.len(), 1);
    }
}
