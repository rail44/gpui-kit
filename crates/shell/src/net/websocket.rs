//! The WebSocket half of the network services that does not depend on who
//! asked: the limits, the URL and grant check, the handshake, and the socket
//! timeouts. A script's `WebSocket` and a guest's `socket` both use it.

use std::{net::TcpStream, time::Duration};

use tungstenite::{
    WebSocket, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

use super::connect as network_connect;

pub(crate) const MESSAGE_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const READ_SLICE: Duration = Duration::from_millis(50);
#[cfg(not(test))]
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(crate) const IO_TIMEOUT: Duration = Duration::from_millis(100);

/// What a WebSocket a caller that is not a script — a WebAssembly guest —
/// holds: the connected socket, with the read slice set so its owner can
/// interleave reads and writes on one thread.
#[cfg_attr(not(feature = "wasm-engine"), allow(dead_code))]
pub(crate) type GuestSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// `WebSocket.connect`'s URL check, authorization and handshake for a caller
/// that is not a script. The authorization runs here, on the calling thread
/// (the caller passes the grants), so a denial fails the call that made it;
/// the connection is the caller's to make off the main thread with
/// [`guest_handshake`].
#[cfg_attr(not(feature = "wasm-engine"), allow(dead_code))]
pub(crate) fn authorize_guest_socket(
    capabilities: &crate::Capabilities,
    url: &str,
) -> std::result::Result<(String, String, u16), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("invalid WebSocket URL: {error}"))?;
    if !matches!(parsed.scheme(), "ws" | "wss") {
        return Err("a WebSocket expects a ws:// or wss:// URL".to_owned());
    }
    let host_name = parsed
        .host_str()
        .ok_or("WebSocket URL has no host")?
        .to_owned();
    if !capabilities.may_reach(&host_name) {
        return Err(format!(
            "network access to `{host_name}` is not granted; add it to capabilities.network.hosts"
        ));
    }
    let port = parsed
        .port_or_known_default()
        .ok_or("WebSocket URL has no port and no known default")?;
    Ok((url.to_owned(), host_name, port))
}

/// The handshake `WebSocket.connect` performs. Blocking.
#[cfg_attr(not(feature = "wasm-engine"), allow(dead_code))]
pub(crate) fn guest_handshake(
    url: String,
    host_name: String,
    port: u16,
) -> std::result::Result<GuestSocket, String> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MESSAGE_LIMIT))
        .max_frame_size(Some(MESSAGE_LIMIT));
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("WebSocket handshake failed: {error}"))?;
    request
        .headers_mut()
        .entry(tungstenite::http::header::USER_AGENT)
        .or_insert(HeaderValue::from_static(super::USER_AGENT));
    let operation = format!("WebSocket host {host_name}:{port}");
    let (stream, _deadline) =
        network_connect::connect_tcp(&host_name, port, IO_TIMEOUT, &operation)
            .map_err(|error| format!("WebSocket connection failed: {error}"))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("setting WebSocket handshake timeout failed: {error}"))?;
    let (mut socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|error| format!("WebSocket handshake failed: {error}"))?;
    set_timeouts(&mut socket)?;
    Ok(socket)
}

pub(crate) fn set_timeouts(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> std::result::Result<(), String> {
    let stream = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &mut stream.sock,
        _ => return Err("WebSocket transport does not support read timeouts".to_owned()),
    };
    stream
        .set_read_timeout(Some(READ_SLICE))
        .map_err(|error| format!("setting WebSocket read timeout failed: {error}"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("setting WebSocket write timeout failed: {error}"))
}
