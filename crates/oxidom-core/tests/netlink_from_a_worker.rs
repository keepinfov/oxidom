//! `Net` is built and used on whatever thread asks for it: a plain CLI thread,
//! or — in the daemon — one of zbus's tokio workers. Both cost an evening of
//! live debugging once, because a netlink read is never exercised by a unit
//! test and neither failure is a compile error:
//!
//! * `rtnetlink::new_connection` registers its socket with the reactor current
//!   on the calling thread. Off a runtime that is a panic; on a foreign runtime
//!   the socket answers to a reactor nobody drives and every exchange hangs.
//! * `Runtime::block_on` from inside a runtime aborts the caller, and dropping
//!   a `Runtime` there panics too.
//!
//! These reads need no privileges. A machine that refuses netlink entirely
//! returns an error rather than panicking, and that case is skipped: the point
//! here is the threading contract, not the routing table.

use oxidom_core::tun::net::Net;

fn exercise(label: &str) {
    let Ok(net) = Net::new() else {
        eprintln!("skipping {label}: this machine does not allow netlink");
        return;
    };
    // More than one exchange: the reactor only advances while `block_on` runs,
    // so a second read is where a mis-parked connection task would surface.
    // What each read answers is the environment's business — a build sandbox
    // has no default route. Only completing at all is this test's business.
    for _ in 0..3 {
        let _ = net.default_network();
        let _ = net.link_index("lo");
    }
    eprintln!("{label}: six netlink exchanges completed");
}

#[test]
fn netlink_works_off_any_runtime() {
    exercise("a plain thread");
}

#[test]
fn netlink_works_on_a_multi_threaded_worker() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("building the stand-in for the D-Bus runtime");
    runtime.block_on(async {
        tokio::task::spawn(async { exercise("a tokio worker") })
            .await
            .expect("the worker task must not panic")
    });
}
