//! The network services both engines offer, below the language binding: TCP
//! connection with resolver deadlines, HTTP with its redirect checks, and the
//! WebSocket handshake. Each checks the application's grants itself, so a
//! binding cannot reach the network without asking.

pub(crate) mod connect;
pub(crate) mod http;
pub(crate) mod websocket;

/// What an HTTP request and the WebSocket handshake send when the caller names
/// no `User-Agent`. Some APIs (GitHub's among them) refuse a request without
/// one.
pub(crate) const USER_AGENT: &str = concat!("gpui-shell/", env!("CARGO_PKG_VERSION"));
