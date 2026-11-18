//! Siphon library: CLI command grammar, domain types, service contracts,
//! cookie loading, and platform refresh clients.
//!
//! Downstream modules implement:
//! - [`storage::Storage`] for local SQLite persistence (`docs/database.md`)
//! - [`refresh::RefreshSource`] / [`platform::bilibili::BilibiliRefreshSource`] for Bilibili creator resolution and feed pagination
//! - [`download::Downloader`] for `yt-dlp` execution and output artifact verification

#![warn(missing_docs)]

pub mod app;
pub mod cli;
pub mod config;
pub mod cookies;
pub mod domain;
pub mod download;
pub mod error;
pub mod output;
pub mod platform;
pub mod refresh;
pub mod storage;
