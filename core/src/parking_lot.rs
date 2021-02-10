// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
use crate::thread_parker::{ThreadParker, ThreadParkerT, UnparkHandleT};
use crate::util::UncheckedOptionExt;
use core::{
    cell::{Cell, UnsafeCell},
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};
use instant::Instant;
use smallvec::SmallVec;
use std::time::Duration;

use safe_lock::SafeLock;
use safe_lock::Guard as LockGuard;
use locked_bucket::LockedBucket;

mod safe_lock {
    use crate::word_lock::WordLock;
    use core::cell:: UnsafeCell;

    pub(crate) struct SafeLock<T> {
        lock: WordLock,
        data: UnsafeCell<T>,
    }

    impl<T> SafeLock<T> {
        pub fn new(value: T) -> Self {
            SafeLock {
                lock: WordLock::new(),
                data: UnsafeCell::new(value),
            }
        }

        pub fn get_mut(&mut self) -> &mut T {
            // get_mut on UnsafeCell requires nightly
            unsafe {
                &mut *self.data.get()
            }
        }

        pub fn lock(&self) -> Guard<'_, T> {
            unsafe {
                self.lock.lock();
                self.assume_locked()
            }
        }

        pub unsafe fn assume_locked(&self) -> Guard<'_, T> {
            Guard {
                lock: self,
                _mark_not_send: Default::default(),
            }
        }
    }

    pub(crate) struct Guard<'a, T> {
        lock: &'a SafeLock<T>,
        _mark_not_send: core::marker::PhantomData<*mut T>,
    }

    impl<'a, T: Sync> Guard<'a, T> {
        pub fn forget(this: Self) -> &'a mut T {
            unsafe {
                let data = &mut *this.lock.data.get();
                core::mem::forget(this);
                data
            }
        }
    }

    // Sync is safe because the guard doesn't do any modification on internal lock data
    // using shared reference - same logic as in std
    unsafe impl<'a, T: Sync> Sync for Guard<'a, T> {}

    impl<'a, T: Sync> core::ops::Deref for Guard<'a, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            unsafe {
                &*self.lock.data.get()
            }
        }
    }

    impl<'a, T: Sync> core::ops::DerefMut for Guard<'a, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            unsafe {
                &mut *self.lock.data.get()
            }
        }
    }

    impl<'a, T> Drop for Guard<'a, T> {
        fn drop(&mut self) {
            unsafe {
                self.lock.lock.unlock()
            }
        }
    }
}

mod locked_hash_table {
    use super::{HashTable, LockGuard};

    pub(crate) struct LockedHashTable<'a> {
        hash_table: &'a HashTable,
        _mark_not_send: core::marker::PhantomData<*const HashTable>,
    }

    impl<'a> LockedHashTable<'a> {
        pub(crate) fn new(hash_table: &'a HashTable) -> Self {
            for bucket in &hash_table.entries[..] {
                LockGuard::forget(bucket.queue.lock());
            }

            LockedHashTable {
                hash_table,
                _mark_not_send: Default::default(),
            }
        }

        pub(crate) fn table(&self) -> &'a HashTable {
            self.hash_table
        }

        pub(crate) fn queues_mut(&mut self) -> impl Iterator<Item=&mut super::Queue> {
            self.hash_table.entries.iter().map(|bucket| unsafe { LockGuard::forget(bucket.queue.assume_locked()) })
        }
    }

    impl<'a> Drop for LockedHashTable<'a> {
        fn drop(&mut self) {
            for bucket in &self.hash_table.entries[..] {
                // SAFETY: We hold the lock here, as required
                unsafe { bucket.queue.assume_locked() };
            }
        }
    }
}

static NUM_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Holds the pointer to the currently active `HashTable`.
///
/// # Safety
///
/// Except for the initial value of null, it must always point to a valid `HashTable` instance.
/// Any `HashTable` this global static has ever pointed to must never be freed.
static HASHTABLE: AtomicPtr<HashTable> = AtomicPtr::new(ptr::null_mut());

// Even with 3x more buckets than threads, the memory overhead per thread is
// still only a few hundred bytes per thread.
const LOAD_FACTOR: usize = 3;

pub(crate) struct HashTable {
    // Hash buckets for the table
    entries: Box<[Bucket]>,

    // Number of bits used for the hash function
    hash_bits: u32,

    // Previous table. This is only kept to keep leak detectors happy.
    _prev: *const HashTable,
}

impl HashTable {
    #[inline]
    fn new(num_threads: usize, prev: *const HashTable) -> Box<HashTable> {
        let new_size = (num_threads * LOAD_FACTOR).next_power_of_two();
        let hash_bits = 0usize.leading_zeros() - new_size.leading_zeros() - 1;

        let now = Instant::now();
        let mut entries = Vec::with_capacity(new_size);
        for i in 0..new_size {
            // We must ensure the seed is not zero
            entries.push(Bucket::new(now, i as u32 + 1));
        }

        Box::new(HashTable {
            entries: entries.into_boxed_slice(),
            hash_bits,
            _prev: prev,
        })
    }

    fn lock_all_queues(&self) -> locked_hash_table::LockedHashTable<'_> {
        locked_hash_table::LockedHashTable::new(self)
    }
}

pub(crate) struct Queue {
    head: *const ThreadData,
    tail: *const ThreadData,
}

// This is not actually safe yet, the Queue operations must be protected by module boundary to be
// safe. Should be fine for a while.
unsafe impl Sync for Queue {}

impl Queue {
    fn new() -> Self {
        Queue {
            head: ptr::null(),
            tail: ptr::null(),
        }
    }
}

mod locked_bucket {
    use super::{Bucket, Queue, LockGuard};

    pub(crate) struct LockedBucket<'a> {
        bucket: &'a Bucket,
        _mark_not_send: core::marker::PhantomData<*const Bucket>,
    }

    impl<'a> LockedBucket<'a> {
        pub fn new(bucket: &'a Bucket) -> Self {
            LockGuard::forget(bucket.queue.lock());
            LockedBucket {
                bucket,
                _mark_not_send: Default::default(),
            }
        }

        pub fn bucket(&self) -> &'a Bucket {
            self.bucket
        }

        pub fn queue(&mut self) -> &mut Queue {
            unsafe {
                LockGuard::forget(self.bucket.queue.assume_locked())
            }
        }

        pub fn into_queue_guard(self) -> LockGuard<'a, Queue> {
            unsafe {
                let guard = self.bucket.queue.assume_locked();
                core::mem::forget(self);
                guard
            }
        }
    }

    impl<'a> Drop for LockedBucket<'a> {
        fn drop(&mut self) {
            unsafe {
                self.bucket.queue.assume_locked();
            }
        }
    }
}

#[repr(align(64))]
pub(crate) struct Bucket {
    // Linked list of threads waiting on this bucket
    queue: SafeLock<Queue>,

    // Next time at which point be_fair should be set
    fair_timeout: UnsafeCell<FairTimeout>,
}

