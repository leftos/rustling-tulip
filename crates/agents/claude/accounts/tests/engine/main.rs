//! The claude-accounts crate's auto-switch engine files: cswap's state file and the engine lease, against a scratch
//! account store.

#![cfg_attr(
    all(windows, target_pointer_width = "64"),
    expect(
        clippy::expect_used,
        reason = "a test that cannot set up its files or take its lock must fail loudly"
    )
)]

#[cfg(all(windows, target_pointer_width = "64"))]
mod balance;
mod cadence;
#[cfg(all(windows, target_pointer_width = "64"))]
mod cadence_tick;
#[cfg(all(windows, target_pointer_width = "64"))]
mod collect;
#[cfg(all(windows, target_pointer_width = "64"))]
mod lease;
#[cfg(all(windows, target_pointer_width = "64"))]
mod paid;
#[cfg(all(windows, target_pointer_width = "64"))]
mod rank;
#[cfg(all(windows, target_pointer_width = "64"))]
mod reading;
#[cfg(all(windows, target_pointer_width = "64"))]
mod state;
#[cfg(all(windows, target_pointer_width = "64"))]
mod support;
#[cfg(all(windows, target_pointer_width = "64"))]
mod tick;
#[cfg(all(windows, target_pointer_width = "64"))]
mod trigger;
