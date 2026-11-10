//! A guest that reaches the rest of the host services a script has: session
//! storage, the clipboard, a process, an HTTP request with a method, headers
//! and a body, files read and written off the main thread, and a registered
//! component's named slot. The asynchronous ones return at once and fill the
//! view in as their handlers run.

use gpui_shell_wasm_guest::{clipboard, fs, http, prelude::*, process, session_storage, storage};

#[derive(Default)]
struct Services {
    session: String,
    clipboard: String,
    process: String,
    http: String,
    notes: String,
    wrote: bool,
}

fn field(event: &Event, key: &str) -> String {
    match event.0.get("error").and_then(|error| error.as_str()) {
        Some(error) => format!("failed: {error}"),
        None => event.0[key].as_str().unwrap_or_default().to_owned(),
    }
}

impl View for Services {
    fn init(cx: &mut Cx<Self>) -> Self {
        session_storage::set("greeting", "hello session");
        clipboard::write("copied text");
        let services = Services {
            session: session_storage::get("greeting").unwrap_or_default(),
            clipboard: clipboard::read().unwrap_or_default(),
            ..Services::default()
        };

        process::run("echo", &["from a process"], cx, |this, event, cx| {
            this.process = field(event, "stdout").trim().to_owned();
            cx.notify();
        });
        if let Some(url) = storage::get("url") {
            http::request(
                "POST",
                &url,
                &[("x-guest", "wasm")],
                "ping",
                cx,
                |this, event, cx| {
                    this.http = format!("{} {}", event.0["status"], field(event, "body"));
                    cx.notify();
                },
            );
        }
        if let Some(path) = storage::get("notes_path") {
            fs::read_text_async(&path, cx, |this, event, cx| {
                this.notes = field(event, "text");
                cx.notify();
            });
            fs::write_text_async(&format!("{path}.async"), "ASYNC", cx, |this, event, cx| {
                this.wrote = event.0.get("ok").is_some();
                cx.notify();
            });
        }
        services
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex()
            .child(text(format!("Session: {}", self.session)))
            .child(text(format!("Clipboard: {}", self.clipboard)))
            .child(text(format!("Process: {}", self.process)))
            .child(text(format!("Http: {}", self.http)))
            .child(text(format!("Notes: {}", self.notes)))
            .child(text(format!("Wrote: {}", self.wrote)))
            // A registered component's named slot: the content is rendered
            // by the Collapsible, not among its children.
            .child(
                component("Collapsible", [])
                    .call("open", [true.into()])
                    .child(text("trigger"))
                    .slot("content", text("revealed")),
            )
    }
}

export_view!(Services);