impl Bucket {
    #[inline]
    pub fn new(timeout: Instant, seed: u32) -> Self {
        Self {
            queue: SafeLock::new(Queue::new()),
            fair_timeout: UnsafeCell::new(FairTimeout::new(timeout, seed)),
        }
    }

    fn lock(&self) -> LockedBucket<'_> {
        LockedBucket::new(self)
    }
}

struct FairTimeout {
    // Next time at which point be_fair should be set
    timeout: Instant,

    // the PRNG state for calculating the next timeout
    seed: u32,
}

impl FairTimeout {
    #[inline]
    fn new(timeout: Instant, seed: u32) -> FairTimeout {
        FairTimeout { timeout, seed }
    }

    // Determine whether we should force a fair unlock, and update the timeout
    #[inline]
    fn should_timeout(&mut self) -> bool {
        let now = Instant::now();
        if now > self.timeout {
            // Time between 0 and 1ms.
            let nanos = self.gen_u32() % 1_000_000;
            self.timeout = now + Duration::new(0, nanos);
            true
        } else {
            false
        }
    }

    // Pseudorandom number generator from the "Xorshift RNGs" paper by George Marsaglia.
    fn gen_u32(&mut self) -> u32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed
    }
}

struct ThreadData {
    parker: ThreadParker,

    // Key that this thread is sleeping on. This may change if the thread is
    // requeued to a different key.
    key: AtomicUsize,

    // Linked list of parked threads in a bucket
    next_in_queue: Cell<*const ThreadData>,

    // UnparkToken passed to this thread when it is unparked
    unpark_token: Cell<UnparkToken>,

    // ParkToken value set by the thread when it was parked
    park_token: Cell<ParkToken>,

    // Is the thread parked with a timeout?
    parked_with_timeout: Cell<bool>,

    // Extra data for deadlock detection
    #[cfg(feature = "deadlock_detection")]
    deadlock_data: deadlock::DeadlockData,
}

impl ThreadData {
    fn new() -> ThreadData {
        // Keep track of the total number of live ThreadData objects and resize
        // the hash table accordingly.
        let num_threads = NUM_THREADS.fetch_add(1, Ordering::Relaxed) + 1;
        grow_hashtable(num_threads);

        ThreadData {
            parker: ThreadParker::new(),
            key: AtomicUsize::new(0),
            next_in_queue: Cell::new(ptr::null()),
            unpark_token: Cell::new(DEFAULT_UNPARK_TOKEN),
            park_token: Cell::new(DEFAULT_PARK_TOKEN),
            parked_with_timeout: Cell::new(false),
            #[cfg(feature = "deadlock_detection")]
            deadlock_data: deadlock::DeadlockData::new(),
        }
    }
}

// Invokes the given closure with a reference to the current thread `ThreadData`.
#[inline(always)]
fn with_thread_data<T>(f: impl FnOnce(&ThreadData) -> T) -> T {
    // Unlike word_lock::ThreadData, parking_lot::ThreadData is always expensive
    // to construct. Try to use a thread-local version if possible. Otherwise just
    // create a ThreadData on the stack
    let mut thread_data_storage = None;
    thread_local!(static THREAD_DATA: ThreadData = ThreadData::new());
    let thread_data_ptr = THREAD_DATA
        .try_with(|x| x as *const ThreadData)
        .unwrap_or_else(|_| thread_data_storage.get_or_insert_with(ThreadData::new));

    f(unsafe { &*thread_data_ptr })
}

impl Drop for ThreadData {
    fn drop(&mut self) {
        NUM_THREADS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Returns a reference to the latest hash table, creating one if it doesn't exist yet.
/// The reference is valid forever. However, the `HashTable` it references might become stale
/// at any point. Meaning it still exists, but it is not the instance in active use.
#[inline]
fn get_hashtable() -> &'static HashTable {
    let table = HASHTABLE.load(Ordering::Acquire);

    // If there is no table, create one
    if table.is_null() {
        create_hashtable()
    } else {
        // SAFETY: when not null, `HASHTABLE` always points to a `HashTable` that is never freed.
        unsafe { &*table }
    }
}

/// Returns a reference to the latest hash table, creating one if it doesn't exist yet.
/// The reference is valid forever. However, the `HashTable` it references might become stale
/// at any point. Meaning it still exists, but it is not the instance in active use.
#[cold]
fn create_hashtable() -> &'static HashTable {
    let new_table = Box::into_raw(HashTable::new(LOAD_FACTOR, ptr::null()));

    // If this fails then it means some other thread created the hash table first.
    let table = match HASHTABLE.compare_exchange(
        ptr::null_mut(),
        new_table,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => new_table,
        Err(old_table) => {
            // Free the table we created
            // SAFETY: `new_table` is created from `Box::into_raw` above and only freed here.
            unsafe {
                Box::from_raw(new_table);
            }
            old_table
        }
    };
    // SAFETY: The `HashTable` behind `table` is never freed. It is either the table pointer we
    // created here, or it is one loaded from `HASHTABLE`.
    unsafe { &*table }
}

// Grow the hash table so that it is big enough for the given number of threads.
// This isn't performance-critical since it is only done when a ThreadData is
// created, which only happens once per thread.
fn grow_hashtable(num_threads: usize) {
    // Lock all buckets in the existing table and get a reference to it
    let mut old_table = loop {
        let table = get_hashtable();

        // Check if we need to resize the existing table
        if table.entries.len() >= LOAD_FACTOR * num_threads {
            return;
        }

        let locked = table.lock_all_queues();

        // Now check if our table is still the latest one. Another thread could
        // have grown the hash table between us reading HASHTABLE and locking
        // the buckets.
        if HASHTABLE.load(Ordering::Relaxed) == table as *const _ as *mut _ {
            break locked;
        }
    };

    // Create the new table
    let mut new_table = HashTable::new(num_threads, old_table.table());

    // Move the entries from the old table to the new one
    for queue in old_table.queues_mut() {
        // SAFETY: The park, unpark* and check_wait_graph_fast functions create only correct linked
        // lists. All `ThreadData` instances in these lists will remain valid as long as they are
        // present in the lists, meaning as long as their threads are parked.
        unsafe { rehash_bucket_into(queue, &mut new_table) };
    }

    // Publish the new table. No races are possible at this point because
    // any other thread trying to grow the hash table is blocked on the bucket
    // locks in the old table.
    HASHTABLE.store(Box::into_raw(new_table), Ordering::Release);
}

/// Iterate through all `ThreadData` objects in the bucket and insert them into the given table
/// in the bucket their key correspond to for this table.
///
/// # Safety
///
/// The given `bucket` must have a correctly constructed linked list under `queue_head`, containing
/// `ThreadData` instances that must stay valid at least as long as the given `table` is in use.
///
/// The given `table` must only contain buckets with correctly constructed linked lists.
unsafe fn rehash_bucket_into(queue: &mut Queue, table: &mut HashTable) {
    let mut current: *const ThreadData = queue.head;
    while !current.is_null() {
        //  This is the unsafety unsafe impl Sync for Queue talks about
        let next = (*current).next_in_queue.get();
        let hash = hash((*current).key.load(Ordering::Relaxed), table.hash_bits);
        if table.entries[hash].queue.get_mut().tail.is_null() {
            table.entries[hash].queue.get_mut().head = current;
        } else {
            (*table.entries[hash].queue.get_mut().tail)
                .next_in_queue
                .set(current);
        }
        table.entries[hash].queue.get_mut().tail = current;
        (*current).next_in_queue.set(ptr::null());
        current = next;
    }
}

