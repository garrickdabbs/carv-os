//! The CarvOS user-space runtime (P3.1).
//!
//! A program is a `no_std`, `no_main` binary that depends on this crate, declares its entry point
//! with [`entry!`] and gets, on the `x86_64-unknown-none` target:
//!
//! - `_start`, which runs the program's `main` and then exits ([`runtime`]);
//! - one wrapper per syscall and `invoke` method ([`sys`]);
//! - a global allocator, so `alloc` works: the heap starts empty and grows by mapping frames
//!   charged to the program's budget ([`heap`]);
//! - a panic handler that prints the message and exits the thread;
//! - [`println!`]/[`print!`], sent to the kernel's `debug_putc` or, once a console capability is
//!   installed with [`console::set_console_cap`], to a console server ([`console`]).
//!
//! The hardware-facing parts are compiled only for the kernel target (`target_os = "none"`), so
//! the pure logic — heap growth, console encoding, status decoding — is unit-tested on the host.
//! Link a program with `linker.ld` from this crate's directory (see `userland/hello/build.rs`).
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! carv_rt::entry!(main);
//!
//! fn main() {
//!     carv_rt::println!("hello, world");
//! }
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(test)]
extern crate std;

pub use carv_abi;

pub mod console;
pub mod error;
pub mod heap;
pub mod sync;

#[cfg(target_os = "none")]
pub mod runtime;
#[cfg(target_os = "none")]
pub mod sys;

pub use error::{Result, SysError};

/// Declares the program's entry point: `entry!(main)` defines `_start`, which calls `fn main()`
/// and then exits the thread. Use it once, in a `#![no_main]` binary.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        /// The program's entry point, started by the kernel in ring 3.
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            let main: fn() = $main;
            $crate::runtime::start(main)
        }
    };
}
