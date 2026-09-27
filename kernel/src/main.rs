//! chisel: the CarvOS microkernel.
//!
//! P0.3 state: Limine boots us, the 16550 boot console comes up, we print the banner and what
//! the bootloader handed over, and halt. Kernel command line `panic-test` exercises the panic
//! handler. GDT/IDT (P1.1) and the rest of the kernel core come next.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;

use limine::memmap::MEMMAP_USABLE;
use limine::request::{
    BootloaderInfoRequest, ExecutableCmdlineRequest, HhdmRequest, MemmapRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

mod arch;
mod serial;
mod sync;

use arch::x86_64::halt_forever;

/// Limine base-revision-6 protocol requests. `kernel/linker.ld` keeps these sections in the
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
static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

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
    serial::init();
    // Limine's serial terminal leaves the cursor mid-line; start the banner on a fresh one.
    kprintln!();
    kprintln!("CarvOS chisel v{} booting", env!("CARGO_PKG_VERSION"));

    if !BASE_REVISION.is_supported() {
        panic!(
            "bootloader does not support Limine base revision {} (got {:?})",
            BaseRevision::MAX_SUPPORTED,
            BASE_REVISION.actual_revision()
        );
    }

    if let Some(info) = BOOTLOADER_INFO.response() {
        kprintln!(
            "  bootloader: {} {} (base revision {})",
            info.name(),
            info.version(),
            BaseRevision::MAX_SUPPORTED
        );
    }
    let cmdline = CMDLINE.response().map(|c| c.cmdline()).unwrap_or("");
    if !cmdline.is_empty() {
        kprintln!("  cmdline: {cmdline}");
    }
    if let Some(hhdm) = HHDM.response() {
        kprintln!("  hhdm offset: {:#x}", hhdm.offset);
    }
    if let Some(memmap) = MEMMAP.response() {
        let entries = memmap.entries();
        let usable: u64 = entries
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.length)
            .sum();
        kprintln!(
            "  memory map: {} entries, {} MiB usable",
            entries.len(),
            usable / (1024 * 1024)
        );
    }

    if cmdline.split_whitespace().any(|w| w == "panic-test") {
        panic!("deliberate panic requested on the kernel command line");
    }

    kprintln!("chisel: nothing more to do yet; halting");
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::x86_64::disable_interrupts();
    match info.location() {
        Some(loc) => serial::_print_unlocked(format_args!(
            "chisel: PANIC at {}:{}:{}: {}\n",
            loc.file(),
            loc.line(),
            loc.column(),
            info.message()
        )),
        None => serial::_print_unlocked(format_args!("chisel: PANIC: {}\n", info.message())),
    }
    halt_forever()
}
