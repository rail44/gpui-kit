//! A guest whose render passes `Progress` a number where its constructor
//! declares a string id. The host must refuse it with the schema's message,
//! as it refuses the same mistake from a script, rather than hand the adapter
//! a value it never agreed to decode.

use gpui_shell_wasm_guest::prelude::*;

struct BadArgs;

impl View for BadArgs {
    fn init(_cx: &mut Cx<Self>) -> Self {
        BadArgs
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex().child(component("Progress", [CArg::Number(1.)]))
    }
}

export_view!(BadArgs);
