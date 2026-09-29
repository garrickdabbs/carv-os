//! In-kernel test framework (`cargo xtask test --kernel`).
//!
//! Built only for `cargo test -p chisel --target x86_64-unknown-none`. The test binary is a
//! complete kernel: Limine boots it in QEMU, `kmain` calls the generated `test_main`, every
//! `#[test_case]` function runs on the boot console, and the kernel exits QEMU through the
//! `isa-debug-exit` device with a code that tells `xtask` whether everything passed.

use crate::arch::x86_64::port::Port;
use crate::{kprint, kprintln};

/// QEMU `isa-debug-exit` device (`-device isa-debug-exit,iobase=0xf4,iosize=0x04`). A write of
/// `v` makes QEMU exit with status `(v << 1) | 1`.
const DEBUG_EXIT_PORT: u16 = 0xF4;

/// Exit codes handed to QEMU. `xtask` maps `Success` → 0 and `Failed` → 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum QemuExitCode {
    /// All tests passed. QEMU exits 33.
    Success = 0x10,
    /// A test panicked. QEMU exits 35.
    Failed = 0x11,
}

/// Asks QEMU to exit with `code`. Never returns under QEMU; halts if the device is absent.
pub fn exit_qemu(code: QemuExitCode) -> ! {
    // SAFETY: port 0xF4 is QEMU's isa-debug-exit device, which only terminates the VM; on any
    // other machine the write hits nothing and we simply halt below.
    unsafe { Port::<u8>::new(DEBUG_EXIT_PORT).write(code as u8) };
    crate::arch::x86_64::halt_forever()
}

/// Anything the test runner can execute and name. Implemented for every `fn()`.
pub trait Testable {
    /// Runs the test, printing its name before and `[ok]` after.
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        kprint!("test {} ... ", core::any::type_name::<T>());
        self();
        kprintln!("[ok]");
    }
}

