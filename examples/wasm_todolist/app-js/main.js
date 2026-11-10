// The wasm_todolist guest (`../src/lib.rs`) written as a script: the same
// view, element for element, so the two engines can be looked at and timed
// side by side. `crates/shell/src/engine/comparison.rs` checks that both
// describe the same tree.
//
//   cargo run -p gpui-shell --no-default-features --features quickjs,components \
//       -- run examples/wasm_todolist/app-js

import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button, Checkbox, Input, InputState } from "gpui-base";
import { Tag, Progress, Button as StyledButton } from "gpui-component";

export default class TodoList extends View {
  init(_props, _cx) {
    this.draft = InputState.new({ placeholder: "What needs doing?" });
    // Enter is how a list like this is actually used.
    this.draft.on("submit", (_event, cx) => {
      this.add();
      cx.notify();
    });
    this.items = [];
    this.filter = "All";
    this.nextId = 1;
  }

  add() {
    const caption = this.draft.value().trim();
    if (caption === "") return;
    this.items.push({ id: this.nextId, caption, done: false });
    this.nextId += 1;
    this.draft.set_value("");
  }

  remaining() {
    return this.items.filter((item) => !item.done).length;
  }

  percentDone() {
    if (this.items.length === 0) return 0;
    return ((this.items.length - this.remaining()) * 100) / this.items.length;
  }

  visible() {
    return this.items.filter((item) =>
      this.filter === "All" ? true : this.filter === "Active" ? !item.done : item.done,
    );
  }

  row(item, cx) {
    const colors = cx.theme().colors;
    return h_flex()
      .items_center()
      .gap(8)
      .px(12)
      .py(6)
      .hover((style) => style.bg(colors.muted))
      .child(
        Checkbox.new(`done-${item.id}`)
          .checked(item.done)
          .on_change((done, cx) => {
            const found = this.items.find((other) => other.id === item.id);
            if (found) found.done = done;
            cx.notify();
          }),
      )
      .child(div().flex_1().child(item.caption))
      .child(
        ghostButton(`remove-${item.id}`, "Remove", cx).on_click((_event, cx) => {
          this.items = this.items.filter((other) => other.id !== item.id);
          cx.notify();
        }),
      );
  }

  filterButton(filter, cx) {
    const button =
      this.filter === filter
        ? primaryButton(`filter-${filter}`, filter, cx)
        : ghostButton(`filter-${filter}`, filter, cx);
    return button.on_click((_event, cx) => {
      this.filter = filter;
      cx.notify();
    });
  }

  render(cx) {
    const colors = cx.theme().colors;
    const rows = this.visible().map((item) => this.row(item, cx));
    const summary =
      this.items.length === 0
        ? "Nothing yet"
        : `${this.remaining()} of ${this.items.length} remaining`;

    return v_flex()
      .size_full()
      .gap(12)
      .p(16)
      .bg(colors.background)
      .text_color(colors.foreground)
      .child(
        h_flex()
          .items_center()
          .justify_between()
          .child(div().text_size(18).child("Todo"))
          .child(Tag.new().variant("secondary").child(summary)),
      )
      .child(
        h_flex()
          .gap(8)
          .child(div().flex_1().child(Input.new(this.draft)))
          .child(
            StyledButton.new("add")
              .label("Add")
              .primary()
              .on_click((_event, cx) => {
                this.add();
                cx.notify();
              }),
          ),
      )
      .child(Progress.new("done-progress").value(this.percentDone()))
      .child(
        h_flex()
          .gap(4)
          .child(this.filterButton("All", cx))
          .child(this.filterButton("Active", cx))
          .child(this.filterButton("Done", cx)),
      )
      .child(v_flex().border(1).border_color(colors.border).rounded(8).children(rows));
  }
}

// `gpui-base` buttons carry no look of their own; these two are the
// example's, as the guest's are.
function primaryButton(id, caption, cx) {
  const colors = cx.theme().colors;
  return Button.new(id)
    .px(12)
    .py(6)
    .rounded(6)
    .bg(colors.primary)
    .text_color(colors.primary_foreground)
    .cursor_pointer()
    .child(caption);
}

function ghostButton(id, caption, cx) {
  return Button.new(id)
    .px(12)
    .py(6)
    .rounded(6)
    .text_color(cx.theme().colors.muted_foreground)
    .cursor_pointer()
    .child(caption);
}
