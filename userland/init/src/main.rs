//! Minimal CarvOS root task.
//!
//! On the kernel target this binary starts in ring 3, exercises the initial root-budget authority by
//! creating an endpoint, then blocks forever receiving on that endpoint. On host targets it builds as
//! an empty stub so workspace host checks can include the crate.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "none")]
use core::arch::asm;
#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
const OK: u64 = 0;

#[cfg(target_os = "none")]
#[derive(Clone, Copy)]
struct SyscallResults {
    status: u64,
    result0: u64,
}

#[cfg(target_os = "none")]
fn syscall6(
    number: carv_abi::Syscall,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> SyscallResults {
    let mut status = number as u64;
    let mut result0 = arg2;
    let mut result1 = arg1;
    // SAFETY: This is the CarvOS userspace syscall ABI: rax holds the syscall number; arguments are
    // in rdi, rsi, rdx, r10, r8, and r9; syscall clobbers rcx and r11 and returns status/results in
    // rax, rdx, and rsi. The inline assembly declares exactly those registers.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") status,
            in("rdi") arg0,
            inlateout("rsi") result1,
            inlateout("rdx") result0,
            in("r10") arg3,
            in("r8") arg4,
            in("r9") arg5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    let _ = result1;
    SyscallResults { status, result0 }
}

#[cfg(target_os = "none")]
fn debug_putc(byte: u8) {
    let _ = syscall6(carv_abi::Syscall::DebugPutc, u64::from(byte), 0, 0, 0, 0, 0);
}

#[cfg(target_os = "none")]
fn print(message: &str) {
    for byte in message.bytes() {
        debug_putc(byte);
    }
}

#[cfg(target_os = "none")]
fn invoke(cap: u64, method: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> SyscallResults {
    syscall6(
        carv_abi::Syscall::Invoke,
        cap,
        method,
        arg0,
        arg1,
        arg2,
        arg3,
    )
}

#[cfg(target_os = "none")]
fn recv(endpoint: u64, message: &mut carv_abi::Message) {
    let _ = syscall6(
        carv_abi::Syscall::Recv,
        endpoint,
        core::ptr::from_mut(message) as u64,
        0,
        0,
        0,
        0,
    );
}

#[cfg(target_os = "none")]
fn yield_now() {
    let _ = syscall6(carv_abi::Syscall::Yield, 0, 0, 0, 0, 0, 0);
}

/// Starts the root task in ring 3.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    print("init: hello from ring 3\n");

    let created = invoke(
        carv_abi::init::ROOT_BUDGET,
        carv_abi::method::BUDGET_CREATE,
        carv_abi::ObjectType::Endpoint as u64,
        carv_abi::init::FIRST_FREE,
        0,
        0,
    );
    let described = if created.status == OK {
        invoke(
            carv_abi::init::FIRST_FREE,
            carv_abi::method::DESCRIBE,
            0,
            0,
            0,
            0,
        )
    } else {
        created
    };
    if created.status == OK
        && described.status == OK
        && described.result0 == carv_abi::ObjectType::Endpoint as u64
    {
        print("init: created an endpoint via invoke\n");
    } else {
        print("init: invoke failed\n");
    }

    print("init: idle\n");
    let mut message = carv_abi::Message::default();
    loop {
        recv(carv_abi::init::FIRST_FREE, &mut message);
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    print("init: panic\n");
    loop {
        yield_now();
    }
}