// Hash function for addresses
#[cfg(target_pointer_width = "32")]
#[inline]
fn hash(key: usize, bits: u32) -> usize {
    key.wrapping_mul(0x9E3779B9) >> (32 - bits)
}
#[cfg(target_pointer_width = "64")]
#[inline]
fn hash(key: usize, bits: u32) -> usize {
    key.wrapping_mul(0x9E3779B97F4A7C15) >> (64 - bits)
}

/// Locks the bucket for the given key and returns a reference to it.
/// The returned bucket must be unlocked again in order to not cause deadlocks.
#[inline]
fn lock_bucket(key: usize) -> LockedBucket<'static> {
    loop {
        let hashtable = get_hashtable();

        let hash = hash(key, hashtable.hash_bits);
        let bucket = &hashtable.entries[hash];

        // Lock the bucket
        let guard= bucket.lock();

        // If no other thread has rehashed the table before we grabbed the lock
        // then we are good to go! The lock we grabbed prevents any rehashes.
        if HASHTABLE.load(Ordering::Relaxed) == hashtable as *const _ as *mut _ {
            return guard;
        }
    }
}

#[inline]
fn lock_queue(key: usize) -> LockGuard<'static, Queue> {
    lock_bucket(key).into_queue_guard()
}

/// Locks the bucket for the given key and returns a reference to it. But checks that the key
/// hasn't been changed in the meantime due to a requeue.
/// The returned bucket must be unlocked again in order to not cause deadlocks.
#[inline]
fn lock_bucket_checked(key: &AtomicUsize) -> (usize, LockedBucket<'static>) {
    loop {
        let hashtable = get_hashtable();
        let current_key = key.load(Ordering::Relaxed);

        let hash = hash(current_key, hashtable.hash_bits);
        let bucket = &hashtable.entries[hash];

        // Lock the bucket
        let guard = bucket.lock();

        // Check that both the hash table and key are correct while the bucket
        // is locked. Note that the key can't change once we locked the proper
        // bucket for it, so we just keep trying until we have the correct key.
        if HASHTABLE.load(Ordering::Relaxed) == hashtable as *const _ as *mut _
            && key.load(Ordering::Relaxed) == current_key
        {
            return (current_key, guard);
        }
    }
}

#[inline]
fn lock_queue_checked(key: &AtomicUsize) -> (usize, LockGuard<'static, Queue>) {
    let (key, guard) = lock_bucket_checked(key);
    (key, guard.into_queue_guard())
}

/// Locks the two buckets for the given pair of keys and returns references to them.
/// The returned buckets must be unlocked again in order to not cause deadlocks.
///
/// If both keys hash to the same value, the second bucket will be `None`.
///
/// To avoid deadlocks the buckets have to be unlocked (dropped) from left to right.
#[inline]
fn lock_bucket_pair(key1: usize, key2: usize) -> (LockedBucket<'static>, Option<LockedBucket<'static>>) {
    loop {
        let hashtable = get_hashtable();

        let hash1 = hash(key1, hashtable.hash_bits);
        let hash2 = hash(key2, hashtable.hash_bits);

        // Get the bucket at the lowest hash/index first
        let bucket1 = if hash1 <= hash2 {
            &hashtable.entries[hash1]
        } else {
            &hashtable.entries[hash2]
        };

        // Lock the first bucket
        let guard1 = bucket1.lock();

        // If no other thread has rehashed the table before we grabbed the lock
        // then we are good to go! The lock we grabbed prevents any rehashes.
        if HASHTABLE.load(Ordering::Relaxed) == hashtable as *const _ as *mut _ {
            // Now lock the second bucket and return the two buckets
            if hash1 == hash2 {
                return (guard1, None);
            } else if hash1 < hash2 {
                let bucket2 = &hashtable.entries[hash2];
                let guard2 = bucket2.lock();
                return (guard1, Some(guard2));
            } else {
                let bucket2 = &hashtable.entries[hash1];
                let guard2 = bucket2.lock();
                return (guard2, Some(guard1));
            }
        }
    }
}

/// Result of a park operation.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ParkResult {
    /// We were unparked by another thread with the given token.
    Unparked(UnparkToken),

    /// The validation callback returned false.
    Invalid,

    /// The timeout expired.
    TimedOut,
}

impl ParkResult {
    /// Returns true if we were unparked by another thread.
    #[inline]
    pub fn is_unparked(self) -> bool {
        if let ParkResult::Unparked(_) = self {
            true
        } else {
            false
        }
    }
}

/// Result of an unpark operation.
#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub struct UnparkResult {
    /// The number of threads that were unparked.
    pub unparked_threads: usize,

    /// The number of threads that were requeued.
    pub requeued_threads: usize,

    /// Whether there are any threads remaining in the queue. This only returns
    /// true if a thread was unparked.
    pub have_more_threads: bool,

    /// This is set to true on average once every 0.5ms for any given key. It
    /// should be used to switch to a fair unlocking mechanism for a particular
    /// unlock.
    pub be_fair: bool,

    /// Private field so new fields can be added without breakage.
    _sealed: (),
}

/// Operation that `unpark_requeue` should perform.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum RequeueOp {
    /// Abort the operation without doing anything.
    Abort,

    /// Unpark one thread and requeue the rest onto the target queue.
    UnparkOneRequeueRest,

    /// Requeue all threads onto the target queue.
    RequeueAll,

    /// Unpark one thread and leave the rest parked. No requeuing is done.
    UnparkOne,

    /// Requeue one thread and leave the rest parked on the original queue.
    RequeueOne,
}

/// Operation that `unpark_filter` should perform for each thread.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FilterOp {
    /// Unpark the thread and continue scanning the list of parked threads.
    Unpark,

    /// Don't unpark the thread and continue scanning the list of parked threads.
    Skip,

    /// Don't unpark the thread and stop scanning the list of parked threads.
    Stop,
}

/// A value which is passed from an unparker to a parked thread.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct UnparkToken(pub usize);

/// A value associated with a parked thread which can be used by `unpark_filter`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct ParkToken(pub usize);

/// A default unpark token to use.
pub const DEFAULT_UNPARK_TOKEN: UnparkToken = UnparkToken(0);

/// A default park token to use.
pub const DEFAULT_PARK_TOKEN: ParkToken = ParkToken(0);

