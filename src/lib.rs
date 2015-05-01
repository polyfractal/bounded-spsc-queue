
#![feature(core, alloc, box_syntax, optin_builtin_traits)]

extern crate core;
extern crate alloc;

#[cfg(feature = "benchmark")] extern crate criterion;
#[cfg(feature = "benchmark")] extern crate time;


use alloc::heap::{allocate, deallocate};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use core::{mem, ptr};
use core::mem::transmute;



/// The internal memory buffer used by the queue.
///
/// Buffer holds a pointer to allocated memory which represents the bounded
/// ring buffer, as well as a head and tail atomicUsize which the producer and consumer
/// use to track location in the ring.
#[repr(C)]
pub struct Buffer<T> {
    /// A pointer to the allocated ring buffer
    buffer: *mut T,

    /// The bounded size as specified by the user.  If the queue reaches capacity, it will block
    /// until values are poppped off.
    capacity: usize,

    /// The allocated size of the ring buffer, in terms of number of values (not physical memory).
    /// This will be the next power of two larger than `capacity`
    allocated_size: usize,

    /// Index position of the current head
    head: AtomicUsize,

    /// Padding to prevent false sharing beween cache lines
    _padding: [u64;8],

    /// Index position of current tail
    tail: AtomicUsize
}

unsafe impl<T: Sync> Sync for Buffer<T> { }

/// A handle to the queue which allows consuming values from the buffer
pub struct Consumer<T> {
    buffer: Arc<Buffer<T>>,
}

/// A handle to the queue which allows adding values onto the buffer
pub struct Producer<T> {
    buffer: Arc<Buffer<T>>,
}

unsafe impl<T: Send> Send for Consumer<T> { }
unsafe impl<T: Send> Send for Producer<T> { }

impl<T> !Sync for Consumer<T> {}
impl<T> !Sync for Producer<T> {}

impl<T> Buffer<T> {

    /// Attempt to pop a value off the buffer.
    ///
    /// If the buffer is empty, this method will not block.  Instead, it will return `None`
    /// signifying the buffer was empty.  The caller may then decide what to do next (e.g. spin-wait,
    /// sleep, process something else, etc)
    ///
    /// # Examples
    ///
    /// ```
    /// // Attempt to pop off a value
    /// let t = buffer.try_pop();
    /// match t {
    ///   Some(v) => {}, // Got a value
    ///   None => {}     // Buffer empty, try again later
    /// }
    /// ```
    pub fn try_pop(&self) -> Option<T> {
        let current_head = self.head.load(Ordering::Acquire);

        if current_head == self.tail.load(Ordering::Acquire) {
            None
        } else {
            let v = unsafe { ptr::read(self.load(current_head as isize)) };
            self.head.fetch_add(1, Ordering::Release);
            Some(v)
        }

    }

    /// Pop a value off the buffer.
    ///
    /// This method will block until the buffer is non-empty.  The waiting strategy is a simple
    /// spin-wait and will repeatedly call `try_pop()` until a value is available.  If you do not
    /// want a spin-wait burning CPU, you should call `try_pop()` directly and implement a different
    /// waiting strategy.
    ///
    /// # Examples
    ///
    /// ```
    /// // Block until a value is ready
    /// let t = buffer.pop();
    /// ```
    pub fn pop(&self) -> T {
        loop {
            match self.try_pop()  {
                None => {},
                Some(v) => return v
            }
        }
    }

    /// Attempt to push a value onto the buffer.
    ///
    /// If the buffer is full, this method will not block.  Instead, it will return `Some(v)`, where
    /// `v` was the value attempting to be pushed onto the buffer.  If the value was successfully
    /// pushed onto the buffer, `None` will be returned signifying success.
    ///
    /// # Examples
    ///
    /// ```
    /// // Attempt to push a value onto the buffer
    /// let t = buffer.try_push(123);
    /// match t {
    ///   Some(v) => {}, // Buffer was full, try again later
    ///   None => {}     // Value was successfully pushed onto the buffer
    /// }
    /// ```
    pub fn try_push(&self, v: T) -> Option<T> {
        let current_tail = self.tail.load(Ordering::Acquire);

        return if self.head.load(Ordering::Acquire) + self.capacity <= current_tail {
            Some(v)
        } else {
            unsafe { self.store((current_tail) as isize, v); }
            self.tail.fetch_add(1, Ordering::Release);
            None
        }
    }

