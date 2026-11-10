//! A todo list, as a WebAssembly guest view written reactively
//! (`docs/wasm-retained-guest.md`): the view is built once, state lives in
//! signals, and each change reaches the host as operations on its tree.
//!
//! The same core as `examples/js_todolist` (a draft input that adds on Enter
//! or on the Add button, rows with a checkbox and a remove button, a filter,
//! and a remaining count) written against `gpui-shell-wasm-guest`.
//! `app-js/main.js` is this view written as a script, element for element.
//!
//! ```sh
//! examples/wasm_todolist/run.sh      # this guest, on the wasm engine
//! examples/wasm_todolist/run.sh js   # the script, on QuickJS
//! scripts/compare-engines.sh         # both, timed side by side
//! ```
//!
//! `app/gpui-shell.json` names the built module as its entry
//! (`build.sh` puts it there).

use gpui_shell_wasm_guest::{export_reactive, reactive::*};

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Active,
    Done,
}

impl Filter {
    fn caption(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Active => "Active",
            Filter::Done => "Done",
        }
    }

    fn shows(self, todo: &Todo) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => !todo.done,
            Filter::Done => todo.done,
        }
    }
}

#[derive(Clone, PartialEq)]
struct Todo {
    id: u32,
    caption: String,
    done: bool,
}

fn app() -> Node {
    let draft = InputState::new("What needs doing?");
    let items = create_signal(Vec::<Todo>::new());
    let filter = create_signal(Filter::All);
    let next_id = create_signal(1u32);

    let add = move || {
        let caption = draft.value().trim().to_owned();
        if caption.is_empty() {
            return;
        }
        let id = next_id.get();
        next_id.set(id + 1);
        items.update(|items| {
            items.push(Todo {
                id,
                caption,
                done: false,
            })
        });
        draft.set_value("");
    };
    // Enter is how a list like this is actually used.
    draft.on_submit(move |_| add());

    let remaining =
        create_memo(move || items.with(|items| items.iter().filter(|todo| !todo.done).count()));
    let summary = move || {
        let total = items.with(Vec::len);
        if total == 0 {
            "Nothing yet".to_owned()
        } else {
            format!("{} of {total} remaining", remaining.get())
        }
    };
    let percent_done = move || {
        let total = items.with(Vec::len);
        if total == 0 {
            0.
        } else {
            (total - remaining.get()) as f64 * 100. / total as f64
        }
    };
    let visible = create_memo(move || {
        let filter = filter.get();
        items.with(|items| {
            items
                .iter()
                .filter(|todo| filter.shows(todo))
                .cloned()
                .collect::<Vec<_>>()
        })
    });

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
                .child(div().flex_1().child(input(&draft)))
                // `gpui-component`'s Button rather than the unstyled base
                // one: its look comes from the catalog.
                .child(
                    component("Button", ["add".into()])
                        .call("label", ["Add".into()])
                        .call("primary", [])
                        .on_click(move |_| add()),
                ),
        )
        .child(
            component("Progress", ["done-progress".into()])
                .call_with("value", move || vec![percent_done().into()]),
        )
        .child(
            h_flex()
                .gap(4.)
                .child(filter_button(Filter::All, filter))
                .child(filter_button(Filter::Active, filter))
                .child(filter_button(Filter::Done, filter)),
        )
        .child(v_flex().border(1.).border_color("border").rounded(8.).each(
            visible,
            |todo| todo.id,
            move |todo| row(todo, items),
        ))
}

/// One todo. Its key is its id, so a row is built once per todo: what can
/// change while the todo stays (whether it is done) is read from `items`.
fn row(todo: Todo, items: Signal<Vec<Todo>>) -> Node {
    let id = todo.id;
    let done = move || {
        items.with(|items| {
            items
                .iter()
                .find(|todo| todo.id == id)
                .is_some_and(|todo| todo.done)
        })
    };
    h_flex()
        .items_center()
        .gap(8.)
        .px(12.)
        .py(6.)
        .hover(|style| style.bg("muted"))
        .child(
            checkbox(format!("done-{id}"))
                .checked(done)
                .on_change(move |event| {
                    let done = event.as_bool().unwrap_or(false);
                    items.update(|items| {
                        if let Some(todo) = items.iter_mut().find(|todo| todo.id == id) {
                            todo.done = done;
                        }
                    });
                }),
        )
        .child(div().flex_1().child(text(todo.caption)))
        .child(
            ghost_button(format!("remove-{id}"), "Remove").on_click(move |_| {
                items.update(|items| items.retain(|todo| todo.id != id));
            }),
        )
}

/// A filter's button: the selected one primary, the others ghost.
fn filter_button(this: Filter, filter: Signal<Filter>) -> Node {
    let selected = move || filter.get() == this;
    let caption = this.caption();
    button(format!("filter-{caption}"))
        .px(12.)
        .py(6.)
        .rounded(6.)
        .style_str_opt("bg", move || selected().then_some("primary"))
        .text_color(move || {
            if selected() {
                "primary_foreground"
            } else {
                "muted_foreground"
            }
        })
        .cursor_pointer()
        .child(text(caption))
        .on_click(move |_| filter.set(this))
}

/// `gpui-base` buttons carry no look of their own; this is the example's,
/// the way `ui.js` defines the script example's.
fn ghost_button(id: String, caption: &'static str) -> Node {
    button(id)
        .px(12.)
        .py(6.)
        .rounded(6.)
        .text_color("muted_foreground")
        .cursor_pointer()
        .child(text(caption))
}

export_reactive!(app);