/// Parks the current thread in the queue associated with the given key.
///
/// The `validate` function is called while the queue is locked and can abort
/// the operation by returning false. If `validate` returns true then the
/// current thread is appended to the queue and the queue is unlocked.
///
/// The `before_sleep` function is called after the queue is unlocked but before
/// the thread is put to sleep. The thread will then sleep until it is unparked
/// or the given timeout is reached.
///
/// The `timed_out` function is also called while the queue is locked, but only
/// if the timeout was reached. It is passed the key of the queue it was in when
/// it timed out, which may be different from the original key if
/// `unpark_requeue` was called. It is also passed a bool which indicates
/// whether it was the last thread in the queue.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `validate` and `timed_out` functions are called while the queue is
/// locked and must not panic or call into any function in `parking_lot`.
///
/// The `before_sleep` function is called outside the queue lock and is allowed
/// to call `unpark_one`, `unpark_all`, `unpark_requeue` or `unpark_filter`, but
/// it is not allowed to call `park` or panic.
#[inline]
pub unsafe fn park(
    key: usize,
    validate: impl FnOnce() -> bool,
    before_sleep: impl FnOnce(),
    timed_out: impl FnOnce(usize, bool),
    park_token: ParkToken,
    timeout: Option<Instant>,
) -> ParkResult {
    // Grab our thread data, this also ensures that the hash table exists
    with_thread_data(|thread_data| {
        // Lock the bucket for the given key
        let mut queue = lock_queue(key);

        // If the validation function fails, just return
        if !validate() {
            return ParkResult::Invalid;
        }

        // Append our thread data to the queue and unlock the bucket
        thread_data.parked_with_timeout.set(timeout.is_some());
        thread_data.next_in_queue.set(ptr::null());
        thread_data.key.store(key, Ordering::Relaxed);
        thread_data.park_token.set(park_token);
        thread_data.parker.prepare_park();
        if !queue.head.is_null() {
            (*queue.tail).next_in_queue.set(thread_data);
        } else {
            queue.head = thread_data;
        }
        queue.tail = thread_data;
        drop(queue);

        // Invoke the pre-sleep callback
        before_sleep();

        // Park our thread and determine whether we were woken up by an unpark
        // or by our timeout. Note that this isn't precise: we can still be
        // unparked since we are still in the queue.
        let unparked = match timeout {
            Some(timeout) => thread_data.parker.park_until(timeout),
            None => {
                thread_data.parker.park();
                // call deadlock detection on_unpark hook
                deadlock::on_unpark(thread_data);
                true
            }
        };

        // If we were unparked, return now
        if unparked {
            return ParkResult::Unparked(thread_data.unpark_token.get());
        }

        // Lock our bucket again. Note that the hashtable may have been rehashed in
        // the meantime. Our key may also have changed if we were requeued.
        let (key, mut queue) = lock_queue_checked(&thread_data.key);

        // Now we need to check again if we were unparked or timed out. Unlike the
        // last check this is precise because we hold the bucket lock.
        if !thread_data.parker.timed_out() {
            return ParkResult::Unparked(thread_data.unpark_token.get());
        }

        // We timed out, so we now need to remove our thread from the queue
        let mut current = queue.head;
        let mut link = Cell::from_mut(&mut queue.head);
        let mut previous = ptr::null();
        let mut was_last_thread = true;
        while !current.is_null() {
            if current == thread_data {
                let next = (*current).next_in_queue.get();
                link.set(next);
                if queue.tail == current {
                    queue.tail = previous;
                } else {
                    // Scan the rest of the queue to see if there are any other
                    // entries with the given key.
                    let mut scan = next;
                    while !scan.is_null() {
                        if (*scan).key.load(Ordering::Relaxed) == key {
                            was_last_thread = false;
                            break;
                        }
                        scan = (*scan).next_in_queue.get();
                    }
                }

                // Callback to indicate that we timed out, and whether we were the
                // last thread on the queue.
                timed_out(key, was_last_thread);
                break;
            } else {
                if (*current).key.load(Ordering::Relaxed) == key {
                    was_last_thread = false;
                }
                link = &(*current).next_in_queue;
                previous = current;
                current = link.get();
            }
        }

        // There should be no way for our thread to have been removed from the queue
        // if we timed out.
        debug_assert!(!current.is_null());

        ParkResult::TimedOut
    })
}

/// Unparks one thread from the queue associated with the given key.
///
/// The `callback` function is called while the queue is locked and before the
/// target thread is woken up. The `UnparkResult` argument to the function
/// indicates whether a thread was found in the queue and whether this was the
/// last thread in the queue. This value is also returned by `unpark_one`.
///
/// The `callback` function should return an `UnparkToken` value which will be
/// passed to the thread that is unparked. If no thread is unparked then the
/// returned value is ignored.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `callback` function is called while the queue is locked and must not
/// panic or call into any function in `parking_lot`.
#[inline]
pub unsafe fn unpark_one(
    key: usize,
    callback: impl FnOnce(UnparkResult) -> UnparkToken,
) -> UnparkResult {
    // Lock the bucket for the given key
    let mut bucket = lock_bucket(key);

    // Find a thread with a matching key and remove it from the queue
    let mut current = bucket.queue().head;
    let mut link = Cell::from_mut(&mut bucket.queue().head);
    let mut previous = ptr::null();
    let mut result = UnparkResult::default();
    while !current.is_null() {
        if (*current).key.load(Ordering::Relaxed) == key {
            // Remove the thread from the queue
            let next = (*current).next_in_queue.get();
            link.set(next);
            if bucket.queue().tail == current {
                bucket.queue().tail = previous;
            } else {
                // Scan the rest of the queue to see if there are any other
                // entries with the given key.
                let mut scan = next;
                while !scan.is_null() {
                    if (*scan).key.load(Ordering::Relaxed) == key {
                        result.have_more_threads = true;
                        break;
                    }
                    scan = (*scan).next_in_queue.get();
                }
            }

            // Invoke the callback before waking up the thread
            result.unparked_threads = 1;
            result.be_fair = (*bucket.bucket().fair_timeout.get()).should_timeout();
            let token = callback(result);

            // Set the token for the target thread
            (*current).unpark_token.set(token);

            // This is a bit tricky: we first lock the ThreadParker to prevent
            // the thread from exiting and freeing its ThreadData if its wait
            // times out. Then we unlock the queue since we don't want to keep
            // the queue locked while we perform a system call. Finally we wake
            // up the parked thread.
            let handle = (*current).parker.unpark_lock();
            drop(bucket);
            handle.unpark();

            return result;
        } else {
            link = &(*current).next_in_queue;
            previous = current;
            current = link.get();
        }
    }

    // No threads with a matching key were found in the bucket
    callback(result);
    result
}

