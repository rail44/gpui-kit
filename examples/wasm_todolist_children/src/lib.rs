//! `examples/wasm_todolist_render` with each row a child view, the way a
//! GPUI view splits itself into entities: a row holds its own todo and
//! renders only itself, so ticking one renders that row again and the list
//! around it, which keeps the other rows where they are without rendering
//! them. A row tells the list what happened with `cx.emit`; the list hears
//! it through `cx.subscribe`.

use gpui_shell_wasm_guest::{export_view_retained, prelude::*};

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Active,
    Done,
}

/// What a row tells the list.
enum RowEvent {
    /// Its todo was ticked or cleared: the list's counts changed.
    Toggled,
    /// Its Remove button was pressed.
    Remove(u32),
}

struct Row {
    id: u32,
    caption: String,
    done: bool,
}

impl Render for Row {
    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let id = self.id;
        h_flex()
            .items_center()
            .gap(8.)
            .px(12.)
            .py(6.)
            .hover(|style| style.bg("muted"))
            .child(
                checkbox(format!("done-{id}"))
                    .checked(self.done)
                    .on_change(cx.listener(|this, event, cx| {
                        this.done = event.as_bool().unwrap_or(false);
                        cx.emit(RowEvent::Toggled);
                        cx.notify();
                    })),
            )
            .child(div().flex_1().child(text(self.caption.clone())))
            .child(
                ghost_button(format!("remove-{id}"), "Remove")
                    .on_click(cx.listener(move |_, _, cx| cx.emit(RowEvent::Remove(id)))),
            )
    }
}

struct TodoList {
    draft: InputState,
    rows: Vec<Child<Row>>,
    filter: Filter,
    next_id: u32,
}

impl TodoList {
    fn add(&mut self, cx: &mut Cx<Self>) {
        let caption = self.draft.value().trim().to_owned();
        if caption.is_empty() {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let row = cx.new(|_| Row {
            id,
            caption,
            done: false,
        });
        cx.subscribe(&row, |this: &mut Self, event: &RowEvent, cx| {
            if let RowEvent::Remove(id) = event {
                this.rows.retain(|row| row.read(|row| row.id) != *id);
            }
            cx.notify();
        });
        self.rows.push(row);
        self.draft.set_value("");
    }

    fn percent_done(&self) -> f64 {
        if self.rows.is_empty() {
            return 0.;
        }
        let done = self.rows.len() - self.remaining();
        done as f64 * 100. / self.rows.len() as f64
    }

    fn remaining(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| !row.read(|row| row.done))
            .count()
    }

    fn filter_button(&self, filter: Filter, caption: &'static str, cx: &mut Cx<Self>) -> Element {
        let button = if self.filter == filter {
            primary_button(format!("filter-{caption}"), caption)
        } else {
            ghost_button(format!("filter-{caption}"), caption)
        };
        button.on_click(cx.listener(move |this, _, cx| {
            this.filter = filter;
            cx.notify();
        }))
    }
}

/// `gpui-base` buttons carry no look of their own; these two are the
/// example's, the way `ui.js` defines the script example's.
fn primary_button(id: String, caption: &'static str) -> Element {
    button(id)
        .px(12.)
        .py(6.)
        .rounded(6.)
        .bg("primary")
        .text_color("primary_foreground")
        .cursor_pointer()
        .child(text(caption))
}

fn ghost_button(id: String, caption: &'static str) -> Element {
    button(id)
        .px(12.)
        .py(6.)
        .rounded(6.)
        .text_color("muted_foreground")
        .cursor_pointer()
        .child(text(caption))
}

impl View for TodoList {
    fn init(cx: &mut Cx<Self>) -> Self {
        let draft = InputState::new("What needs doing?");
        // Enter is how a list like this is actually used.
        draft.on_submit(cx, |this, _, cx| {
            this.add(cx);
            cx.notify();
        });
        TodoList {
            draft,
            rows: Vec::new(),
            filter: Filter::All,
            next_id: 1,
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let filter = self.filter;
        let rows = self
            .rows
            .iter()
            .filter(|row| {
                let done = row.read(|row| row.done);
                match filter {
                    Filter::All => true,
                    Filter::Active => !done,
                    Filter::Done => done,
                }
            })
            .map(Child::element);
        let summary = if self.rows.is_empty() {
            "Nothing yet".to_owned()
        } else {
            format!("{} of {} remaining", self.remaining(), self.rows.len())
        };

        v_flex()
            .size_full()
            .gap(12.)
            .p(16.)
            .bg("background")
            .text_color("foreground")
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    // Text has no box of its own; style the element around it.
                    .child(div().text_size(18.).child(text("Todo")))
                    // A registered `gpui-component` Tag, as a script's
                    // `Tag.new().variant("secondary")`.
                    .child(
                        component("Tag", [])
                            .call("variant", [CArg::Enum("secondary".into())])
                            .child(text(summary)),
                    ),
            )
            .child(
                h_flex()
                    .gap(8.)
                    .child(div().flex_1().child(input(&self.draft)))
                    // `gpui-component`'s Button rather than the unstyled base
                    // one: its look comes from the catalog.
                    .child(
                        component("Button", ["add".into()])
                            .call("label", ["Add".into()])
                            .call("primary", [])
                            .on_click(cx.listener(|this, _, cx| {
                                this.add(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                component("Progress", ["done-progress".into()])
                    .call("value", [self.percent_done().into()]),
            )
            .child(
                h_flex()
                    .gap(4.)
                    .child(self.filter_button(Filter::All, "All", cx))
                    .child(self.filter_button(Filter::Active, "Active", cx))
                    .child(self.filter_button(Filter::Done, "Done", cx)),
            )
            .child(
                v_flex()
                    .border(1.)
                    .border_color("border")
                    .rounded(8.)
                    .children(rows),
            )
    }
}

export_view_retained!(TodoList);
