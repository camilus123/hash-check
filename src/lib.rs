//! hash-check: a GUI file-integrity checker.
//!
//! Module map (see README, "Read-only guarantee"):
//! - [`fsread`]  the only module that opens or lists user files, read-only.
//! - [`appdata`] the only module that writes files, and only in the app's own directory.
//! - [`hasher`]  BLAKE3 / XXH3-128 / SHA-256 behind one streaming interface.
//! - [`hashfile`] hashes one file through `fsread`, detecting changes mid-hash.
//! - [`db`]      SQLite storage with schema versioning and pre-migration backups.
//! - [`verify`]  classification of a re-hashed file; which volume a path is on.
//! - [`report`]  CSV and text verification reports.
//! - [`drives`]  which physical drive a path is on and how it should be read.
//! - [`engine`]  background jobs, scheduled per drive, with progress and cancellation.
//! - [`fonts`]   fallback fonts for characters the built-in font lacks.
//! - [`ui`]      the egui front end.

pub mod appdata;
pub mod db;
pub mod drives;
pub mod engine;
pub mod fonts;
pub mod fsread;
pub mod hasher;
pub mod hashfile;
pub mod report;
pub mod ui;
pub mod util;
pub mod verify;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
