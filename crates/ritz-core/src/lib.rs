//! ritz-core — pure logic for the Ritz game launcher.
//!
//! No egui, no threads. Everything here is unit-testable: the extension schema,
//! the `Requires` condition grammar, variable resolution/interpolation, the
//! launch-command builder, and the Steam `%command%` parser.

pub mod builder;
pub mod condition;
pub mod config;
pub mod error;
pub mod extension;
pub mod lsfg_toml;
pub mod resolve;
pub mod schema;
pub mod steam;
pub mod variables;

pub use error::{Result, RitzError};
