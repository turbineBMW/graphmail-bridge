// SPDX-License-Identifier: GPL-2.0-or-later

pub mod config;
pub mod graph;
pub mod imap;
pub mod oauth;
pub mod photos;
pub mod search;
pub mod secrets;
pub mod service;
pub mod smtp;
pub mod store;
pub mod sync;

pub const APP_NAME: &str = "graphmail-bridge";
pub const KEYRING_SERVICE: &str = "dev.graphmail.bridge";
