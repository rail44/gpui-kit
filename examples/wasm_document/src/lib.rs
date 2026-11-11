//! A Markdown document viewer, as a guest built for `wasm32-wasip2`: the
//! shape of a view an embedding application hands to an extension.
//!
//! It shows what such a guest draws on beyond gpui-shell's own boundary:
//!
//! * **WASI** for the file: `std::fs` reads it, inside the directories the
//!   application's file grants preopen.
//! * **The embedding host's own interface**, imported beside gpui-shell's
//!   (`example:document/host`): which document to open, and a report of each
//!   one shown, as a host would record what a pane holds.
//! * **Events gpui-shell delivers**: a link clicked in the document (a
//!   relative one is followed in place, with the way back kept) and keys
//!   while the view has focus (Backspace goes back).

use std::path::{Path, PathBuf};

use gpui_shell_wasm_guest::{export_view_retained, prelude::*};

wit_bindgen::generate!({
    inline: r#"
        package example:document@0.1.0;

        interface host {
            /// The document to show first.
            initial: func() -> string;
            /// Reports the document now shown.
            opened: func(path: string);
        }

        world imports {
            import host;
        }
    "#,
});

use example::document::host;

struct Document {
    path: PathBuf,
    /// The documents left by following links, most recent last.
    history: Vec<PathBuf>,
    body: Result<String, String>,
    focus: FocusHandle,
}

impl Document {
    fn show(&mut self, path: PathBuf) {
        self.body = std::fs::read_to_string(&path).map_err(|error| error.to_string());
        host::opened(&path.to_string_lossy());
        self.path = path;
    }

    fn follow(&mut self, target: &str) {
        // Only another document beside this one; anything else is left be.
        if target.contains("://") || target.starts_with('#') {
            return;
        }
        let next = self.path.parent().unwrap_or(Path::new("/")).join(target);
        let previous = std::mem::take(&mut self.path);
        self.history.push(previous);
        self.show(next);
    }

    fn back(&mut self) -> bool {
        match self.history.pop() {
            Some(previous) => {
                self.show(previous);
                true
            }
            None => false,
        }
    }
}

impl View for Document {
    fn init(_cx: &mut Cx<Self>) -> Self {
        let mut document = Document {
            path: PathBuf::new(),
            history: Vec::new(),
            body: Ok(String::new()),
            focus: FocusHandle::new(),
        };
        document.show(PathBuf::from(host::initial()));
        document
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let body = match &self.body {
            Ok(markdown) => text_view("document", markdown.clone()).on_link_click(cx.listener(
                |this, event, cx| {
                    if let Some(target) = event.0.as_str() {
                        this.follow(target);
                        cx.notify();
                    }
                },
            )),
            Err(error) => div().text_color("danger").child(text(error.clone())),
        };
        v_flex()
            .size_full()
            .p(16.)
            .gap(8.)
            .bg("background")
            .text_color("foreground")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, cx| {
                if event.0["key"] == "backspace" && this.back() {
                    cx.notify();
                }
            }))
            .child(div().text_color("muted_foreground").child(text(format!(
                "{} ({} back)",
                self.path.display(),
                self.history.len()
            ))))
            .child(
                div()
                    .id("document-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

export_view_retained!(Document);
