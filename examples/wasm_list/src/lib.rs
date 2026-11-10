//! A guest with ten thousand rows, of which the host asks only for the ones
//! on screen, during layout — a script's `list`. Each row has a handler; the
//! last range the host asked for is kept so it can be shown (and checked).

use gpui_shell_wasm_guest::prelude::*;

const ROWS: u32 = 10_000;

struct Rows {
    selected: Option<u32>,
    asked: Option<(u32, u32)>,
}

impl View for Rows {
    fn init(_cx: &mut Cx<Self>) -> Self {
        Rows {
            selected: None,
            asked: None,
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let asked = self
            .asked
            .map(|(start, end)| format!("{start}..{end}"))
            .unwrap_or_default();
        let list = cx.list(
            "rows",
            ROWS,
            |this: &mut Rows, range, cx| {
                this.asked = Some((range.start, range.end));
                range
                    .map(|index| {
                        button(format!("row-{index}"))
                            .h(24.)
                            .child(text(format!("Row {index}")))
                            .on_click(cx.listener(move |this: &mut Rows, _, cx| {
                                this.selected = Some(index);
                                cx.notify();
                            }))
                    })
                    .collect()
            },
            |_, index| format!("row-{index}"),
        );
        v_flex()
            .size_full()
            .child(text(format!(
                "Selected: {}",
                self.selected.map(|row| row.to_string()).unwrap_or_default()
            )))
            .child(text(format!("Asked: {asked}")))
            .child(div().flex_1().child(list))
    }
}

export_view!(Rows);
