//! A fixed capacity Multiple-Producer Multiple-Consumer (MPMC) lock-free queue
//!
//! NOTE: This module is not available on targets that do *not* support CAS operations, e.g. ARMv6-M
//!
//! # Example
//!
//! This queue can be constructed in "const context". Placing it in a `static` variable lets *all*
//! contexts (interrupts / threads / `main`) safely enqueue and dequeue items from it.
//!
//! ``` ignore
//! #![no_main]
//! #![no_std]
//!
//! use panic_semihosting as _;
//!
//! use cortex_m::{asm, peripheral::syst::SystClkSource};
//! use cortex_m_rt::{entry, exception};
//! use cortex_m_semihosting::hprintln;
//! use heapless::mpmc::Q2;
//!
//! static Q: Q2<u8> = Q2::new();
//!
//! #[entry]
//! fn main() -> ! {
//!     if let Some(p) = cortex_m::Peripherals::take() {
//!         let mut syst = p.SYST;
//!
//!         // configures the system timer to trigger a SysTick exception every second
//!         syst.set_clock_source(SystClkSource::Core);
//!         syst.set_reload(12_000_000);
//!         syst.enable_counter();
//!         syst.enable_interrupt();
//!     }
//!
//!     loop {
//!         if let Some(x) = Q.dequeue() {
//!             hprintln!("{}", x).ok();
//!         } else {
//!             asm::wfi();
//!         }
//!     }
//! }
//!
//! #[exception]
//! fn SysTick() {
//!     static mut COUNT: u8 = 0;
//!
//!     Q.enqueue(*COUNT).ok();
//!     *COUNT += 1;
//! }
//! ```
//!
//! # Benchmark
//!
//! Measured on a ARM Cortex-M3 core running at 8 MHz and with zero Flash wait cycles
//!
//! N| `Q8::<u8>::enqueue().ok()` (`z`) | `Q8::<u8>::dequeue()` (`z`) |
//! -|----------------------------------|-----------------------------|
//! 0|34                                |35                           |
//! 1|52                                |53                           |
//! 2|69                                |71                           |
//!
//! - `N` denotes the number of *interruptions*. On Cortex-M, an interruption consists of an
//!   interrupt handler preempting the would-be atomic section of the `enqueue` / `dequeue`
//!   operation. Note that it does *not* matter if the higher priority handler uses the queue or
//!   not.
//! - All execution times are in clock cycles. 1 clock cycle = 125 ns.
//! - Execution time is *dependent* of `mem::size_of::<T>()`. Both operations include one
//! `memcpy(T)` in their successful path.
//! - The optimization level is indicated in parentheses.
//! - The numbers reported correspond to the successful path (i.e. `Some` is returned by `dequeue`
//! and `Ok` is returned by `enqueue`).
//!
//! # Portability
//!
//! This module is not exposed to architectures that lack the instructions to implement CAS loops.
//! Those architectures include ARMv6-M (`thumbv6m-none-eabi`) and MSP430 (`msp430-none-elf`).
//!
//! # References
//!
//! This is an implementation of Dmitry Vyukov's ["Bounded MPMC queue"][0] minus the cache padding.
//!
//! [0]: http://www.1024cores.net/home/lock-free-algorithms/queues/bounded-mpmc-queue

use core::{cell::UnsafeCell, mem::MaybeUninit};

#[cfg(armv6m)]
use atomic_polyfill::{AtomicUsize, Ordering};

#[cfg(not(armv6m))]
use core::sync::atomic::{AtomicUsize, Ordering};

/// MPMC queue with a capability for 2 elements.
pub type Q2<T> = MpMcQueue<T, 2>;

/// MPMC queue with a capability for 4 elements.
pub type Q4<T> = MpMcQueue<T, 4>;

/// MPMC queue with a capability for 8 elements.
pub type Q8<T> = MpMcQueue<T, 8>;

/// MPMC queue with a capability for 16 elements.
pub type Q16<T> = MpMcQueue<T, 16>;

/// MPMC queue with a capability for 32 elements.
pub type Q32<T> = MpMcQueue<T, 32>;

/// MPMC queue with a capability for 64 elements.
pub type Q64<T> = MpMcQueue<T, 64>;

/// MPMC queue with a capacity for N elements
pub struct MpMcQueue<T, const N: usize> {
    buffer: UnsafeCell<[Cell<T>; N]>,
    dequeue_pos: AtomicUsize,
    enqueue_pos: AtomicUsize,
}

