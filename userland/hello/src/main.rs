//! Hello-world for CarvOS, built separately from the kernel on top of `carv-rt` (P3.1).
//!
//! It prints through `println!`, then allocates enough on the heap to make the runtime map more
//! frames from its budget, and returns, which exits the thread. On host targets it builds as an
//! empty stub so workspace host checks can include the crate.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
carv_rt::entry!(main);

#[cfg(target_os = "none")]
fn main() {
    use alloc::{string::String, vec::Vec};
    use carv_rt::println;

    println!("hello: hello, world from carv-rt");

    let squares: Vec<u64> = (1..=10).map(|n| n * n).collect();
    let sum: u64 = squares.iter().sum();
    println!("hello: sum of the first ten squares is {sum}");

    // 1 MiB is far more than the heap's first mapping, so this grows it from the budget.
    let mut big: Vec<u8> = Vec::new();
    big.resize(1024 * 1024, 0xab);
    let checksum = big.iter().map(|&b| u64::from(b)).sum::<u64>();
    println!("hello: grew the heap, 1 MiB checksum {checksum}");

    let mut greeting = String::from("hello");
    greeting.push_str(": done");
    println!("{greeting}");
}
