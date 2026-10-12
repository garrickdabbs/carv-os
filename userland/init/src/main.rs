//! Minimal CarvOS root task.
//!
//! On the kernel target this binary starts in ring 3, exercises the initial root-budget authority by
//! creating an endpoint, then blocks forever receiving on that endpoint. It is built on `carv-rt`
//! (P3.1). On host targets it builds as an empty stub so workspace host checks can include the
//! crate.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
carv_rt::entry!(main);

/// The root task: report in, create an endpoint, then idle receiving on it.
#[cfg(target_os = "none")]
fn main() {
    use carv_rt::carv_abi::{self, Message, ObjectType};
    use carv_rt::{println, sys};

    println!("init: hello from ring 3");

    let endpoint = carv_abi::init::FIRST_FREE;
    let created = sys::budget_create(
        carv_abi::init::ROOT_BUDGET,
        ObjectType::Endpoint,
        endpoint,
        0,
        0,
    );
    let described = created.and_then(|_| sys::describe(endpoint));
    if described.is_ok_and(|d| d.object_type == Some(ObjectType::Endpoint)) {
        println!("init: created an endpoint via invoke");
    } else {
        println!("init: invoke failed");
    }

    println!("init: idle");
    let mut message = Message::default();
    loop {
        let _ = sys::recv(endpoint, &mut message);
    }
}
