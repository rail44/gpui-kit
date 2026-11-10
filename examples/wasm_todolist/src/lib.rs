//! A todo list, as a WebAssembly guest view.
//!
//! The same core as `examples/js_todolist` — a draft input that adds on Enter
//! or on the Add button, rows with a checkbox and a remove button, a filter,
//! and a remaining count — written against `gpui-shell-wasm-guest`.
//!
//! `app/gpui-shell.json` names the built module as its entry
//! (`./examples/wasm_todolist/build.sh` puts it there). The shell's CLI still
//! resolves a fixed `main.js`, so the guest is exercised through the manifest
//! load path by the host's test:
//!
//! ```sh
//! cargo test -p gpui-shell --no-default-features \
//!     --features quickjs,components,wasm-guest --lib wasm_guest
//! ```

use gpui_shell_wasm_guest::prelude::*;

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Active,
    Done,
}

struct Todo {
    id: u32,
    caption: String,
    done: bool,
}

struct TodoList {
    draft: InputState,
    items: Vec<Todo>,
    filter: Filter,
    next_id: u32,
}

impl TodoList {
    fn add(&mut self) {
        let caption = self.draft.value().trim().to_owned();
        if caption.is_empty() {
            return;
        }
        self.items.push(Todo {
            id: self.next_id,
            caption,
            done: false,
        });
        self.next_id += 1;
        self.draft.set_value("");
    }

    fn percent_done(&self) -> f64 {
        if self.items.is_empty() {
            return 0.;
        }
        let done = self.items.len() - self.remaining();
        done as f64 * 100. / self.items.len() as f64
    }

    fn remaining(&self) -> usize {
        self.items.iter().filter(|item| !item.done).count()
    }

    fn visible(&self) -> impl Iterator<Item = &Todo> {
        self.items.iter().filter(move |item| match self.filter {
            Filter::All => true,
            Filter::Active => !item.done,
            Filter::Done => item.done,
        })
    }

    fn row(&self, item: &Todo, cx: &mut Cx<Self>) -> Element {
        let id = item.id;
        h_flex()
            .items_center()
            .gap(8.)
            .px(12.)
            .py(6.)
            .child(
                checkbox(format!("done-{id}"))
                    .checked(item.done)
                    .on_change(cx.listener(move |this, event, cx| {
                        let done = event.as_bool().unwrap_or(false);
                        if let Some(item) = this.items.iter_mut().find(|item| item.id == id) {
                            item.done = done;
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1().child(text(item.caption.clone())))
            .child(
                ghost_button(format!("remove-{id}"), "Remove").on_click(cx.listener(
                    move |this, _, cx| {
                        this.items.retain(|item| item.id != id);
                        cx.notify();
                    },
                )),
            )
    }

    fn filter_button(&self, filter: Filter, caption: &str, cx: &mut Cx<Self>) -> Element {
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
fn primary_button(id: String, caption: &str) -> Element {
    button(id)
        .px(12.)
        .py(6.)
        .rounded(6.)
        .bg("primary")
        .text_color("primary_foreground")
        .cursor_pointer()
        .child(text(caption))
}

fn ghost_button(id: String, caption: &str) -> Element {
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
            this.add();
            cx.notify();
        });
        TodoList {
            draft,
            items: Vec::new(),
            filter: Filter::All,
            next_id: 1,
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let rows: Vec<Element> = self
            .visible()
            .map(|item| (item.id, item.caption.clone(), item.done))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(id, caption, done)| self.row(&Todo { id, caption, done }, cx))
            .collect();
        let summary = if self.items.is_empty() {
            "Nothing yet".to_owned()
        } else {
            format!("{} of {} remaining", self.remaining(), self.items.len())
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
                                this.add();
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

export_view!(TodoList);
