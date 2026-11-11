//! The same applications, timed the same way, on whichever engine this build
//! has: the grid and the todo list, each as a script (`app-js/main.js`) and as
//! a WebAssembly guest (`src/lib.rs`) under `examples/wasm_*`.
//!
//! The two engines are exclusive features, so one build times one of them.
//! `scripts/compare-engines.sh` runs both builds and puts their lines side by
//! side; each run also writes the description it timed to
//! `target/engine-comparison/<case>.<engine>.tree`, and the script refuses to
//! compare two runs whose trees differ, since then they did different work.
//!
//! Meaningful only in a release build.

use std::{ops::Deref as _, path::PathBuf, rc::Rc, time::Duration};

use gpui::{ClickEvent, Entity, IntoElement as _, TestAppContext, VisualTestContext};

use crate::{
    engine::ShellRuntime,
    spec::{CallbackId, Component, SpecId, SpecOp},
    view::ScriptView,
};

const ENGINE: &str = if cfg!(feature = "quickjs") {
    "quickjs"
} else {
    "wasm"
};

/// How many rows the todo list is timed with.
const TODO_ROWS: usize = 100;
const ROUNDS: usize = 7;
const ITERATIONS: usize = 50;

/// Best of `ROUNDS` batches of `ITERATIONS`, per call.
fn best_per_call(mut body: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..ROUNDS {
        let started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            body();
        }
        best = best.min(started.elapsed() / ITERATIONS as u32);
    }
    best
}

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// The application directory and entry this engine runs for `example`.
#[cfg(feature = "quickjs")]
fn application(example_name: &str) -> (PathBuf, &'static str) {
    (example(example_name).join("app-js"), "main.js")
}

/// The application directory and entry this engine runs for `example`: the
/// guest, built now so it cannot drift from the host it is timed on.
#[cfg(feature = "wasm-engine")]
fn application(example_name: &str) -> (PathBuf, &'static str) {
    let directory = example(example_name);
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--manifest-path")
        .arg(directory.join("Cargo.toml"))
        .status()
        .expect("run cargo for the guest");
    assert!(status.success(), "the guest {example_name} failed to build");
    let module = directory
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{example_name}.wasm"));
    let target = std::env::temp_dir().join(format!(
        "gpui-shell-comparison-{example_name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&target).unwrap();
    std::fs::copy(module, target.join("guest.wasm")).unwrap();
    (target, "guest.wasm")
}

struct Loaded {
    runtime: Rc<ShellRuntime>,
    context: VisualTestContext,
    view: Entity<ScriptView>,
}

fn load(cx: &mut TestAppContext, example_name: &str) -> Loaded {
    let components = crate::components::components().expect("catalog");
    cx.update(|cx| crate::init_with_components(cx, &components));
    let runtime = ShellRuntime::new_isolated_with_components(components).expect("runtime");
    // A run must not leave editor files in the example.
    runtime.set_editor_files(false);
    cx.update(|cx| runtime.set_global(cx));
    let window = cx.add_window(|_, _| gpui::Empty);
    let mut context = VisualTestContext::from_window(*window.deref(), cx);
    let (directory, entry) = application(example_name);
    let view_type = runtime.load_app(&directory, entry).expect("load");
    let view = context
        .update(|window, cx| runtime.instantiate_view(&view_type, window, cx))
        .expect("instantiate");
    Loaded {
        runtime,
        context,
        view,
    }
}

impl Loaded {
    fn snapshot(&mut self) -> crate::RenderSnapshot {
        let (runtime, view) = (self.runtime.clone(), self.view.clone());
        self.context.update(|window, cx| {
            let object = view.read(cx).object().clone();
            let policy = view.read(cx).policy();
            runtime
                .build_snapshot(&object, Some(view), policy, window, cx)
                .expect("render")
        })
    }

    fn describe(&mut self) -> Duration {
        let (runtime, view) = (self.runtime.clone(), self.view.clone());
        self.context.update(|window, cx| {
            let object = view.read(cx).object().clone();
            let policy = view.read(cx).policy();
            best_per_call(|| {
                runtime
                    .build_snapshot(&object, Some(view.clone()), policy.clone(), window, cx)
                    .expect("render");
            })
        })
    }

    fn materialize(&mut self, snapshot: &crate::RenderSnapshot) -> Duration {
        let runtime = self.runtime.clone();
        let mut elapsed = Duration::MAX;
        self.context.draw(
            gpui::Point::default(),
            gpui::size(gpui::px(800.), gpui::px(600.)),
            |window, cx| {
                elapsed = best_per_call(|| {
                    std::hint::black_box(crate::materialize::materialize(
                        &runtime, snapshot, window, cx,
                    ));
                });
                gpui::div().into_any_element()
            },
        );
        elapsed
    }

    fn click(&mut self, callback: CallbackId) {
        let runtime = self.runtime.clone();
        self.context.update(|window, cx| {
            runtime.dispatch_click(callback, &ClickEvent::default(), window, cx)
        });
    }
}

fn find<T>(
    snapshot: &crate::RenderSnapshot,
    pick: impl Fn(&crate::spec::SpecNode) -> Option<T>,
) -> Option<T> {
    let arena = snapshot.arena();
    (0..arena.len())
        .filter_map(|id| arena.node(id as SpecId))
        .find_map(pick)
}

fn on_click(node: &crate::spec::SpecNode) -> Option<CallbackId> {
    node.ops().iter().find_map(|op| match op {
        SpecOp::Callback("on_click", callback) => Some(*callback),
        _ => None,
    })
}

/// Writes the tree `engine` described and prints the line the script
/// collects. `engine` names the build, or a guest variant within it.
fn report(
    engine: &str,
    case: &str,
    snapshot: &crate::RenderSnapshot,
    figures: &[(&str, Duration)],
) {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/engine-comparison");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{case}.{engine}.tree")),
        snapshot.debug_tree(),
    )
    .unwrap();
    let figures: Vec<String> = figures
        .iter()
        .map(|(name, duration)| format!("{name}={:.3}", duration.as_secs_f64() * 1000.0))
        .collect();
    println!(
        "[engine-comparison] engine={engine} case={case} nodes={} {} (ms)",
        snapshot.len(),
        figures.join(" ")
    );
}

