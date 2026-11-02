//! The headless client driver, compiled only with the `driver` feature.
//!
//! [`registry`] knows where every element named with
//! [`crate::driver_id::DriverId`] was painted in each window's latest frame;
//! [`protocol`] is the wire format of a driver request and its reply, and
//! [`handlers`] answers each request against the main window. On Windows,
//! [`endpoint`] serves those requests over [`pipe`] in a driven run and
//! answers `screenshot` through [`capture`].

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod endpoint;
pub mod handlers;
#[cfg(windows)]
pub mod pipe;
pub mod protocol;
pub mod registry;
