//! chisel: the CarvOS microkernel.
//!
//! P1.1 state: Limine boots us, the 16550 boot console comes up, the GDT/TSS/IDT are loaded so
//! every CPU exception prints a message (double faults on their own IST stack), we print the
//! banner and what the bootloader handed over, and halt. Kernel command line words `panic-test`
//! and `double-fault-test` exercise the panic handler and the double-fault path. In test builds
//! (`cargo xtask test --kernel`) `kmain` runs the in-kernel tests instead and exits QEMU with a
//! pass/fail code. The physical frame allocator is built from the memory map (P1.2) and the
//! bootloader's page tables are adopted for map/unmap/translate (P1.3); the kernel heap (P1.4)
//! comes next. Cmdline words `page-fault-test` and `stack-overflow-test` exercise the page-fault
//! handler and the guard-page → double-fault path.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
// Every `unsafe` block must carry a `// SAFETY:` comment (CLAUDE.md rule, enforced mechanically).
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]
#![feature(abi_x86_interrupt)]
// In-kernel tests: `#[test_case]` functions collected by the compiler and run by `test::runner`.
#![cfg_attr(test, feature(custom_test_frameworks))]
#![cfg_attr(test, test_runner(crate::test::runner))]
#![cfg_attr(test, reexport_test_harness_main = "test_main")]

use core::panic::PanicInfo;

use limine::memmap::MEMMAP_USABLE;
use limine::request::{
    BootloaderInfoRequest, ExecutableCmdlineRequest, HhdmRequest, MemmapRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

mod arch;
mod mm;
mod serial;
mod sync;
#[cfg(test)]
mod test;

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

    arch::x86_64::gdt::init();
    arch::x86_64::idt::init();
    let sel = arch::x86_64::gdt::selectors();
    kprintln!(
        "  gdt/tss/idt: loaded (cs={:#x} ss={:#x} tss={:#x}; double-fault IST top {:#x})",
        sel.code.0,
        sel.data.0,
        sel.tss.0,
        arch::x86_64::gdt::double_fault_stack_top().as_u64()
    );

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
        let hhdm = HHDM
            .response()
            .expect("Limine did not provide the HHDM offset")
            .offset;
        let layout = mm::frame::init(entries, hhdm);
        let probe = mm::frame::self_check(hhdm);
        // SAFETY: `hhdm` is Limine's HHDM offset and CR3 still holds Limine's tables, which live in
        // bootloader-reclaimable memory the frame allocator never hands out.
        unsafe { mm::paging::init(hhdm) };
        kprintln!(
            "  paging: adopted CR3 tables; kernel dynamic region at {:#x}",
            mm::paging::KERNEL_DYNAMIC_BASE
        );
        kprintln!(
            "  frames: {} free of {} ({} MiB); bitmap {} KiB at {:#x}; self-check ok ({:#x})",
            layout.free_frames,
            layout.frame_count,
            layout.free_frames * carv_frames::FRAME_SIZE / (1024 * 1024),
            layout.bitmap_bytes / 1024,
            layout.bitmap_phys.as_u64(),
            probe.start().as_u64()
        );
    }

    #[cfg(test)]
    test_main();

    if cmdline.split_whitespace().any(|w| w == "panic-test") {
        panic!("deliberate panic requested on the kernel command line");
    }
    if cmdline.split_whitespace().any(|w| w == "double-fault-test") {
        force_double_fault();
    }
    if cmdline.split_whitespace().any(|w| w == "page-fault-test") {
        force_page_fault();
    }
    if cmdline
        .split_whitespace()
        .any(|w| w == "stack-overflow-test")
    {
        force_stack_overflow();
    }

    kprintln!("chisel: nothing more to do yet; halting");
    halt_forever()
}

/// Makes the stack unusable and raises an exception, so the CPU faults while pushing the
/// exception frame and escalates to a double fault — which must land on the IST stack and print
/// `chisel: DOUBLE FAULT`. This is the deterministic stand-in for a stack overflow until paging
/// (P1.3) gives the kernel stack a guard page.
fn force_double_fault() -> ! {
    // SAFETY: deliberately destroys the stack pointer; the double-fault handler halts the CPU
    // and nothing after this point ever runs.
    unsafe {
        core::arch::asm!(
            "mov rsp, {bad}",
            "int3",
            bad = in(reg) 0x0000_8000_0000_0000u64, // first non-canonical address
            options(noreturn)
        );
    }
}

/// Maps a page, unmaps it again, then touches it: the access must reach the `#PF` handler,
/// which reports `PAGE FAULT accessing <addr>` and panics.
fn force_page_fault() -> ! {
    use x86_64::VirtAddr;
    use x86_64::structures::paging::Page;
    let addr = VirtAddr::new(mm::paging::KERNEL_DYNAMIC_BASE + 0x10_0000);
    let page = Page::containing_address(addr);
    let frame = mm::frame::allocate().expect("frame");
    if let Err((e, f)) = mm::paging::map(page, frame, mm::paging::KERNEL_DATA) {
        mm::frame::free(f);
        panic!("map: {e}");
    }
    let unmapped = match mm::paging::unmap(page) {
        Ok(f) => f,
        Err(e) => panic!("unmap: {e}"),
    };
    mm::frame::free(unmapped);
    assert_eq!(
        mm::paging::translate(addr),
        None,
        "page still translates after unmap"
    );
    kprintln!(
        "chisel: touching unmapped page {:#x} on purpose",
        addr.as_u64()
    );
    // SAFETY: intentionally faulting read of an unmapped page; the #PF handler panics and halts.
    let _ = unsafe { core::ptr::read_volatile(addr.as_ptr::<u64>()) };
    unreachable!("read of an unmapped page did not fault");
}

/// Switches to a small kernel stack that has an unmapped guard page beneath it, then recurses
/// until the stack overflows into the guard: the write faults, the `#PF` handler cannot push its
/// frame onto the exhausted stack, and the CPU escalates to `#DF` on the IST stack.
fn force_stack_overflow() -> ! {
    use x86_64::VirtAddr;
    // Guard page at TEST_STACK_BASE - 4 KiB stays unmapped; four mapped pages above it.
    let base = VirtAddr::new(mm::paging::KERNEL_DYNAMIC_BASE + 0x20_0000);
    let top = mm::paging::map_stack_with_guard(base, 4);
    kprintln!(
        "chisel: recursing on a 16 KiB stack at {:#x} with a guard page below",
        base.as_u64()
    );
    // SAFETY: switches to the freshly mapped stack and never returns; the recursion ends in a
    // double fault whose handler halts the CPU.
    unsafe {
        core::arch::asm!(
            "mov rsp, {top}",
            "call {f}",
            top = in(reg) top.as_u64(),
            f = sym recurse_forever,
            options(noreturn)
        );
    }
}

/// Unbounded recursion with a live stack frame each level (no tail call possible). The
/// recursion is the point: it ends in the double-fault handler, never by returning.
#[inline(never)]
#[allow(unconditional_recursion)]
extern "C" fn recurse_forever(depth: u64) -> u64 {
    let mut frame = [0u64; 32];
    frame[(depth % 32) as usize] = core::hint::black_box(depth);
    let below = recurse_forever(depth + 1);
    core::hint::black_box(&frame);
    below + frame[0]
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::x86_64::disable_interrupts();
    #[cfg(test)]
    serial::_print_unlocked(format_args!("[failed]\n"));
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
    #[cfg(test)]
    test::exit_qemu(test::QemuExitCode::Failed);
    #[cfg(not(test))]
    halt_forever()
}