    /// Push a value onto the buffer.
    ///
    /// This method will block until the buffer is non-full.  The waiting strategy is a simple
    /// spin-wait and will repeatedly call `try_push()` until the value can be added.  If you do not
    /// want a spin-wait burning CPU, you should call `try_push()` directly and implement a different
    /// waiting strategy.
    ///
    /// # Examples
    ///
    /// ```
    /// // Block until we can push this value onto the buffer
    /// buffer.try_push(123);
    /// ```
    pub fn push(&self, v: T) {
        let mut t = v;
        loop {
            match self.try_push(t) {
                Some(rv) => t = rv,
                None => return
            }
        }
    }

    /// Load a value out of the buffer
    ///
    /// # Safety
    ///
    /// This method assumes the caller has:
    /// - Initialized a valid block of memory
    /// - Specified an index position that contains valid data
    ///
    /// The caller can use either absolute or monotonically increasing index positions, since
    /// buffer wrapping is handled inside the method.
    #[inline]
    unsafe fn load(&self, pos: isize) -> &T {
        transmute(self.buffer.offset(pos & (self.allocated_size as isize - 1)))
    }

    /// Store a value in the buffer
    ///
    /// # Safety
    ///
    /// This method assumes the caller has:
    /// - Initialized a valid block of memory
    #[inline]
    unsafe fn store(&self, pos: isize, v: T) {
        let end = self.buffer.offset(pos & (self.allocated_size as isize - 1));
        ptr::write(&mut *end, v);
    }
}

/// Handles deallocation of heap memory when the buffer is dropped
impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {

        // Pop the rest of the values off the queue.  By moving them into this scope,
        // we implicitly call their destructor

        // TODO this could be optimized to avoid the atomic operations / book-keeping...but
        // since this is the destructor, there shouldn't be any contention... so meh?
        loop {
            match self.try_pop() {
                Some(_) => {},  // Got a value, keep poppin!
                None => break   // All done, deallocate mem now
            }
        }

        unsafe {
            deallocate(self.buffer as *mut u8,
                self.allocated_size * mem::size_of::<T>(),
                mem::min_align_of::<T>());
        }
    }
}

/// Creates a new SPSC Queue, returning a Producer and Consumer handle
///
/// Capacity specifies the size of the bounded queue to create.  Actual memory usage
/// will be `capacity.next_power_of_two() * size_of::<T>()`, since ringbuffers with
/// power of two sizes are more efficient to operate on (can use a bitwise AND to index
/// into the ring instead of a more expensive modulo operator).
///
/// # Examples
///
/// Here is a simple usage of make, using the queue within the same thread:
///
/// ```
/// // Create a queue with capacity to hold 100 values
/// let (p, c) = make(100);
///
/// // Push `123` onto the queue
/// p.push(123);
///
/// // Pop the value back off
/// let t = c.pop();
/// assert!(t == 123);
/// ```
///
/// Of course, a SPSC queue is really only useful if you plan to use it in a multi-threaded
/// environment.  The Producer and Consumer can both be sent to a thread, providing a fast, bounded
/// one-way communication channel between those threads:
///
/// ```
/// use std::thread;
///
/// let (p, c) = make(500);
///
/// // Spawn a new thread and move the Producer into it
/// thread::spawn(move|| {
///   for i in 0..100000 {
///     p.push(i as u32);
///   }
/// });
///
/// // Back in the first thread, start Pop'ing values off the queue
/// for i in 0..100000 {
///   let t = c.pop();
///   assert!(t == i);
/// }
///
/// ```
///
/// # Panics
///
/// If the requested queue size is larger than available memory (e.g.
/// `capacity.next_power_of_two() * size_of::<T>() > available memory` ), this function will abort
/// with an OOM panic.
pub fn make<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {

    let ptr = unsafe { allocate_buffer(capacity) };

    let arc = Arc::new(Buffer{
        buffer: ptr,
        capacity: capacity,
        allocated_size: capacity.next_power_of_two(),
        head: AtomicUsize::new(0),
        _padding: [0; 8],
        tail: AtomicUsize::new(0)
    });

    (Producer { buffer: arc.clone() }, Consumer { buffer: arc.clone() })
}

