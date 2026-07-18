//! ZeroClaw WIT tool plugin: `lending_health`.
//!
//! Read-only DeFi lending position health for operator-configured wallets.
//! The pure core lives in [`health`] with no wasm dependency, so it compiles
//! and tests on the host with a plain `cargo test`; the wasm component reuses
//! the same logic through the shim below.
//!
//! Build:  rustup target add wasm32-wasip2
//!         cargo build --target wasm32-wasip2 --release

pub mod health;
