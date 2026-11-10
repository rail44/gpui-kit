// The grid `crates/shell/src/tests/benchmark.rs` describes, at 40 x 5, as an
// application: the script counterpart of the wasm_grid guest
// (`../src/lib.rs`). `crates/shell/src/engine/comparison.rs` checks that
// both describe the same tree, then times them.
//
//   cargo run -p gpui-shell --no-default-features --features quickjs,components \
//       -- run examples/wasm_grid/app-js

import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button } from "gpui-base";

export default class Grid extends View {
  init() {
    this.rows = 40;
    this.columns = 5;
  }

  cell(row, column) {
    return div()
      .flex()
      .items_center()
      .justify_center()
      .w(90)
      .h(24)
      .px(6)
      .rounded(4)
      .bg(`#f8f8f8`)
      .text_color(`#111111`)
      .text_sm()
      .child(`${row}:${column}`);
  }

  row(row) {
    const cells = [];
    for (let column = 0; column < this.columns; column += 1) {
      cells.push(this.cell(row, column));
    }
    return h_flex().gap(6).py(2).children(cells);
  }

  render(cx) {
    const rows = [];
    for (let row = 0; row < this.rows; row += 1) rows.push(this.row(row));
    return v_flex()
      .size_full()
      .p(12)
      .gap(4)
      .bg(`#ffffff`)
      .children(rows)
      .child(Button.new("refresh").px(10).py(4).rounded(6).bg(`#2563eb`).child("Refresh"));
  }
}
