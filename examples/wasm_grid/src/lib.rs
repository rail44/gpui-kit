//! The grid panel `crates/shell/src/tests/benchmark.rs` describes in
//! JavaScript, written as a WebAssembly guest: 40 rows of 5 styled cells and a
//! button, so the wasm guest's description cost can be read next to the
//! script's under the same harness.

use gpui_shell_wasm_guest::prelude::*;

const ROWS: usize = 40;
const COLUMNS: usize = 5;

struct Grid;

fn cell(row: usize, column: usize) -> Element {
    div()
        .style("flex")
        .items_center()
        .style("justify_center")
        .style_with("w", [gpui_shell_wasm_guest::abi::Arg::Number(90.)])
        .h(24.)
        .px(6.)
        .rounded(4.)
        .bg("#f8f8f8")
        .text_color("#111111")
        .style("text_sm")
        .child(text(format!("{row}:{column}")))
}

fn row(row: usize) -> Element {
    h_flex()
        .gap(6.)
        .py(2.)
        .children((0..COLUMNS).map(|column| cell(row, column)))
}

impl View for Grid {
    fn init(_cx: &mut Cx<Self>) -> Self {
        Grid
    }

    // No handler on the button: the script grid has none, and the host test
    // asserts both describe exactly the same tree.
    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex()
            .size_full()
            .p(12.)
            .gap(4.)
            .bg("#ffffff")
            .children((0..ROWS).map(row))
            .child(
                button("refresh")
                    .px(10.)
                    .py(4.)
                    .rounded(6.)
                    .bg("#2563eb")
                    .child(text("Refresh")),
            )
    }
}

export_view!(Grid);
