//! The headless client driver, compiled only with the `driver` feature.
//!
//! [`registry`] knows where every element named with
//! [`crate::driver_id::DriverId`] was painted in each window's latest frame.

pub mod registry;
