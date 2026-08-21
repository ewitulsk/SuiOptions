//! Dakota platform API: client, error mapping and wire types.

pub mod client;
pub mod error;
pub mod types;

pub use client::DakotaClient;
pub use error::{DakotaError, ProblemDetails};