/// Allocates a memory buffer on the heap and returns a pointer to it
unsafe fn allocate_buffer<T>(capacity: usize) -> *mut T {
    let adjusted_size = capacity.next_power_of_two();
    let size = adjusted_size.checked_mul(mem::size_of::<T>())
                .expect("capacity overflow");

    let ptr = allocate(size, mem::min_align_of::<T>()) as *mut T;
    if ptr.is_null() { ::alloc::oom() }
    ptr
}

impl<T> Producer<T> {

    /// Push a value onto the buffer.
    ///
    /// If the buffer is non-full, the operation will execute immediately.  If the buffer is full,
    /// this method will block until the buffer is non-full.  The waiting strategy is a simple
    /// spin-wait. If you do not want a spin-wait burning CPU, you should call `try_push()`
    /// directly and implement a different waiting strategy.
    ///
    /// # Examples
    ///
    /// ```
    /// let (producer, _) = make(100);
    ///
    /// // Block until we can push this value onto the queue
    /// producer.push(123);
    /// ```
    pub fn push(&self, v: T) {
        (*self.buffer).push(v);
    }

    /// Attempt to push a value onto the buffer.
    ///
    /// This method does not block.  If the queue is not full, the value will be added to the
    /// queue and the method will return `None`, signifying success.  If the queue is full,
    /// this method will return `Some(v)``, where `v` is your original value.
    ///
    /// # Examples
    ///
    /// ```
    /// let (producer, _) = make(100);
    ///
    /// // Attempt to add this value to the queue
    /// match producer.try push(123) {
    ///     Some(v) => {}, // Queue full, try again later
    ///     None => {}     // Value added to queue
    /// }
    /// ```
    pub fn try_push(&self, v: T) -> Option<T> {
        (*self.buffer).try_push(v)
    }

    /// Returns the total capacity of this queue
    ///
    /// This value represents the total capacity of the queue when it is full.  It does not
    /// represent the current usage.  For that, call `size()`.
    ///
    /// # Examples
    ///
    /// ```
    /// let (producer, _) = make(100);
    ///
    /// assert!(producer.capacity() == 100);
    /// producer.push(123);
    /// assert!(producer.capacity() == 100);
    /// ```
    pub fn capacity(&self) -> usize {
        (*self.buffer).capacity
    }

    /// Returns the current size of the queue
    ///
    /// This value represents the current size of the queue.  This value can be from 0-`capacity`
    /// inclusive.
    ///
    /// # Examples
    ///
    /// ```
    /// let (producer, _) = make(100);
    ///
    /// assert!(producer.size() == 0);
    /// producer.push(123);
    /// assert!(producer.size() == 1);
    /// ```
    pub fn size(&self) -> usize {
        (*self.buffer).tail.load(Ordering::Acquire) - (*self.buffer).head.load(Ordering::Acquire)
    }

    /// Returns the available space in the queue
    ///
    /// This value represents the number of items that can be pushed onto the queue before it
    /// becomes full.
    ///
    /// # Examples
    ///
    /// ```
    /// let (producer, _) = make(100);
    ///
    /// assert!(producer.free_space() == 100);
    /// producer.push(123);
    /// assert!(producer.free_space() == 99);
    /// ```
    pub fn free_space(&self) -> usize {
        self.capacity() - self.size()
    }

}

impl<T> Consumer<T> {

    /// Pop a value off the queue.
    ///
    /// If the buffer contains values, this method will execute immediately and return a value.
    /// If the buffer is empty, this method will block until a value becomes available.  The
    /// waiting strategy is a simple spin-wait. If you do not want a spin-wait burning CPU, you
    /// should call `try_push()` directly and implement a different waiting strategy.
    ///
    /// # Examples
    ///
    /// ```
    /// let (_, consumer) = make(100);
    ///
    /// // Block until a value becomes available
    /// let t = consumer.pop();
    /// ```
    pub fn pop(&self) -> T {
        (*self.buffer).pop()
    }