/// Unparks all threads in the queue associated with the given key.
///
/// The given `UnparkToken` is passed to all unparked threads.
///
/// This function returns the number of threads that were unparked.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
#[inline]
pub unsafe fn unpark_all(key: usize, unpark_token: UnparkToken) -> usize {
    // Lock the bucket for the given key
    let mut queue = lock_queue(key);
    // borrow checker needs a little help here
    let queue_mut = &mut *queue;

    // Remove all threads with the given key in the bucket
    let mut current = queue_mut.head;
    let mut link = Cell::from_mut(&mut queue_mut.head);
    let mut previous = ptr::null();
    let mut threads = SmallVec::<[_; 8]>::new();
    while !current.is_null() {
        if (*current).key.load(Ordering::Relaxed) == key {
            // Remove the thread from the queue
            let next = (*current).next_in_queue.get();
            link.set(next);
            if queue_mut.tail == current {
                queue_mut.tail = previous;
            }

            // Set the token for the target thread
            (*current).unpark_token.set(unpark_token);

            // Don't wake up threads while holding the queue lock. See comment
            // in unpark_one. For now just record which threads we need to wake
            // up.
            threads.push((*current).parker.unpark_lock());
            current = next;
        } else {
            link = &(*current).next_in_queue;
            previous = current;
            current = link.get();
        }
    }

    drop(queue);

    // Now that we are outside the lock, wake up all the threads that we removed
    // from the queue.
    let num_threads = threads.len();
    for handle in threads.into_iter() {
        handle.unpark();
    }

    num_threads
}

/// Removes all threads from the queue associated with `key_from`, optionally
/// unparks the first one and requeues the rest onto the queue associated with
/// `key_to`.
///
/// The `validate` function is called while both queues are locked. Its return
/// value will determine which operation is performed, or whether the operation
/// should be aborted. See `RequeueOp` for details about the different possible
/// return values.
///
/// The `callback` function is also called while both queues are locked. It is
/// passed the `RequeueOp` returned by `validate` and an `UnparkResult`
/// indicating whether a thread was unparked and whether there are threads still
/// parked in the new queue. This `UnparkResult` value is also returned by
/// `unpark_requeue`.
///
/// The `callback` function should return an `UnparkToken` value which will be
/// passed to the thread that is unparked. If no thread is unparked then the
/// returned value is ignored.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `validate` and `callback` functions are called while the queue is locked
/// and must not panic or call into any function in `parking_lot`.
#[inline]
pub unsafe fn unpark_requeue(
    key_from: usize,
    key_to: usize,
    validate: impl FnOnce() -> RequeueOp,
    callback: impl FnOnce(RequeueOp, UnparkResult) -> UnparkToken,
) -> UnparkResult {
    // Lock the two buckets for the given key
    let (mut bucket_from, mut bucket_to) = lock_bucket_pair(key_from, key_to);

    // If the validation function fails, just return
    let mut result = UnparkResult::default();
    let op = validate();
    if op == RequeueOp::Abort {
        drop(bucket_from);
        drop(bucket_to);
        return result;
    }

    // borrow checker needs a little help here
    let bucket_from_queue = &mut bucket_from.queue();

    // Remove all threads with the given key in the source bucket
    let mut current = bucket_from_queue.head;
    let mut link = Cell::from_mut(&mut bucket_from_queue.head);
    let mut previous = ptr::null();
    let mut requeue_threads: *const ThreadData = ptr::null();
    let mut requeue_threads_tail: *const ThreadData = ptr::null();
    let mut wakeup_thread = None;
    while !current.is_null() {
        if (*current).key.load(Ordering::Relaxed) == key_from {
            // Remove the thread from the queue
            let next = (*current).next_in_queue.get();
            link.set(next);
            if bucket_from_queue.tail == current {
                bucket_from_queue.tail = previous;
            }

            // Prepare the first thread for wakeup and requeue the rest.
            if (op == RequeueOp::UnparkOneRequeueRest || op == RequeueOp::UnparkOne)
                && wakeup_thread.is_none()
            {
                wakeup_thread = Some(current);
                result.unparked_threads = 1;
            } else {
                if !requeue_threads.is_null() {
                    (*requeue_threads_tail).next_in_queue.set(current);
                } else {
                    requeue_threads = current;
                }
                requeue_threads_tail = current;
                (*current).key.store(key_to, Ordering::Relaxed);
                result.requeued_threads += 1;
            }
            if op == RequeueOp::UnparkOne || op == RequeueOp::RequeueOne {
                // Scan the rest of the queue to see if there are any other
                // entries with the given key.
                let mut scan = next;
                while !scan.is_null() {
                    if (*scan).key.load(Ordering::Relaxed) == key_from {
                        result.have_more_threads = true;
                        break;
                    }
                    scan = (*scan).next_in_queue.get();
                }
                break;
            }
            current = next;
        } else {
            link = &(*current).next_in_queue;
            previous = current;
            current = link.get();
        }
    }

    // Add the requeued threads to the destination bucket
    if !requeue_threads.is_null() {
        (*requeue_threads_tail).next_in_queue.set(ptr::null());
        if !bucket_to.as_mut().unwrap_or(&mut bucket_from).queue().head.is_null() {
            (*bucket_to.as_mut().unwrap_or(&mut bucket_from).queue().tail)
                .next_in_queue
                .set(requeue_threads);
        } else {
            bucket_to.as_mut().unwrap_or(&mut bucket_from).queue().head = requeue_threads;
        }
        bucket_to.as_mut().unwrap_or(&mut bucket_from).queue().tail = requeue_threads_tail;
    }

    // Invoke the callback before waking up the thread
    if result.unparked_threads != 0 {
        result.be_fair = (*bucket_from.bucket().fair_timeout.get()).should_timeout();
    }
    let token = callback(op, result);

    // See comment in unpark_one for why we mess with the locking
    if let Some(wakeup_thread) = wakeup_thread {
        (*wakeup_thread).unpark_token.set(token);
        let handle = (*wakeup_thread).parker.unpark_lock();
        drop(bucket_from);
        drop(bucket_to);
        handle.unpark();
    } else {
        drop(bucket_from);
        drop(bucket_to);
    }

    result
}

