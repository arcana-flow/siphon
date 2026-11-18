//! Siphon foundation library: CLI command grammar, domain types, and service contracts.
//!
//! Downstream tasks implement:
//! - [`storage::Storage`] for local SQLite persistence (`docs/database.md`)
//! - [`refresh::RefreshSource`] for Bilibili creator resolution and feed pagination
//! - [`download::Downloader`] for `yt-dlp` execution and output artifact verification

#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod config;
pub mod domain;
pub mod download;
pub mod error;
pub mod refresh;
pub mod storage;
