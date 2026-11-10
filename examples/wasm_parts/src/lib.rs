//! A guest whose view is split into parts: a counter drawn in place as a
//! child view of its own, a confirmation shown as a dialog over the window,
//! and toasts — what a script does with a nested view, `window.open_dialog`
//! and `window.push_toast`. Every part reads the same guest state, and a
//! notify from any of them re-renders all of them.

use gpui_shell_wasm_guest::prelude::*;

struct Parts {
    count: u32,
    counter: Part,
    confirm: Part,
}

impl View for Parts {
    fn init(cx: &mut Cx<Self>) -> Self {
        let counter = cx.part(|this: &mut Parts, cx| {
            h_flex()
                .gap(8.)
                .child(text(format!("Count: {}", this.count)))
                .child(
                    button("bump")
                        .child(text("+1"))
                        .on_click(cx.listener(|this, _, cx| {
                            this.count += 1;
                            cx.notify();
                        })),
                )
        });
        let confirm =
            cx.part(|this: &mut Parts, cx| {
                v_flex()
                    .gap(8.)
                    .p(16.)
                    .child(text(format!("Reset {}?", this.count)))
                    .child(button("reset").child(text("Reset")).on_click(cx.listener(
                        |this, _, cx| {
                            this.count = 0;
                            cx.close_dialog();
                            cx.push_toast(
                                "Reset",
                                Some("The count is back to zero"),
                                ToastLevel::Success,
                            );
                            cx.notify();
                        },
                    )))
            });
        Parts {
            count: 0,
            counter,
            confirm,
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let confirm = self.confirm;
        v_flex()
            .gap(8.)
            .child(text(format!("Total: {}", self.count)))
            .child(nested(self.counter))
            .child(
                button("open")
                    .child(text("Reset…"))
                    .on_click(cx.listener(move |_, _, cx| cx.open_dialog(confirm))),
            )
    }
}

export_view!(Parts);
