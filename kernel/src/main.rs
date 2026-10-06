//! chisel: the CarvOS microkernel.
//!
//! Boot sequence today (P1.1–P1.7): Limine hands over control, `kmain` brings up the 16550 serial
//! console and prints the boot banner immediately, then loads the GDT/TSS/IDT so every CPU
//! exception prints a message (double faults on their own IST stack), builds the physical frame
//! allocator from the memory map (P1.2), adopts the bootloader's page tables for
//! map/unmap/translate (P1.3), backs `alloc` with a 1 MiB kernel heap (P1.4), masks the legacy
//! PICs (P1.5), parses the ACPI tables into a platform summary (P1.6), and enumerates PCI(e)
//! through the MCFG's ECAM window (P1.7). Phase 2 then masks every I/O APIC input, builds the
//! kernel object state (root budget, CSpaces, scheduler with the boot and idle threads). In test
//! builds (`cargo xtask test --kernel`) `kmain` then runs the in-kernel tests instead of the rest
//! of this sequence: `test::runner` exits QEMU directly and never returns; tests that need time
//! to pass block the boot thread, and the idle thread enables interrupts while it halts. In a
//! normal boot, `kmain` instead enables interrupts, validates the local APIC timer ticks at
//! 1 kHz, loads the `init` boot module into its own address space, starts it in ring 3 on the
//! root budget (P2.9) and parks the boot thread. Kernel command line words `panic-test`, `double-fault-test`, `page-fault-test`,
//! `stack-overflow-test` and `oom-test` exercise the panic handler, the double-fault path, the
//! page-fault handler and the guard-page → double-fault path.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
// Every `unsafe` block must carry a `// SAFETY:` comment (CLAUDE.md rule, enforced mechanically).
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]
#![feature(abi_x86_interrupt)]
// Explicit out-of-memory path (see `alloc_error`) instead of `alloc`'s default handler.
#![feature(alloc_error_handler)]
// In-kernel tests: `#[test_case]` functions collected by the compiler and run by `test::runner`.
#![cfg_attr(test, feature(custom_test_frameworks))]
#![cfg_attr(test, test_runner(crate::test::runner))]
#![cfg_attr(test, reexport_test_harness_main = "test_main")]

extern crate alloc;

use core::panic::PanicInfo;