    /// Attempt to pop a value off the queue.
    ///
    /// This method does not block.  If the queue is empty, the method will return `None`.  If
    /// there is a value available, the method will return `Some(v)`, where `v` is the value
    /// being popped off the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use bounded_spsc_queue::*;
    ///
    /// let (_, consumer) = make(100);
    ///
    /// // Attempt to pop a value off the queue
    /// let t = consumer.try_pop();
    /// match t {
    ///     Some(v) => {},      // Successfully popped a value
    ///     None => {}          // Queue empty, try again later
    /// }
    /// ```
    pub fn try_pop(&self) -> Option<T> {
        (*self.buffer).try_pop()
    }

    /// Returns the total capacity of this queue
    ///
    /// This value represents the total capacity of the queue when it is full.  It does not
    /// represent the current usage.  For that, call `size()`.
    ///
    /// # Examples
    ///
    /// ```
    /// let (_, consumer) = make(100);
    ///
    /// assert!(consumer.capacity() == 100);
    /// let t = consumer.pop();
    /// assert!(producer.capacity() == 100);
    /// ```
    pub fn capacity(&self) -> usize {
        (*self.buffer).capacity
    }

    /// Returns the current size of the queue
    ///
    /// This value represents the current size of the queue.  This value can be from 0-`capacity`
    /// inclusive.
    ///
    /// # Examples
    ///
    /// ```
    /// let (_, consumer) = make(100);
    ///
    /// //... producer pushes somewhere ...
    ///
    /// assert!(consumer.size() == 10);
    /// consumer.pop();
    /// assert!(producer.size() == 9);
    /// ```
    pub fn size(&self) -> usize {
        (*self.buffer).tail.load(Ordering::Acquire) - (*self.buffer).head.load(Ordering::Acquire)
    }

}



#[cfg(test)]
mod tests {

    use super::*;
    use std::thread;

    #[cfg(feature = "benchmark")] use std::sync::mpsc::sync_channel;
    #[cfg(feature = "benchmark")] use criterion::{Bencher, Criterion};
    #[cfg(feature = "benchmark")] use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(feature = "benchmark")] use std::sync::Arc;
    //#[cfg(feature = "benchmark")] use std::time::Duration;

    #[test]
    fn test_producer_push() {
        let (p, _) = super::make(10);

        for i in 0..9 {
            p.push(i);
            assert!(p.capacity() == 10);
            assert!(p.size() == i + 1);
        }
    }

    #[test]
    fn test_consumer_pop() {
        let (p, c) = super::make(10);

        for i in 0..9 {
            p.push(i);
            assert!(p.capacity() == 10);
            assert!(p.size() == i + 1);
        }

        for i in 0..9 {
            assert!(c.size() == 9 - i);
            let t = c.pop();
            assert!(c.capacity() == 10);
            assert!(c.size() == 9 - i - 1);
            assert!(t == i);
        }
    }

    #[test]
    fn test_try_push() {
        let (p, _) = super::make(10);

        for i in 0..10 {
            p.push(i);
            assert!(p.capacity() == 10);
            assert!(p.size() == i + 1);
        }

        match p.try_push(10) {
            Some(v) => {
                assert!(v == 10);
            },
            None => assert!(false, "Queue should not have accepted another write!")
        }
    }

    #[test]
    fn test_try_poll() {
        let (p, c) = super::make(10);

        match c.try_pop() {
            Some(_) => {
                assert!(false, "Queue was empty but a value was read!")
            },
            None => {}
        }

        p.push(123);

        match c.try_pop() {
            Some(v) => assert!(v == 123),
            None => assert!(false, "Queue was not empty but poll() returned nothing!")
        }

        match c.try_pop() {
            Some(_) => {
                assert!(false, "Queue was empty but a value was read!")
            },
            None => {}
        }
    }

    #[test]
    fn test_threaded() {
        let (p, c) = super::make(500);

        thread::spawn(move|| {
            for i in 0..100000 {
                p.push(i);
            }
        });

        for i in 0..100000 {
            let t = c.pop();
            assert!(t == i);
        }
    }

    #[cfg(feature = "benchmark")]
    fn bench_chan(b: &mut Bencher) {
        let (tx, rx) = sync_channel::<u8>(500);
        b.iter(|| {
            tx.send(1);
            rx.recv().unwrap()
        });
    }

