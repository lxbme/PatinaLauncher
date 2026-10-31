//! `patina-dev` internals shared by the binary and its tests.

#[cfg(not(feature = "dev-offline"))]
const _: () = assert!(!patina_core::DEV_OFFLINE);

pub mod accounts;
pub mod cli;
pub mod follow;
pub mod out;