/// Unparks a number of threads from the front of the queue associated with
/// `key` depending on the results of a filter function which inspects the
/// `ParkToken` associated with each thread.
///
/// The `filter` function is called for each thread in the queue or until
/// `FilterOp::Stop` is returned. This function is passed the `ParkToken`
/// associated with a particular thread, which is unparked if `FilterOp::Unpark`
/// is returned.
///
/// The `callback` function is also called while both queues are locked. It is
/// passed an `UnparkResult` indicating the number of threads that were unparked
/// and whether there are still parked threads in the queue. This `UnparkResult`
/// value is also returned by `unpark_filter`.
///
/// The `callback` function should return an `UnparkToken` value which will be
/// passed to all threads that are unparked. If no thread is unparked then the
/// returned value is ignored.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `filter` and `callback` functions are called while the queue is locked
/// and must not panic or call into any function in `parking_lot`.
#[inline]
pub unsafe fn unpark_filter(
    key: usize,
    mut filter: impl FnMut(ParkToken) -> FilterOp,
    callback: impl FnOnce(UnparkResult) -> UnparkToken,
) -> UnparkResult {
    // Lock the bucket for the given key
    let mut bucket = lock_bucket(key);
    // borrow checker needs a little help here
    let queue = bucket.queue();

    // Go through the queue looking for threads with a matching key
    let mut current = queue.head;
    let mut link = Cell::from_mut(&mut queue.head);
    let mut previous = ptr::null();
    let mut threads = SmallVec::<[_; 8]>::new();
    let mut result = UnparkResult::default();
    while !current.is_null() {
        if (*current).key.load(Ordering::Relaxed) == key {
            // Call the filter function with the thread's ParkToken
            let next = (*current).next_in_queue.get();
            match filter((*current).park_token.get()) {
                FilterOp::Unpark => {
                    // Remove the thread from the queue
                    link.set(next);
                    if queue.tail == current {
                        queue.tail = previous;
                    }

                    // Add the thread to our list of threads to unpark
                    threads.push((current, None));

                    current = next;
                }
                FilterOp::Skip => {
                    result.have_more_threads = true;
                    link = &(*current).next_in_queue;
                    previous = current;
                    current = link.get();
                }
                FilterOp::Stop => {
                    result.have_more_threads = true;
                    break;
                }
            }
        } else {
            link = &(*current).next_in_queue;
            previous = current;
            current = link.get();
        }
    }

    // Invoke the callback before waking up the threads
    result.unparked_threads = threads.len();
    if result.unparked_threads != 0 {
        result.be_fair = (*bucket.bucket().fair_timeout.get()).should_timeout();
    }
    let token = callback(result);

    // Pass the token to all threads that are going to be unparked and prepare
    // them for unparking.
    for t in threads.iter_mut() {
        (*t.0).unpark_token.set(token);
        t.1 = Some((*t.0).parker.unpark_lock());
    }

    drop(bucket);

    // Now that we are outside the lock, wake up all the threads that we removed
    // from the queue.
    for (_, handle) in threads.into_iter() {
        handle.unchecked_unwrap().unpark();
    }

    result
}

/// \[Experimental\] Deadlock detection
///
/// Enabled via the `deadlock_detection` feature flag.
pub mod deadlock {
    #[cfg(feature = "deadlock_detection")]
    use super::deadlock_impl;

    #[cfg(feature = "deadlock_detection")]
    pub(super) use super::deadlock_impl::DeadlockData;

    /// Acquire a resource identified by key in the deadlock detector
    /// Noop if deadlock_detection feature isn't enabled.
    ///
    /// # Safety
    ///
    /// Call after the resource is acquired
    #[inline]
    pub unsafe fn acquire_resource(_key: usize) {
        #[cfg(feature = "deadlock_detection")]
        deadlock_impl::acquire_resource(_key);
    }

    /// Release a resource identified by key in the deadlock detector.
    /// Noop if deadlock_detection feature isn't enabled.
    ///
    /// # Panics
    ///
    /// Panics if the resource was already released or wasn't acquired in this thread.
    ///
    /// # Safety
    ///
    /// Call before the resource is released
    #[inline]
    pub unsafe fn release_resource(_key: usize) {
        #[cfg(feature = "deadlock_detection")]
        deadlock_impl::release_resource(_key);
    }

    /// Returns all deadlocks detected *since* the last call.
    /// Each cycle consist of a vector of `DeadlockedThread`.
    #[cfg(feature = "deadlock_detection")]
    #[inline]
    pub fn check_deadlock() -> Vec<Vec<deadlock_impl::DeadlockedThread>> {
        deadlock_impl::check_deadlock()
    }

    #[inline]
    pub(super) unsafe fn on_unpark(_td: &super::ThreadData) {
        #[cfg(feature = "deadlock_detection")]
        deadlock_impl::on_unpark(_td);
    }
}

#[cfg(feature = "deadlock_detection")]
mod deadlock_impl {
    use super::{get_hashtable, lock_bucket, with_thread_data, ThreadData, NUM_THREADS};
    use crate::thread_parker::{ThreadParkerT, UnparkHandleT};
    use crate::word_lock::WordLock;
    use backtrace::Backtrace;
    use petgraph;
    use petgraph::graphmap::DiGraphMap;
    use std::cell::{Cell, UnsafeCell};
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use thread_id;

    /// Representation of a deadlocked thread
    pub struct DeadlockedThread {
        thread_id: usize,
        backtrace: Backtrace,
    }

    impl DeadlockedThread {
        /// The system thread id
        pub fn thread_id(&self) -> usize {
            self.thread_id
        }

        /// The thread backtrace
        pub fn backtrace(&self) -> &Backtrace {
            &self.backtrace
        }
    }

    pub struct DeadlockData {
        // Currently owned resources (keys)
        resources: UnsafeCell<Vec<usize>>,

        // Set when there's a pending callstack request
        deadlocked: Cell<bool>,

        // Sender used to report the backtrace
        backtrace_sender: UnsafeCell<Option<mpsc::Sender<DeadlockedThread>>>,

        // System thread id
        thread_id: usize,
    }

    impl DeadlockData {
        pub fn new() -> Self {
            DeadlockData {
                resources: UnsafeCell::new(Vec::new()),
                deadlocked: Cell::new(false),
                backtrace_sender: UnsafeCell::new(None),
                thread_id: thread_id::get(),
            }
        }
    }

    pub(super) unsafe fn on_unpark(td: &ThreadData) {
        if td.deadlock_data.deadlocked.get() {
            let sender = (*td.deadlock_data.backtrace_sender.get()).take().unwrap();
            sender
                .send(DeadlockedThread {
                    thread_id: td.deadlock_data.thread_id,
                    backtrace: Backtrace::new(),
                })
                .unwrap();
            // make sure to close this sender
            drop(sender);

            // park until the end of the time
            td.parker.prepare_park();
            td.parker.park();
            unreachable!("unparked deadlocked thread!");
        }
    }

    pub unsafe fn acquire_resource(key: usize) {
        with_thread_data(|thread_data| {
            (*thread_data.deadlock_data.resources.get()).push(key);
        });
    }

    pub unsafe fn release_resource(key: usize) {
        with_thread_data(|thread_data| {
            let resources = &mut (*thread_data.deadlock_data.resources.get());

            // There is only one situation where we can fail to find the
            // resource: we are currently running TLS destructors and our
            // ThreadData has already been freed. There isn't much we can do
            // about it at this point, so just ignore it.
            if let Some(p) = resources.iter().rposition(|x| *x == key) {
                resources.swap_remove(p);
            }
        });
    }

    pub fn check_deadlock() -> Vec<Vec<DeadlockedThread>> {
        unsafe {
            // fast pass
            if check_wait_graph_fast() {
                // double check
                check_wait_graph_slow()
            } else {
                Vec::new()
            }
        }
    }

