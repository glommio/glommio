//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! Synchronisation primitives for code shared across threads, taken from loom
//! when built with `--cfg loom` so its interleavings can be model checked.

#[cfg(loom)]
pub(crate) use loom::sync::{
    atomic::{fence, AtomicBool, AtomicU64},
    Mutex,
};
#[cfg(not(loom))]
pub(crate) use std::sync::{atomic::AtomicBool, Mutex};