/// The `#![test_runner]`: runs every `#[test_case]`, then exits QEMU with success. A failing
/// test panics, and the test-build panic handler in `main.rs` exits with `Failed`.
pub fn runner(tests: &[&dyn Testable]) -> ! {
    kprintln!();
    kprintln!("chisel-test: running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    kprintln!("chisel-test: {} passed; 0 failed", tests.len());
    exit_qemu(QemuExitCode::Success)
}

// ---------------------------------------------------------------- the tests

#[test_case]
fn arithmetic_works() {
    let two = core::hint::black_box(2);
    assert_eq!(two + two, 4);
}

#[test_case]
fn console_prints_many_lines() {
    for i in 0..64 {
        kprintln!("console line {i}");
    }
}

#[test_case]
fn console_lock_is_reentrant_across_calls() {
    // Each kprintln! takes and releases the console lock; a second call must not deadlock.
    kprintln!("first");
    kprintln!("second");
}

#[test_case]
fn spinlock_guards_a_value() {
    let lock = crate::sync::SpinLock::new(1u32);
    {
        let mut g = lock.lock();
        *g += 41;
    }
    assert_eq!(*lock.lock(), 42);
}

#[test_case]
fn breakpoint_exception_is_handled_and_resumes() {
    use core::sync::atomic::Ordering;
    let before = crate::arch::x86_64::idt::BREAKPOINTS.load(Ordering::Relaxed);
    x86_64::instructions::interrupts::int3();
    assert_eq!(
        crate::arch::x86_64::idt::BREAKPOINTS.load(Ordering::Relaxed),
        before + 1
    );
}

#[test_case]
fn gdt_selectors_are_live() {
    use x86_64::instructions::segmentation::{CS, Segment};
    let sel = crate::arch::x86_64::gdt::selectors();
    assert_eq!(CS::get_reg(), sel.code);
    assert_ne!(sel.tss.0, 0);
    // The double-fault IST slot points into our own stack, 16-byte aligned.
    let top = crate::arch::x86_64::gdt::double_fault_stack_top().as_u64();
    assert_ne!(top, 0);
    assert_eq!(top % 16, 0);
}

/// Scratch space for the frame test; 10k indexes is too big for the 64 KiB boot stack.
static FRAME_SCRATCH: crate::sync::StaticCell<[u32; 10_000]> =
    crate::sync::StaticCell::new([0; 10_000]);

#[test_case]
fn frame_allocator_ten_thousand_frames_unique_and_restored() {
    use crate::mm::frame;
    let (free_before, total) = frame::stats();
    assert!(
        total > 10_000 && free_before > 10_000,
        "VM too small for the test"
    );

    // SAFETY: test code on the single boot CPU; nothing else touches FRAME_SCRATCH.
    let scratch = unsafe { FRAME_SCRATCH.get_mut() };
    for slot in scratch.iter_mut() {
        let f = frame::allocate().expect("frame available");
        assert_ne!(f.start().as_u64(), 0, "frame 0 must never be handed out");
        assert_eq!(f.start().as_u64() % 4096, 0);
        *slot = f.index() as u32;
    }
    assert_eq!(frame::stats().0, free_before - 10_000);

    // No duplicates: the allocator hands out ascending indexes from a fresh bitmap, so a sorted
    // check is a strict-increase check; fall back to the quadratic check if that ever changes.
    let ascending = scratch.windows(2).all(|w| w[0] < w[1]);
    if !ascending {
        for i in 0..scratch.len() {
            for j in (i + 1)..scratch.len() {
                assert_ne!(scratch[i], scratch[j], "duplicate frame {}", scratch[i]);
            }
        }
    }

    // Free every other one first, then the rest: the count must come back exactly.
    for (k, &idx) in scratch.iter().enumerate() {
        if k % 2 == 0 {
            frame::free(frame::Frame::from_index(idx as usize));
        }
    }
    for (k, &idx) in scratch.iter().enumerate() {
        if k % 2 == 1 {
            frame::free(frame::Frame::from_index(idx as usize));
        }
    }
    assert_eq!(frame::stats(), (free_before, total));
}

#[test_case]
fn paging_map_write_translate_unmap() {
    use crate::mm::{frame, paging};
    use x86_64::VirtAddr;
    use x86_64::structures::paging::Page;

    let addr = VirtAddr::new(paging::KERNEL_DYNAMIC_BASE + 0x40_0000);
    let page = Page::containing_address(addr);
    assert_eq!(
        paging::translate(addr),
        None,
        "test page must start unmapped"
    );

    let f = frame::allocate().expect("frame");
    if let Err((e, returned)) = paging::map(page, f, paging::KERNEL_DATA) {
        frame::free(returned);
        panic!("mapping {addr:#x} failed: {e}");
    }
    assert_eq!(
        paging::translate(addr),
        Some(f.start()),
        "translate must return the mapped frame"
    );

    // Write through the new mapping, read back through the HHDM alias of the same frame.
    let hhdm = crate::HHDM.response().unwrap().offset;
    // SAFETY: `addr` was just mapped to `f` (writable, kernel-only) and the HHDM maps `f` too;
    // both pointers are valid, aligned and refer to memory nobody else uses.
    unsafe {
        core::ptr::write_volatile(addr.as_mut_ptr::<u64>(), 0x5EED_F00D_0000_1337);
        let via_hhdm = core::ptr::read_volatile((hhdm + f.start().as_u64()) as *const u64);
        assert_eq!(via_hhdm, 0x5EED_F00D_0000_1337);
    }

    let unmapped = paging::unmap(page).expect("unmap");
    assert_eq!(unmapped, f);
    assert_eq!(
        paging::translate(addr),
        None,
        "page must be gone after unmap"
    );
    frame::free(f);
}

#[test_case]
fn paging_refuses_pages_outside_the_dynamic_region() {
    use crate::mm::{frame, paging};
    use x86_64::VirtAddr;
    use x86_64::structures::paging::Page;
    // The kernel image and the HHDM are off limits to the safe API.
    let kernel_page = Page::containing_address(VirtAddr::new(0xffff_ffff_8000_0000));
    let f = frame::allocate().unwrap();
    match paging::map(kernel_page, f, paging::KERNEL_DATA) {
        Err((paging::MapError::OutsideDynamicRegion, returned)) => {
            assert_eq!(returned, f, "the frame must come back to its owner");
            frame::free(returned);
        }
        other => panic!("expected OutsideDynamicRegion, got {other:?}"),
    }
    assert!(matches!(
        paging::unmap(kernel_page),
        Err(paging::UnmapFail::OutsideDynamicRegion)
    ));
    // A frame the allocator never handed out (frame 0 is reserved) is refused as not owned.
    let reserved = crate::mm::frame::Frame::from_index(0);
    let page0 = Page::containing_address(VirtAddr::new(paging::KERNEL_DYNAMIC_BASE + 0x60_0000));
    assert!(matches!(
        paging::map(page0, reserved, paging::KERNEL_DATA),
        Err((paging::MapError::FrameNotOwned, _))
    ));
    // Mapping the same dynamic page twice fails and still returns the frame.
    let addr = VirtAddr::new(paging::KERNEL_DYNAMIC_BASE + 0x50_0000);
    let page = Page::containing_address(addr);
    let a = frame::allocate().unwrap();
    if let Err((e, returned)) = paging::map(page, a, paging::KERNEL_DATA) {
        // The test is about to fail anyway, but the frame still goes back to its owner (#43).
        frame::free(returned);
        panic!("first mapping of {addr:#x} failed: {e}");
    }
    // Allocated only once the first mapping holds, so no frame is in flight on the failure path.
    let b = frame::allocate().unwrap();
    match paging::map(page, b, paging::KERNEL_DATA) {
        Err((paging::MapError::Mapper(_), returned)) => frame::free(returned),
        other => panic!("expected a mapper error, got {other:?}"),
    }
    frame::free(paging::unmap(page).unwrap());
}

#[test_case]
fn interrupts_are_disabled_at_boot() {
    // Limine hands us the CPU with IF clear; the IDT is loaded before tests run, but a test build's
    // `test::runner` exits QEMU directly and never returns to the `enable_interrupts()` call later
    // in `kmain`, so interrupts stay off for the whole test run. Anything else is a bug.
    assert!(!crate::arch::x86_64::interrupts_enabled());
}

#[test_case]
fn heap_vec_box_and_btreemap_work() {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    let mut v: Vec<u64> = Vec::new();
    for i in 0..10_000u64 {
        v.push(i * 3);
    }
    assert_eq!(v.len(), 10_000);
    assert_eq!(v.iter().sum::<u64>(), 3 * (9_999 * 10_000 / 2));
    let b = Box::new([0xA5u8; 4096]);
    assert!(b.iter().all(|&x| x == 0xA5));
    let mut m: BTreeMap<u32, &str> = BTreeMap::new();
    for (k, name) in [(3, "three"), (1, "one"), (2, "two")] {
        m.insert(k, name);
    }
    assert_eq!(
        m.values().copied().collect::<Vec<_>>(),
        ["one", "two", "three"]
    );
    assert_eq!(m.get(&2), Some(&"two"));
}

#[test_case]
fn heap_returns_memory_after_drop() {
    use crate::mm::heap;
    use alloc::vec::Vec;
    let before = heap::stats();
    assert!(
        before.free > 512 * 1024,
        "heap should have most of its {} bytes free at test time",
        before.size
    );
    for round in 0..200usize {
        let v: Vec<u8> = (0..(round % 97) + 1).map(|x| x as u8).collect();
        let big: Vec<u64> = alloc::vec![round as u64; 4096];
        assert_eq!(big[4095], round as u64);
        assert_eq!(v.len(), (round % 97) + 1);
    }
    let after = heap::stats();
    assert_eq!(
        after.in_use, before.in_use,
        "live bytes must return to baseline"
    );
    assert_eq!(
        after.used, before.used,
        "allocator usage must return to baseline"
    );
}

#[test_case]
fn lapic_timer_ticks_at_one_khz() {
    use crate::arch::x86_64::{self, apic, pit};
    assert!(
        !x86_64::interrupts_enabled(),
        "tests run with interrupts off"
    );
    let before = apic::ticks();
    x86_64::enable_interrupts();
    pit::busy_wait_ms(100);
    x86_64::disable_interrupts();
    let ticks = apic::ticks() - before;
    assert!(
        (90..=110).contains(&ticks),
        "expected ~100 ticks in 100 ms at 1 kHz, got {ticks}"
    );
}

#[test_case]
fn acpi_summary_lists_apics_hpet_and_ecam() {
    use crate::arch::x86_64::apic;
    use crate::platform::acpi;
    let s = acpi::summary().expect("ACPI parsed at boot");
    assert!(!s.tables.is_empty(), "no ACPI tables found");
    assert!(
        s.tables.iter().any(|t| t == b"APIC"),
        "MADT missing; tables found: {:?}",
        s.tables
            .iter()
            .map(|t| core::str::from_utf8(t).unwrap_or("????"))
            .collect::<alloc::vec::Vec<_>>()
    );
    assert!(!s.cpu_apic_ids.is_empty(), "MADT lists no processors");
    assert_eq!(
        s.cpu_apic_ids[0],
        u32::from(apic::id()),
        "boot processor APIC id must match the running local APIC"
    );
    assert!(!s.io_apics.is_empty(), "no I/O APIC in the MADT");
    assert!(s.hpet_base.is_some(), "QEMU q35 always has an HPET");
    assert!(
        s.ecam.iter().any(|r| r.segment == 0 && r.bus_start == 0),
        "q35 has an MCFG with segment 0 starting at bus 0"
    );
}

#[test_case]
fn pci_enumeration_finds_virtio_blk_and_net() {
    use crate::platform::pci;
    let devs = pci::devices();
    assert!(!devs.is_empty(), "no PCI functions enumerated");
    assert!(
        devs.iter()
            .any(|d| d.bus == 0 && d.device == 0 && d.vendor == 0x8086),
        "q35 host bridge 00:00.0 (Intel) missing"
    );
    let names: alloc::vec::Vec<&str> = devs.iter().filter_map(|d| d.virtio_name()).collect();
    assert!(
        names.contains(&"virtio-blk"),
        "virtio-blk not found in {names:?}"
    );
    assert!(
        names.contains(&"virtio-net"),
        "virtio-net not found in {names:?}"
    );
    // Class codes must agree with the virtio names: mass storage 01, network 02.
    for d in &devs {
        match d.virtio_name() {
            Some("virtio-blk") => assert_eq!(d.class, 0x01),
            Some("virtio-net") => assert_eq!(d.class, 0x02),
            _ => {}
        }
    }
}