    // Simple algorithm that builds a wait graph f the threads and the resources,
    // then checks for the presence of cycles (deadlocks).
    // This variant isn't precise as it doesn't lock the entire table before checking
    unsafe fn check_wait_graph_fast() -> bool {
        let table = get_hashtable();
        let thread_count = NUM_THREADS.load(Ordering::Relaxed);
        let mut graph = DiGraphMap::<usize, ()>::with_capacity(thread_count * 2, thread_count * 2);

        for b in &(*table).entries[..] {
            b.mutex.lock();
            let mut current = b.queue_head.get();
            while !current.is_null() {
                if !(*current).parked_with_timeout.get()
                    && !(*current).deadlock_data.deadlocked.get()
                {
                    // .resources are waiting for their owner
                    for &resource in &(*(*current).deadlock_data.resources.get()) {
                        graph.add_edge(resource, current as usize, ());
                    }
                    // owner waits for resource .key
                    graph.add_edge(current as usize, (*current).key.load(Ordering::Relaxed), ());
                }
                current = (*current).next_in_queue.get();
            }
            // SAFETY: We hold the lock here, as required
            b.mutex.unlock();
        }

        petgraph::algo::is_cyclic_directed(&graph)
    }

    #[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
    enum WaitGraphNode {
        Thread(*const ThreadData),
        Resource(usize),
    }

    use self::WaitGraphNode::*;

    // Contrary to the _fast variant this locks the entries table before looking for cycles.
    // Returns all detected thread wait cycles.
    // Note that once a cycle is reported it's never reported again.
    unsafe fn check_wait_graph_slow() -> Vec<Vec<DeadlockedThread>> {
        static DEADLOCK_DETECTION_LOCK: WordLock = WordLock::new();
        DEADLOCK_DETECTION_LOCK.lock();

        let mut table = get_hashtable();
        loop {
            // Lock all buckets in the old table
            for b in &table.entries[..] {
                b.mutex.lock();
            }

            // Now check if our table is still the latest one. Another thread could
            // have grown the hash table between us getting and locking the hash table.
            let new_table = get_hashtable();
            if new_table as *const _ == table as *const _ {
                break;
            }

            // Unlock buckets and try again
            for b in &table.entries[..] {
                // SAFETY: We hold the lock here, as required
                b.mutex.unlock();
            }

            table = new_table;
        }

        let thread_count = NUM_THREADS.load(Ordering::Relaxed);
        let mut graph =
            DiGraphMap::<WaitGraphNode, ()>::with_capacity(thread_count * 2, thread_count * 2);

        for b in &table.entries[..] {
            let mut current = b.queue_head.get();
            while !current.is_null() {
                if !(*current).parked_with_timeout.get()
                    && !(*current).deadlock_data.deadlocked.get()
                {
                    // .resources are waiting for their owner
                    for &resource in &(*(*current).deadlock_data.resources.get()) {
                        graph.add_edge(Resource(resource), Thread(current), ());
                    }
                    // owner waits for resource .key
                    graph.add_edge(
                        Thread(current),
                        Resource((*current).key.load(Ordering::Relaxed)),
                        (),
                    );
                }
                current = (*current).next_in_queue.get();
            }
        }

        for b in &table.entries[..] {
            // SAFETY: We hold the lock here, as required
            b.mutex.unlock();
        }

        // find cycles
        let cycles = graph_cycles(&graph);

        let mut results = Vec::with_capacity(cycles.len());

        for cycle in cycles {
            let (sender, receiver) = mpsc::channel();
            for td in cycle {
                let bucket = lock_bucket((*td).key.load(Ordering::Relaxed));
                (*td).deadlock_data.deadlocked.set(true);
                *(*td).deadlock_data.backtrace_sender.get() = Some(sender.clone());
                let handle = (*td).parker.unpark_lock();
                // SAFETY: We hold the lock here, as required
                bucket.mutex.unlock();
                // unpark the deadlocked thread!
                // on unpark it'll notice the deadlocked flag and report back
                handle.unpark();
            }
            // make sure to drop our sender before collecting results
            drop(sender);
            results.push(receiver.iter().collect());
        }

        DEADLOCK_DETECTION_LOCK.unlock();

        results
    }

    // normalize a cycle to start with the "smallest" node
    fn normalize_cycle<T: Ord + Copy + Clone>(input: &[T]) -> Vec<T> {
        let min_pos = input
            .iter()
            .enumerate()
            .min_by_key(|&(_, &t)| t)
            .map(|(p, _)| p)
            .unwrap_or(0);
        input
            .iter()
            .cycle()
            .skip(min_pos)
            .take(input.len())
            .cloned()
            .collect()
    }

