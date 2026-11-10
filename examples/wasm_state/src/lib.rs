//! A guest that owns a registered component's retained state: a
//! `gpui-component` `InputState`, drawn by that catalog's `Input`, read with
//! its `value()` method, cleared with `set_value`, and listened to for
//! `submit` — the calls a script makes on `InputState.new(...)` from
//! `"gpui-component"`.

use gpui_shell_wasm_guest::prelude::*;

struct Echo {
    draft: ComponentState,
    echoed: String,
    submitted: Vec<String>,
}

fn text_of(data: Data) -> String {
    match data {
        Data::String(text) => text,
        _ => String::new(),
    }
}

impl View for Echo {
    fn init(cx: &mut Cx<Self>) -> Self {
        let draft = ComponentState::new("InputState", ["Say something".into()]);
        draft.on("submit", cx, |this, _, cx| {
            let text = text_of(this.draft.call("value", []));
            this.submitted.push(text);
            this.draft.call("set_value", [Data::String(String::new())]);
            cx.notify();
        });
        Echo {
            draft,
            echoed: String::new(),
            submitted: Vec::new(),
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        v_flex()
            .gap(8.)
            .p(16.)
            .child(
                h_flex()
                    .gap(8.)
                    .child(
                        div()
                            .flex_1()
                            .child(component("Input", [(&self.draft).into()])),
                    )
                    .child(
                        component("Button", ["echo".into()])
                            .call("label", ["Echo".into()])
                            .on_click(cx.listener(|this, _, cx| {
                                this.echoed = text_of(this.draft.call("value", []));
                                cx.notify();
                            })),
                    ),
            )
            .child(text(format!("Echo: {}", self.echoed)))
            .child(text(format!("Submitted: {}", self.submitted.join(", "))))
    }
}

export_view!(Echo);
