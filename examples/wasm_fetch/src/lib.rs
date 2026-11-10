//! A guest that fetches the URL its persistent store names and shows the
//! response. The request returns at once and the handler runs when the
//! response arrives — the guest never blocks the frame — and it is refused
//! like a script's `fetch` when the application's network grants do not
//! cover the host.

use gpui_shell_wasm_guest::{http, prelude::*, storage};

struct Fetch {
    status: String,
    body: String,
}

impl View for Fetch {
    fn init(cx: &mut Cx<Self>) -> Self {
        if let Some(url) = storage::get("url") {
            http::get(&url, cx, |this, event, cx| {
                let response = &event.0;
                match response.get("error").and_then(|error| error.as_str()) {
                    Some(error) => this.status = format!("failed: {error}"),
                    None => {
                        this.status = response["status"].to_string();
                        this.body = response["body"].as_str().unwrap_or_default().to_owned();
                    }
                }
                cx.notify();
            });
        }
        Fetch {
            status: "pending".into(),
            body: String::new(),
        }
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex()
            .child(text(format!("Status: {}", self.status)))
            .child(text(format!("Body: {}", self.body)))
    }
}

export_view!(Fetch);