    // returns all thread cycles in the wait graph
    fn graph_cycles(g: &DiGraphMap<WaitGraphNode, ()>) -> Vec<Vec<*const ThreadData>> {
        use petgraph::visit::depth_first_search;
        use petgraph::visit::DfsEvent;
        use petgraph::visit::NodeIndexable;

        let mut cycles = HashSet::new();
        let mut path = Vec::with_capacity(g.node_bound());
        // start from threads to get the correct threads cycle
        let threads = g
            .nodes()
            .filter(|n| if let &Thread(_) = n { true } else { false });

        depth_first_search(g, threads, |e| match e {
            DfsEvent::Discover(Thread(n), _) => path.push(n),
            DfsEvent::Finish(Thread(_), _) => {
                path.pop();
            }
            DfsEvent::BackEdge(_, Thread(n)) => {
                let from = path.iter().rposition(|&i| i == n).unwrap();
                cycles.insert(normalize_cycle(&path[from..]));
            }
            _ => (),
        });

        cycles.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{ThreadData, DEFAULT_PARK_TOKEN, DEFAULT_UNPARK_TOKEN};
    use std::{
        ptr,
        sync::{
            atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    };

    /// Calls a closure for every `ThreadData` currently parked on a given key
    fn for_each(key: usize, mut f: impl FnMut(&ThreadData)) {
        let mut bucket = super::lock_bucket(key);

        let mut current: *const ThreadData = bucket.queue().head;
        while !current.is_null() {
            let current_ref = unsafe { &*current };
            if current_ref.key.load(Ordering::Relaxed) == key {
                f(current_ref);
            }
            current = current_ref.next_in_queue.get();
        }
    }

    macro_rules! test {
        ( $( $name:ident(
            repeats: $repeats:expr,
            latches: $latches:expr,
            delay: $delay:expr,
            threads: $threads:expr,
            single_unparks: $single_unparks:expr);
        )* ) => {
            $(#[test]
            fn $name() {
                let delay = Duration::from_micros($delay);
                for _ in 0..$repeats {
                    run_parking_test($latches, delay, $threads, $single_unparks);
                }
            })*
        };
    }

    test! {
        unpark_all_one_fast(
            repeats: 10000, latches: 1, delay: 0, threads: 1, single_unparks: 0
        );
        unpark_all_hundred_fast(
            repeats: 100, latches: 1, delay: 0, threads: 100, single_unparks: 0
        );
        unpark_one_one_fast(
            repeats: 1000, latches: 1, delay: 0, threads: 1, single_unparks: 1
        );
        unpark_one_hundred_fast(
            repeats: 20, latches: 1, delay: 0, threads: 100, single_unparks: 100
        );
        unpark_one_fifty_then_fifty_all_fast(
            repeats: 50, latches: 1, delay: 0, threads: 100, single_unparks: 50
        );
        unpark_all_one(
            repeats: 100, latches: 1, delay: 10000, threads: 1, single_unparks: 0
        );
        unpark_all_hundred(
            repeats: 100, latches: 1, delay: 10000, threads: 100, single_unparks: 0
        );
        unpark_one_one(
            repeats: 10, latches: 1, delay: 10000, threads: 1, single_unparks: 1
        );
        unpark_one_fifty(
            repeats: 1, latches: 1, delay: 10000, threads: 50, single_unparks: 50
        );
        unpark_one_fifty_then_fifty_all(
            repeats: 2, latches: 1, delay: 10000, threads: 100, single_unparks: 50
        );
        hundred_unpark_all_one_fast(
            repeats: 100, latches: 100, delay: 0, threads: 1, single_unparks: 0
        );
        hundred_unpark_all_one(
            repeats: 1, latches: 100, delay: 10000, threads: 1, single_unparks: 0
        );
    }

    fn run_parking_test(
        num_latches: usize,
        delay: Duration,
        num_threads: usize,
        num_single_unparks: usize,
    ) {
        let mut tests = Vec::with_capacity(num_latches);

        for _ in 0..num_latches {
            let test = Arc::new(SingleLatchTest::new(num_threads));
            let mut threads = Vec::with_capacity(num_threads);
            for _ in 0..num_threads {
                let test = test.clone();
                threads.push(thread::spawn(move || test.run()));
            }
            tests.push((test, threads));
        }

        for unpark_index in 0..num_single_unparks {
            thread::sleep(delay);
            for (test, _) in &tests {
                test.unpark_one(unpark_index);
            }
        }

        for (test, threads) in tests {
            test.finish(num_single_unparks);
            for thread in threads {
                thread.join().expect("Test thread panic");
            }
        }
    }

    struct SingleLatchTest {
        semaphore: AtomicIsize,
        num_awake: AtomicUsize,
        /// Holds the pointer to the last *unprocessed* woken up thread.
        last_awoken: AtomicPtr<ThreadData>,
        /// Total number of threads participating in this test.
        num_threads: usize,
    }

    impl SingleLatchTest {
        pub fn new(num_threads: usize) -> Self {
            Self {
                // This implements a fair (FIFO) semaphore, and it starts out unavailable.
                semaphore: AtomicIsize::new(0),
                num_awake: AtomicUsize::new(0),
                last_awoken: AtomicPtr::new(ptr::null_mut()),
                num_threads,
            }
        }

        pub fn run(&self) {
            // Get one slot from the semaphore
            self.down();

            // Report back to the test verification code that this thread woke up
            let this_thread_ptr = super::with_thread_data(|t| t as *const _ as *mut _);
            self.last_awoken.store(this_thread_ptr, Ordering::SeqCst);
            self.num_awake.fetch_add(1, Ordering::SeqCst);
        }

        pub fn unpark_one(&self, single_unpark_index: usize) {
            // last_awoken should be null at all times except between self.up() and at the bottom
            // of this method where it's reset to null again
            assert!(self.last_awoken.load(Ordering::SeqCst).is_null());

            let mut queue: Vec<*mut ThreadData> = Vec::with_capacity(self.num_threads);
            for_each(self.semaphore_addr(), |thread_data| {
                queue.push(thread_data as *const _ as *mut _);
            });
            assert!(queue.len() <= self.num_threads - single_unpark_index);

            let num_awake_before_up = self.num_awake.load(Ordering::SeqCst);

            self.up();

            // Wait for a parked thread to wake up and update num_awake + last_awoken.
            while self.num_awake.load(Ordering::SeqCst) != num_awake_before_up + 1 {
                thread::yield_now();
            }

            // At this point the other thread should have set last_awoken inside the run() method
            let last_awoken = self.last_awoken.load(Ordering::SeqCst);
            assert!(!last_awoken.is_null());
            if !queue.is_empty() && queue[0] != last_awoken {
                panic!(
                    "Woke up wrong thread:\n\tqueue: {:?}\n\tlast awoken: {:?}",
                    queue, last_awoken
                );
            }
            self.last_awoken.store(ptr::null_mut(), Ordering::SeqCst);
        }

        pub fn finish(&self, num_single_unparks: usize) {
            // The amount of threads not unparked via unpark_one
            let mut num_threads_left = self.num_threads.checked_sub(num_single_unparks).unwrap();

            // Wake remaining threads up with unpark_all. Has to be in a loop, because there might
            // still be threads that has not yet parked.
            while num_threads_left > 0 {
                let mut num_waiting_on_address = 0;
                for_each(self.semaphore_addr(), |_thread_data| {
                    num_waiting_on_address += 1;
                });
                assert!(num_waiting_on_address <= num_threads_left);

                let num_awake_before_unpark = self.num_awake.load(Ordering::SeqCst);

                let num_unparked =
                    unsafe { super::unpark_all(self.semaphore_addr(), DEFAULT_UNPARK_TOKEN) };
                assert!(num_unparked >= num_waiting_on_address);
                assert!(num_unparked <= num_threads_left);

                // Wait for all unparked threads to wake up and update num_awake + last_awoken.
                while self.num_awake.load(Ordering::SeqCst)
                    != num_awake_before_unpark + num_unparked
                {
                    thread::yield_now()
                }

                num_threads_left = num_threads_left.checked_sub(num_unparked).unwrap();
            }
            // By now, all threads should have been woken up
            assert_eq!(self.num_awake.load(Ordering::SeqCst), self.num_threads);

            // Make sure no thread is parked on our semaphore address
            let mut num_waiting_on_address = 0;
            for_each(self.semaphore_addr(), |_thread_data| {
                num_waiting_on_address += 1;
            });
            assert_eq!(num_waiting_on_address, 0);
        }

        pub fn down(&self) {
            let old_semaphore_value = self.semaphore.fetch_sub(1, Ordering::SeqCst);

            if old_semaphore_value > 0 {
                // We acquired the semaphore. Done.
                return;
            }

            // We need to wait.
            let validate = || true;
            let before_sleep = || {};
            let timed_out = |_, _| {};
            unsafe {
                super::park(
                    self.semaphore_addr(),
                    validate,
                    before_sleep,
                    timed_out,
                    DEFAULT_PARK_TOKEN,
                    None,
                );
            }
        }

        pub fn up(&self) {
            let old_semaphore_value = self.semaphore.fetch_add(1, Ordering::SeqCst);

            // Check if anyone was waiting on the semaphore. If they were, then pass ownership to them.
            if old_semaphore_value < 0 {
                // We need to continue until we have actually unparked someone. It might be that
                // the thread we want to pass ownership to has decremented the semaphore counter,
                // but not yet parked.
                loop {
                    match unsafe {
                        super::unpark_one(self.semaphore_addr(), |_| DEFAULT_UNPARK_TOKEN)
                            .unparked_threads
                    } {
                        1 => break,
                        0 => (),
                        i => panic!("Should not wake up {} threads", i),
                    }
                }
            }
        }

        fn semaphore_addr(&self) -> usize {
            &self.semaphore as *const _ as usize
        }
    }
}
