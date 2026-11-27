//! When `JobFuture<T>` is `Send`, `Sync` and `Unpin` (ROADMAP 10.1.36; ADR-0064's 2026-11-27 amendment): `Send` and
//! `Sync` exactly when `T: Send`, `Unpin` for every `T`. These are the bounds the public-API snapshot states for it
//! (`STATED_AUTO_TRAIT_BOUNDS` in `xtask/src/public_api/render.rs`), which rustdoc cannot write in public terms: the
//! positive cases are checked by the compiler for any `T`, the negative ones by a probe whose answer is a constant
//! (a bound on an inherent associated constant, which a trait's constant of the same name stands in for when the bound
//! does not hold), so a change of the conditions fails to compile here.
#![cfg(feature = "async")]

use std::cell::Cell;
use std::marker::{PhantomData, PhantomPinned};
use std::rc::Rc;
use std::sync::MutexGuard;

use persia::JobFuture;

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}
fn assert_unpin<T: Unpin>() {}

/// `Send` and `Sync` for every `T: Send`: the compiler proves it for the parameter, so for every such type.
fn send_and_sync_when_t_is_send<T: Send>() {
    assert_send::<JobFuture<T>>();
    assert_sync::<JobFuture<T>>();
}

/// `Unpin` for every `T`, even one that is neither `Send` nor `Unpin`.
fn unpin_for_any_t<T>() {
    assert_unpin::<JobFuture<T>>();
}

/// The probe: `Is<T>::SEND` is `true` when `T: Send`, else the `false` of [`NotSend`]; the same for `Sync`.
struct Is<T: ?Sized>(PhantomData<T>);

trait NotSend {
    const SEND: bool = false;
}
impl<T: ?Sized> NotSend for T {}

trait NotSync {
    const SYNC: bool = false;
}
impl<T: ?Sized> NotSync for T {}

impl<T: ?Sized + Send> Is<T> {
    const SEND: bool = true;
}

impl<T: ?Sized + Sync> Is<T> {
    const SYNC: bool = true;
}

// The probe tells the two apart: it is not always `true` or always `false`.
const _: () = assert!(Is::<u8>::SEND && Is::<u8>::SYNC);
const _: () = assert!(!Is::<Rc<u8>>::SEND && !Is::<Rc<u8>>::SYNC);
const _: () = assert!(Is::<Cell<u8>>::SEND && !Is::<Cell<u8>>::SYNC);
const _: () = assert!(!Is::<MutexGuard<'static, u8>>::SEND && Is::<MutexGuard<'static, u8>>::SYNC);

// `T` neither `Send` nor `Sync`: the future is neither.
const _: () = assert!(!Is::<JobFuture<Rc<u8>>>::SEND);
const _: () = assert!(!Is::<JobFuture<Rc<u8>>>::SYNC);
// `T` `Sync` but not `Send`: neither. `Sync` follows `T: Send`, not `T: Sync` (the result moves between threads
// through the slot's mutex).
const _: () = assert!(!Is::<JobFuture<MutexGuard<'static, u8>>>::SEND);
const _: () = assert!(!Is::<JobFuture<MutexGuard<'static, u8>>>::SYNC);
// `T` `Send` but not `Sync`: both.
const _: () = assert!(Is::<JobFuture<Cell<u8>>>::SEND);
const _: () = assert!(Is::<JobFuture<Cell<u8>>>::SYNC);

#[test]
fn a_job_future_is_send_and_sync_when_its_output_is_send_and_unpin_always() {
    // The futures the API returns.
    send_and_sync_when_t_is_send::<()>();
    send_and_sync_when_t_is_send::<persia::AsyncDb>();
    send_and_sync_when_t_is_send::<Option<persia::Document>>();
    send_and_sync_when_t_is_send::<persia::SearchResponse>();
    // `Send` but not `Sync`.
    send_and_sync_when_t_is_send::<Cell<u8>>();
    unpin_for_any_t::<()>();
    unpin_for_any_t::<Rc<u8>>();
    unpin_for_any_t::<PhantomPinned>();
}
