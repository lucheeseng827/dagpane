//! The pointer boundary, and nothing else.
//!
//! # The contract, completely
//!
//! Five exports. Every one that returns anything returns **one pointer** to a buffer laid out
//! as a little-endian `u32` length followed by that many bytes of UTF-8 JSON. The caller reads
//! the length, reads the body, and hands the whole buffer back to [`dp_free`] with
//! `4 + length`.
//!
//! ```text
//! dp_alloc(len)            -> ptr            a buffer for the host to write a request into
//! dp_free(ptr, len)                          give any buffer back, whoever allocated it
//! dp_open(ptr, len)        -> ptr            compile the app; the reply is its `init` frame
//! dp_send(ptr, len)        -> ptr            one ClientMessage in, one ServerMessage out
//! ```
//!
//! One length prefix rather than a second call for the length: a `dp_last_len()` would be a
//! second piece of state to get wrong, and the wrong answer there is a torn string rather than
//! an error.
//!
//! # Why this is hand-written
//!
//! See `Cargo.toml`. The short version is that the interface is one string in and one string
//! out, `wasm-bindgen` costs a build step and a matched CLI in a repository that has neither,
//! and about forty lines each side is the whole price of not having them.
//!
//! # The unsafe
//!
//! This module is the only place in the tree that is not `#![forbid(unsafe_code)]`, and the
//! blocks below are the entire list. Each one turns a `(ptr, len)` pair the host chose into a
//! slice or a `Vec`, which is exactly the obligation a linear-memory ABI cannot avoid and must
//! therefore state:
//!
//!   * a pointer given to [`dp_open`], [`dp_send`] or [`dp_deliver`] must be one
//!     [`dp_alloc`] returned, with
//!     the same length, still owned by the caller;
//!   * a pointer given to [`dp_free`] must be one this module returned, with the length that
//!     came with it — for a reply buffer, `4 + the prefix`;
//!   * every pointer is used once and not kept.
//!
//! `dagpane.js` is the only caller in this repository and it holds all three. A host that
//! writes its own must too; there is no way to check from this side, which is what "unsafe"
//! means here.
//!
//! # Single-threaded on purpose
//!
//! One session per module instance, in a `thread_local`. wasm32 without the threads proposal
//! has one thread, a browser page showing two apps instantiates the module twice — which
//! isolates their memory, and is a feature rather than a workaround — and running many
//! sessions over one compiled app is what `dagpane-serve` is for. A handle table here would
//! be a second multi-tenancy implementation with no tenant in it.

use std::cell::RefCell;

use crate::engine::{open_json, Engine};

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// A buffer holding `len` as four little-endian bytes, then the body.
///
/// **`Box<[u8]>`, not `Vec<u8>`, and that is a correctness requirement rather than a style.**
/// Freeing an allocation requires the layout it was made with, and a `Vec` carries a capacity
/// that is only guaranteed to be *at least* its length — `with_capacity` may round up, and
/// `shrink_to_fit` is explicitly permitted to leave spare capacity. [`dp_free`] only receives
/// a length, so reconstructing a `Vec` with `capacity == len` would hand the allocator the
/// wrong layout whenever the real capacity differed. That is undefined behaviour, it would
/// have been invisible on every allocator that happens to return exact sizes, and an earlier
/// version of this file did it while asserting in a comment that the capacity was exact.
///
/// A boxed slice has no capacity to disagree with: its layout *is* `len` bytes, so
/// `(ptr, len)` is enough to free it exactly.
fn framed(body: String) -> *mut u8 {
    let bytes = body.into_bytes();
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
    Box::into_raw(out.into_boxed_slice()).cast::<u8>()
}

/// The JSON at `(ptr, len)`, or `None` if those bytes are not UTF-8.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes, as the module docs require.
unsafe fn borrow<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).ok()
}

/// A buffer of `len` bytes for the host to write into.
///
/// Zeroed rather than uninitialised, and boxed rather than a `Vec`, for the reason
/// [`framed`] gives: the allocation's layout has to be recoverable from `(ptr, len)` alone.
/// The zeroing is what `into_boxed_slice` costs on an unfilled buffer and it buys the
/// exactness; a message-sized `memset` is not where this crate's time goes.
///
/// # Safety
///
/// The returned pointer must be given back to [`dp_free`] with the same `len`.
#[no_mangle]
pub extern "C" fn dp_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast::<u8>()
}

/// Give a buffer back.
///
/// # Safety
///
/// `ptr` must be one this module returned and `len` the length that came with it: `len` as
/// passed to [`dp_alloc`], or `4 + the length prefix` for a reply buffer.
#[no_mangle]
pub unsafe extern "C" fn dp_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}

/// Compile an app from a [`crate::Config`] and return its opening frame.
///
/// Replaces whatever app this instance held. A reply of `{"type":"failed",…}` means nothing
/// was opened and the previous app, if any, is gone — a half-open instance would be a state
/// with no way to name it.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn dp_open(ptr: *const u8, len: usize) -> *mut u8 {
    let Some(config) = borrow(ptr, len) else {
        return framed(crate::failed("the config is not UTF-8"));
    };
    let (engine, reply) = open_json(config);
    ENGINE.with(|slot| *slot.borrow_mut() = engine);
    framed(reply)
}

/// Answer one `ClientMessage`.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn dp_send(ptr: *const u8, len: usize) -> *mut u8 {
    let Some(request) = borrow(ptr, len) else {
        return framed(crate::failed("the message is not UTF-8"));
    };
    framed(ENGINE.with(|slot| match slot.borrow_mut().as_mut() {
        Some(engine) => engine.handle(request),
        None => crate::failed("no app is open in this instance; call dp_open first"),
    }))
}

/// Hand this half a `ServerMessage` from the other one, and get back its own patch.
///
/// Only a split app uses this: `dp_open` with `side: "client"` opens the page's half, and
/// every `init` or `patch` the socket delivers goes through here. A message carrying no
/// frontier answers with an empty patch rather than an error, so a host may forward
/// everything and never decide what to forward.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn dp_deliver(ptr: *const u8, len: usize) -> *mut u8 {
    let Some(request) = borrow(ptr, len) else {
        return framed(crate::failed("the server message is not UTF-8"));
    };
    framed(ENGINE.with(|slot| match slot.borrow_mut().as_mut() {
        Some(engine) => engine.deliver(request),
        None => crate::failed("no app is open in this instance; call dp_open first"),
    }))
}