impl<T, const N: usize> MpMcQueue<T, N> {
    const MASK: usize = N - 1;
    const EMPTY_CELL: Cell<T> = Cell::new(0);

    /// Creates an empty queue
    pub const fn new() -> Self {
        let mut cell_count = 0;

        let mut result_cells: [Cell<T>; N] = [Self::EMPTY_CELL; N];
        while cell_count != N {
            result_cells[cell_count] = Cell::new(cell_count);
            cell_count += 1;
        }

        Self {
            buffer: UnsafeCell::new(result_cells),
            dequeue_pos: AtomicUsize::new(0),
            enqueue_pos: AtomicUsize::new(0),
        }
    }

    /// Returns the item in the front of the queue, or `None` if the queue is empty
    pub fn dequeue(&self) -> Option<T> {
        unsafe { dequeue(self.buffer.get() as *mut _, &self.dequeue_pos, Self::MASK) }
    }

    /// Adds an `item` to the end of the queue
    ///
    /// Returns back the `item` if the queue is full
    pub fn enqueue(&self, item: T) -> Result<(), T> {
        unsafe {
            enqueue(
                self.buffer.get() as *mut _,
                &self.enqueue_pos,
                Self::MASK,
                item,
            )
        }
    }
}

unsafe impl<T, const N: usize> Sync for MpMcQueue<T, N> where T: Send {}

struct Cell<T> {
    data: MaybeUninit<T>,
    sequence: AtomicUsize,
}

impl<T> Cell<T> {
    const fn new(seq: usize) -> Self {
        Self {
            data: MaybeUninit::uninit(),
            sequence: AtomicUsize::new(seq),
        }
    }
}

unsafe fn dequeue<T>(buffer: *mut Cell<T>, dequeue_pos: &AtomicUsize, mask: usize) -> Option<T> {
    let mut pos = dequeue_pos.load(Ordering::Relaxed);

    let mut cell;
    loop {
        cell = buffer.add(usize::from(pos & mask));
        let seq = (*cell).sequence.load(Ordering::Acquire);
        let dif = (seq as i8).wrapping_sub((pos.wrapping_add(1)) as i8);

        if dif == 0 {
            if dequeue_pos
                .compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
        } else if dif < 0 {
            return None;
        } else {
            pos = dequeue_pos.load(Ordering::Relaxed);
        }
    }

    let data = (*cell).data.as_ptr().read();
    (*cell)
        .sequence
        .store(pos.wrapping_add(mask).wrapping_add(1), Ordering::Release);
    Some(data)
}

unsafe fn enqueue<T>(
    buffer: *mut Cell<T>,
    enqueue_pos: &AtomicUsize,
    mask: usize,
    item: T,
) -> Result<(), T> {
    let mut pos = enqueue_pos.load(Ordering::Relaxed);

    let mut cell;
    loop {
        cell = buffer.add(usize::from(pos & mask));
        let seq = (*cell).sequence.load(Ordering::Acquire);
        let dif = (seq as i8).wrapping_sub(pos as i8);

        if dif == 0 {
            if enqueue_pos
                .compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
        } else if dif < 0 {
            return Err(item);
        } else {
            pos = enqueue_pos.load(Ordering::Relaxed);
        }
    }

    (*cell).data.as_mut_ptr().write(item);
    (*cell)
        .sequence
        .store(pos.wrapping_add(1), Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Q2;

    #[test]
    fn sanity() {
        let q = Q2::new();
        q.enqueue(0).unwrap();
        q.enqueue(1).unwrap();
        assert!(q.enqueue(2).is_err());

        assert_eq!(q.dequeue(), Some(0));
        assert_eq!(q.dequeue(), Some(1));
        assert_eq!(q.dequeue(), None);
    }

    #[test]
    fn drain_at_pos255() {
        let q = Q2::new();
        for _ in 0..255 {
            assert!(q.enqueue(0).is_ok());
            assert_eq!(q.dequeue(), Some(0));
        }
        // this should not block forever
        assert_eq!(q.dequeue(), None);
    }

    #[test]
    fn full_at_wrapped_pos0() {
        let q = Q2::new();
        for _ in 0..254 {
            assert!(q.enqueue(0).is_ok());
            assert_eq!(q.dequeue(), Some(0));
        }
        assert!(q.enqueue(0).is_ok());
        assert!(q.enqueue(0).is_ok());
        // this should not block forever
        assert!(q.enqueue(0).is_err());
    }
}
