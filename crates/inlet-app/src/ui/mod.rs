// SPDX-License-Identifier: AGPL-3.0-or-later
//! Everything the window draws, one module per domain. Each domain adds `impl Inlet`
//! blocks for its views and for the events of its backend domain, and keeps its view
//! state in its own struct, so tasks that work on different domains do not edit the same
//! files.

pub mod accounts;
pub mod chat;
pub mod composer;
pub mod inbox;
pub mod markdown;
pub mod media;
pub mod members;
pub mod notifications;
pub mod overlays;
pub mod profile;
pub mod rail;
pub mod root;
pub mod search;
pub mod settings;
pub mod sidebar;
pub mod social;
pub mod threads;
pub mod voice;
pub mod widgets;

/// Height of the guild header and the channel header.
pub const HEADER_HEIGHT: f32 = 56.;