#[gpui::test]
fn the_grid_is_timed(cx: &mut TestAppContext) {
    let mut app = load(cx, "wasm_grid");
    let snapshot = app.snapshot();
    let describe = app.describe();
    let materialize = app.materialize(&snapshot);
    report(
        ENGINE,
        "grid-40x5",
        &snapshot,
        &[("describe", describe), ("materialize", materialize)],
    );
}

#[gpui::test]
fn the_todo_list_is_timed(cx: &mut TestAppContext) {
    time_todo(cx, "wasm_todolist", ENGINE);
}

/// The same todo list written as a GPUI view is (a `render` after every
/// notify), diffed in the guest over the retained protocol, next to the
/// reactive one.
#[cfg(feature = "wasm-engine")]
#[gpui::test]
fn the_todo_list_written_as_a_gpui_view_is_timed(cx: &mut TestAppContext) {
    time_todo(cx, "wasm_todolist_render", "wasm-render");
}

/// And with each row a child view, which renders again alone.
#[cfg(feature = "wasm-engine")]
#[gpui::test]
fn the_todo_list_with_child_view_rows_is_timed(cx: &mut TestAppContext) {
    time_todo(cx, "wasm_todolist_children", "wasm-children");
}

fn time_todo(cx: &mut TestAppContext, example_name: &str, engine: &str) {
    let mut app = load(cx, example_name);

    // Rows go in the way a person adds them: type, then press Add.
    for row in 0..TODO_ROWS {
        let snapshot = app.snapshot();
        let input = find(&snapshot, |node| match node.component()? {
            Component::Input(handle) => Some(*handle),
            _ => None,
        })
        .expect("the draft input");
        let add = find(&snapshot, |node| match node.component()? {
            Component::Registered(spec) if spec.name() == "Button" => on_click(node),
            _ => None,
        })
        .expect("the Add button");
        let input = app.runtime.entities().input(input).expect("a live input");
        app.context.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(format!("Item {row}"), window, cx)
            })
        });
        app.click(add);
    }

    let snapshot = app.snapshot();
    let tree = snapshot.debug_tree();
    assert!(
        tree.contains(&format!("0 of {TODO_ROWS} remaining")),
        "{tree}"
    );
    let describe = app.describe();
    // A handler that changes one row, the summary and the progress: one
    // row's checkbox, ticked and cleared in turn.
    let toggle = find(&snapshot, |node| match node.component()? {
        Component::Checkbox(id) if id == "done-1" => node.ops().iter().find_map(|op| match op {
            SpecOp::Callback("on_change", callback) => Some(*callback),
            _ => None,
        }),
        _ => None,
    })
    .expect("the first row's checkbox");
    let toggled = {
        let runtime = app.runtime.clone();
        let mut checked = false;
        app.context.update(|window, cx| {
            best_per_call(|| {
                checked = !checked;
                runtime.dispatch_change(toggle, checked, window, cx)
            })
        })
    };
    // A handler that changes state and asks for a render: the filter. Timed
    // after the toggle, because under `Active` a ticked row leaves the list,
    // and its handler with it.
    let filter = find(&snapshot, |node| match node.component()? {
        Component::Button(id) if id == "filter-Active" => on_click(node),
        _ => None,
    })
    .expect("the Active filter");
    let event = {
        let runtime = app.runtime.clone();
        app.context.update(|window, cx| {
            best_per_call(|| runtime.dispatch_click(filter, &ClickEvent::default(), window, cx))
        })
    };
    let materialize = app.materialize(&snapshot);
    report(
        engine,
        &format!("todo-{TODO_ROWS}"),
        &snapshot,
        &[
            ("describe", describe),
            ("event", event),
            ("toggle", toggled),
            ("materialize", materialize),
        ],
    );
}
