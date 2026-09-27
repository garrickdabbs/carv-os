//! Architecture-specific code. Only x86_64 exists today; the module layout leaves room for more.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