use limine::memmap::MEMMAP_USABLE;
use limine::request::{
    BootloaderInfoRequest, ExecutableCmdlineRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    RsdpRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

mod arch;
pub mod elf;
pub mod ipc;
mod mm;
pub mod objects;
mod platform;
mod scheduler;
mod serial;
mod sync;
mod syscalls;
#[cfg(test)]
mod test;
#[cfg(test)]
mod usertest;

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

#[used]
#[unsafe(link_section = ".requests")]
static RSDP: RsdpRequest = RsdpRequest::new();

/// Boot modules: the root task's ELF image, `boot/init` (P2.9).
#[used]
#[unsafe(link_section = ".requests")]
static MODULES: ModulesRequest = ModulesRequest::new();

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
    arch::x86_64::syscall::init();
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
        let heap = mm::heap::init();
        kprintln!(
            "  heap: {} KiB at {:#x} ({} bytes used by the allocator itself)",
            heap.size / 1024,
            mm::heap::HEAP_BASE,
            heap.used
        );
        arch::x86_64::pic::remap_and_mask();
        let apic = arch::x86_64::apic::init();
        kprintln!(
            "  apic: xAPIC id {} at {:#x} (mapped at {:#x}); timer {} ticks/ms (÷16), periodic at {} Hz on vector {}; PICs masked",
            apic.id,
            apic.phys_base,
            mm::paging::KERNEL_MMIO_BASE,
            apic.ticks_per_ms,
            arch::x86_64::apic::TIMER_HZ,
            arch::x86_64::apic::TIMER_VECTOR
        );
        let rsdp = RSDP
            .response()
            .expect("Limine did not provide the RSDP; ACPI is required")
            .address as u64;
        let acpi = platform::acpi::init(rsdp, hhdm);
        kprintln!(
            "  acpi: RSDP at {:#x} (revision {}), {} tables: {}",
            acpi.rsdp,
            acpi.revision,
            acpi.tables.len(),
            TableList(&acpi.tables)
        );
        kprintln!(
            "  acpi: MADT: local APIC at {:#x}, {} CPU(s) with APIC id(s) {:?} (this CPU: {}), {} I/O APIC(s){}",
            acpi.local_apic_address,
            acpi.cpu_apic_ids.len(),
            acpi.cpu_apic_ids,
            arch::x86_64::apic::id(),
            acpi.io_apics.len(),
            acpi.io_apics
                .first()
                .map(|io| alloc::format!(
                    " (id {} at {:#x}, GSI base {})",
                    io.id,
                    io.address,
                    io.gsi_base
                ))
                .unwrap_or_default()
        );
        match acpi.hpet_base {
            Some(base) => kprintln!("  acpi: HPET at {base:#x}"),
            None => kprintln!("  acpi: no HPET table"),
        }
        for r in &acpi.ecam {
            kprintln!(
                "  acpi: MCFG: PCIe segment {} buses {}-{} ECAM at {:#x}",
                r.segment,
                r.bus_start,
                r.bus_end,
                r.base
            );
        }
        let devices = platform::pci::enumerate(&acpi.ecam);
        kprintln!("  pci: {} function(s) found via ECAM", devices.len());
        for d in &devices {
            kprintln!(
                "  pci: {:04x}:{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}.{:02x}{}",
                d.segment,
                d.bus,
                d.device,
                d.function,
                d.vendor,
                d.device_id,
                d.class,
                d.subclass,
                d.prog_if,
                d.virtio_name()
                    .map(|n| alloc::format!(" {n}"))
                    .unwrap_or_default()
            );
        }
        kprintln!(
            "  frames: {} free of {} ({} MiB); bitmap {} KiB at {:#x}; self-check ok ({:#x})",
            layout.free_frames,
            layout.frame_count,
            layout.free_frames * carv_frames::FRAME_SIZE / (1024 * 1024),
            layout.bitmap_bytes / 1024,
            layout.bitmap_phys.as_u64(),
            probe.start().as_u64()
        );
        match arch::x86_64::ioapic::init(&acpi) {
            Some(inputs) => kprintln!(
                "  ioapic: {inputs} inputs masked; GSI n arrives on vector {} + n",
                arch::x86_64::ioapic::IRQ_VECTOR_BASE
            ),
            None => kprintln!("  ioapic: none found; device IRQs unavailable"),
        }
        let (free, _) = mm::frame::stats();
        objects::init(free as u64 * carv_frames::FRAME_SIZE as u64);
        kprintln!(
            "  objects: root budget owns {} MiB and the whole CPU; scheduler up (boot + idle threads)",
            free * carv_frames::FRAME_SIZE / (1024 * 1024)
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
    if cmdline.split_whitespace().any(|w| w == "oom-test") {
        force_oom();
    }

    // With every table in place, take interrupts and prove the timer runs at the right rate
    // before idling; the smoke scenarios look for the success line, which only a 1 kHz timer
    // produces (a dead or misprogrammed timer panics here instead).
    arch::x86_64::enable_interrupts();
    let before = arch::x86_64::apic::ticks();
    arch::x86_64::pit::busy_wait_ms(100);
    let ticks = arch::x86_64::apic::ticks() - before;
    assert!(
        (90..=110).contains(&ticks),
        "LAPIC timer rate wrong: {ticks} ticks in 100 ms, expected 1 kHz"
    );
    kprintln!("  timer: LAPIC timer ticking at 1 kHz ({ticks} ticks in 100 ms)");
    arch::x86_64::disable_interrupts();
    start_init();
    // The boot thread's work is done: park it for good and let the scheduler run init.
    objects::with(|k| k.sched.block_current(scheduler::Wait::Sleep(u64::MAX)));
    scheduler::reschedule();
    unreachable!("the boot thread was woken")
}

/// Loads the `boot/init` module into a fresh address space and starts it in ring 3 on the root
/// budget, with the CSpace layout of `carv_abi::init` (P2.9). Halts if there is no init.
fn start_init() {
    let module = MODULES
        .response()
        .and_then(|r| r.modules().iter().find(|m| m.path().ends_with("/init")).copied());
    let Some(module) = module else {
        kprintln!("chisel: no init module; nothing to run; halting");
        halt_forever()
    };
    let bytes = module.data();
    let started = objects::with(|k| {
        let root = k.objects.root_budget();
        let process = syscalls::create_process(k, root, true)?;
        let entry = syscalls::load_elf(k, process.space, bytes)?;
        let rsp = syscalls::map_stack(k, process.space)?;
        syscalls::start(k, &process, entry, rsp, 0)?;
        Ok::<_, carv_abi::Error>((process.thread, entry))
    });
    match started {
        Ok((thread, entry)) => kprintln!(
            "chisel: starting init ({} bytes, entry {entry:#x}) as thread {thread}",
            bytes.len()
        ),
        Err(e) => panic!("loading init: {e:?}"),
    }
}

/// Prints ACPI table signatures space-separated (they are ASCII by specification).
struct TableList<'a>(&'a [[u8; 4]]);

impl core::fmt::Display for TableList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, sig) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(core::str::from_utf8(sig).unwrap_or("????"))?;
        }
        Ok(())
    }
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

/// Asks the heap for more than it holds so the allocation-error path runs: `alloc_error` must
/// report `kernel heap exhausted` and end in the panic handler.
fn force_oom() -> ! {
    use alloc::vec::Vec;
    kprintln!(
        "chisel: allocating {} MiB from a {} KiB heap on purpose",
        2,
        mm::heap::HEAP_SIZE / 1024
    );
    let v: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024);
    unreachable!(
        "a {}-byte allocation beyond the heap did not fail",
        v.capacity()
    );
}

/// Out-of-memory is a kernel bug or a missing budget check until the heap can grow; report the
/// request and the heap state, then take the panic path (which halts).
#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    panic!(
        "kernel heap exhausted: allocation of {} bytes (align {}) failed; {:?}",
        layout.size(),
        layout.align(),
        mm::heap::stats()
    );
}

/// Switches to a small kernel stack that has an unmapped guard page beneath it, then recurses
/// until the stack overflows into the guard: the write faults, the `#PF` handler cannot push its
/// frame onto the exhausted stack, and the CPU escalates to `#DF` on the IST stack.
fn force_stack_overflow() -> ! {
    use x86_64::VirtAddr;
    // The guard page one page below `base` stays unmapped; four mapped pages sit above it.
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
            // First integer argument (`depth`) travels in rdi per the SysV ABI; leaving it to
            // whatever the register held would make the recursion start from garbage (#40).
            in("rdi") 0u64,
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
