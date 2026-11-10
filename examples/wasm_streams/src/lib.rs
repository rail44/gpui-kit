//! A guest whose handlers run once per event of a stream: each line a
//! process writes, and each message on a WebSocket (it answers the server's
//! greeting and closes after the reply), the way a script iterates
//! `process.spawn` output and `WebSocket` reads.

use gpui_shell_wasm_guest::{Resource, prelude::*, process_stream, socket, storage};

#[derive(Default)]
struct Streams {
    lines: Vec<String>,
    exit: Option<i64>,
    socket: Option<Resource>,
    messages: Vec<String>,
    closed: bool,
}

impl View for Streams {
    fn init(cx: &mut Cx<Self>) -> Self {
        process_stream::spawn("seq", &["1", "3"], cx, |this, event, cx| {
            if let Some(line) = event.0.get("stdout").and_then(|line| line.as_str()) {
                this.lines.push(line.trim().to_owned());
            }
            if let Some(exit) = event.0.get("exit") {
                this.exit = exit["code"].as_i64();
            }
            cx.notify();
        });
        let socket = storage::get("socket_url").map(|url| {
            socket::open(&url, cx, |this: &mut Streams, event, cx| {
                if let Some(message) = event.0.get("message").and_then(|text| text.as_str()) {
                    this.messages.push(message.to_owned());
                    let socket = this.socket.expect("the socket handled an event");
                    if this.messages.len() == 1 {
                        socket::send(socket, "ping");
                    } else {
                        socket.stop();
                    }
                }
                if event.0.get("close").is_some() {
                    this.closed = true;
                }
                cx.notify();
            })
        });
        Streams {
            socket,
            ..Streams::default()
        }
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex()
            .child(text(format!("Lines: {}", self.lines.join(","))))
            .child(text(format!(
                "Exit: {}",
                self.exit.map(|code| code.to_string()).unwrap_or_default()
            )))
            .child(text(format!("Messages: {}", self.messages.join(","))))
            .child(text(format!("Closed: {}", self.closed)))
    }
}

export_view!(Streams);
