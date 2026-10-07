// SPDX-License-Identifier: GPL-2.0-or-later

pub mod calendar;
pub mod config;
pub mod dav;
pub mod eds;
pub mod graph;
pub mod http;
pub mod ical;
pub mod imap;
pub mod oauth;
pub mod photos;
pub mod search;
pub mod secrets;
pub mod service;
pub mod setup;
pub mod smtp;
pub mod store;
pub mod sync;
pub mod timezones;
mod windows_zones;

pub const APP_NAME: &str = "graphmail-bridge";
pub const KEYRING_SERVICE: &str = "dev.graphmail.bridge";
