//! Use cases and ports; runtime and provider implementations belong in adapters.
pub mod models;
pub mod ports;
pub use models::*;
pub use ports::*;
mod engine;
pub use engine::*;
mod control;
mod release;
pub use release::SendResult;
mod maintenance;
mod presentation;
pub use presentation::{delivery_blocks, split_text};
mod product_models;
pub use product_models::*;
mod contacts;
mod deletion;
mod drafts;
mod overview;
