//! chisel: the CarvOS microkernel.
//!
//! P0.2 state: the kernel is booted by Limine, checks the protocol handoff, prints
//! what the bootloader handed over, and halts. The real serial driver (P0.3), the
//! in-kernel test framework (P0.4) and everything else come next.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::fmt::Write;
use core::panic::PanicInfo;

use limine::memmap::MEMMAP_USABLE;
use limine::request::{BootloaderInfoRequest, HhdmRequest, MemmapRequest, StackSizeRequest};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

mod early;

/// Limine base-revision 6 protocol requests. `kernel/linker.ld` keeps these sections in the
/// RW segment; the bootloader fills in each request's response before jumping to `kmain`.
#[used]
#[unsafe(link_section = ".requests_start")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static MEMMAP: MemmapRequest = MemmapRequest::new();

/// 64 KiB boot stack; the default is smaller than the kernel will want once paging code lands.
#[used]
#[unsafe(link_section = ".requests")]
static STACK_SIZE: StackSizeRequest = StackSizeRequest::new(64 * 1024);

#[used]
#[unsafe(link_section = ".requests_end")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Kernel entry point, named in `linker.ld`. Limine enters here in 64-bit mode with
/// paging on, interrupts off, and `rsp` pointing at the stack it allocated for us.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let mut out = early::Uart;

    if !BASE_REVISION.is_supported() {
        let _ = writeln!(
            out,
            "chisel: bootloader does not support Limine base revision {} (got {:?}); halting",
            BaseRevision::MAX_SUPPORTED,
            BASE_REVISION.actual_revision()
        );
        halt_forever();
    }

    let _ = writeln!(
        out,
        "CarvOS chisel: Limine handoff OK (base revision {})",
        BaseRevision::MAX_SUPPORTED
    );

    if let Some(info) = BOOTLOADER_INFO.response() {
        let _ = writeln!(out, "  bootloader: {} {}", info.name(), info.version());
    }
    if let Some(hhdm) = HHDM.response() {
        let _ = writeln!(out, "  hhdm offset: {:#x}", hhdm.offset);
    }
    if let Some(memmap) = MEMMAP.response() {
        let entries = memmap.entries();
        let usable: u64 = entries
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.length)
            .sum();
        let _ = writeln!(
            out,
            "  memory map: {} entries, {} MiB usable",
            entries.len(),
            usable / (1024 * 1024)
        );
    }

    let _ = writeln!(out, "chisel: nothing more to do yet; halting");
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(early::Uart, "chisel: PANIC: {info}");
    halt_forever()
}

fn halt_forever() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt; it touches no memory.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}
