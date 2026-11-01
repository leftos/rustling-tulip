//! The headless client driver, compiled only with the `driver` feature.
//!
//! [`registry`] knows where every element named with
//! [`crate::driver_id::DriverId`] was painted in each window's latest frame;
//! [`protocol`] is the wire format of a driver request and its reply, and
//! [`handlers`] answers each request against the main window.

pub mod handlers;
pub mod protocol;
pub mod registry;