    #[cfg(feature = "benchmark")]
    fn bench_chan_threaded(b: &mut Bencher) {
        let (tx, rx) = sync_channel::<u8>(500);
        let flag = AtomicBool::new(false);
        let arc_flag = Arc::new(flag);

        let flag_clone = arc_flag.clone();
        thread::spawn(move|| {
            while flag_clone.load(Ordering::Acquire) == false {
                // Try to do as much work as possible without checking the atomic
                for _ in 0..400 {
                    rx.recv().unwrap();
                }
            }
        });

        b.iter(|| {
            tx.send(1)
        });

        let flag_clone = arc_flag.clone();
        flag_clone.store(true, Ordering::Release);

        // We have to loop a minimum of 400 times to guarantee the other thread shuts down
        for _ in 0..400 {
            tx.send(1);
        }
    }

    #[cfg(feature = "benchmark")]
    fn bench_chan_threaded2(b: &mut Bencher) {
        let (tx, rx) = sync_channel::<u8>(500);
        let flag = AtomicBool::new(false);
        let arc_flag = Arc::new(flag);

        let flag_clone = arc_flag.clone();
        thread::spawn(move|| {
            while flag_clone.load(Ordering::Acquire) == false {
                // Try to do as much work as possible without checking the atomic
                for _ in 0..400 {
                    tx.send(1);
                }
            }
        });

        b.iter(|| {
            rx.recv().unwrap()
        });

        let flag_clone = arc_flag.clone();
        flag_clone.store(true, Ordering::Release);

        // We have to loop a minimum of 400 times to guarantee the other thread shuts down
        for _ in 0..400 {
            rx.try_recv();
        }
    }

    #[cfg(feature = "benchmark")]
    fn bench_spsc(b: &mut Bencher) {
        let (p, c) = super::make(500);

        b.iter(|| {
            p.push(1);
            c.pop()
        });
    }

    #[cfg(feature = "benchmark")]
    fn bench_spsc_threaded(b: &mut Bencher) {
        let (p, c) = super::make(500);

        let flag = AtomicBool::new(false);
        let arc_flag = Arc::new(flag);

        let flag_clone = arc_flag.clone();
        thread::spawn(move|| {
            while flag_clone.load(Ordering::Acquire) == false {

                // Try to do as much work as possible without checking the atomic
                for _ in 0..400 {
                    c.pop();
                }
            }
        });

        b.iter(|| {
            p.push(1)
        });

        let flag_clone = arc_flag.clone();
        flag_clone.store(true, Ordering::Release);

        // We have to loop a minimum of 400 times to guarantee the other thread shuts down
        for _ in 0..400 {
            p.try_push(1);
        }
    }

    #[cfg(feature = "benchmark")]
    fn bench_spsc_threaded2(b: &mut Bencher) {
        let (p, c) = super::make(500);

        let flag = AtomicBool::new(false);
        let arc_flag = Arc::new(flag);

        let flag_clone = arc_flag.clone();
        thread::spawn(move|| {
            while flag_clone.load(Ordering::Acquire) == false {

                // Try to do as much work as possible without checking the atomic
                for _ in 0..400 {
                    p.push(1);
                }
            }
        });

        b.iter(|| {
            c.pop()
        });

        let flag_clone = arc_flag.clone();
        flag_clone.store(true, Ordering::Release);

        // We have to loop a minimum of 400 times to guarantee the other thread shuts down
        for _ in 0..400 {
            c.try_pop();
        }
    }

    #[cfg(feature = "benchmark")]
    #[test]
    fn bench_single_thread() {
        Criterion::default()
            .bench("bench_chan", bench_chan);

        Criterion::default()
            .bench("bench_spsc", bench_spsc);
    }

    #[cfg(feature = "benchmark")]
    #[test]
    fn bench_threaded() {
        Criterion::default()
            .bench("bench_chan", bench_chan_threaded);

        Criterion::default()
            .bench("bench_spsc", bench_spsc_threaded);
    }

    #[cfg(feature = "benchmark")]
    #[test]
    fn bench_threaded_reverse() {
        Criterion::default()
            //.warm_up_time(Duration::seconds(10))
            //.measurement_time(Duration::seconds(100))
            //.sample_size(100)
            //.nresamples(500000)
            .bench("bench_chan", bench_chan_threaded2);

        Criterion::default()
            //.warm_up_time(Duration::seconds(10))
            //.measurement_time(Duration::seconds(100))
            //.sample_size(100)
            //.nresamples(500000)
            .bench("bench_spsc", bench_spsc_threaded2);
    }
}
