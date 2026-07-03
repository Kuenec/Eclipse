//! The ONE place the cross-crate source sharing lives: this module `#[path]`-includes the
//! root crate's `src/webview/*` leaf modules VERBATIM, so the wire protocol, the URL-redaction
//! contract, the SCM_RIGHTS fd passing, the memfd mapping guard, and the slot state machine
//! have exactly one implementation ("integrates, never duplicates").
//!
//! 2026-07-03 SIBLING-MODULE-SHAPE INVARIANT: the included files reference each other ONLY as
//! siblings (`super::redact`, `super::proto`, `super::PROTO_V1`, …) and use std/libc only —
//! never `crate::` paths — so they resolve identically under `crate::webview` in the root and
//! under `crate::shared` here. This module must therefore declare the SAME sibling set as
//! `src/webview/mod.rs` (including [`PROTO_V1`]). Their `#[cfg(test)]` units compile and run
//! under BOTH crates' `cargo test` — same code, two gates (deliberate parity insurance).

#[path = "../../../src/webview/fdpass.rs"]
pub mod fdpass;
#[path = "../../../src/webview/proto.rs"]
pub mod proto;
#[path = "../../../src/webview/redact.rs"]
pub mod redact;
#[path = "../../../src/webview/shm.rs"]
pub mod shm;
#[path = "../../../src/webview/slots.rs"]
pub mod slots;

/// Wire-protocol version 1 — MUST mirror `src/webview/mod.rs::PROTO_V1` (the sibling-shape
/// invariant above; `proto.rs` resolves it as `super::PROTO_V1` in both crates).
pub const PROTO_V1: u16 = 1;
