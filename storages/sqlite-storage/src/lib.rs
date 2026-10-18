//! SQLite storage backend for whatsapp-rust
//!
//! This crate provides a SQLite-based storage implementation for the whatsapp-rust library.
//! It implements all the required storage traits from wacore::store::traits.

#[cfg(unix)]
mod nofollow;
mod schema;
mod sqlite_store;

pub use sqlite_store::SqliteStore;
