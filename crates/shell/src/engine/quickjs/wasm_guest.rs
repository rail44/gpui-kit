//! Spike: WebAssembly guest views hosted by the QuickJS runtime.
//!
//! A guest is a core WebAssembly module (see `gpui-shell-wasm-abi`) whose
//! `render` returns a whole description at once. It does not replace the
//! engine: an application whose manifest entry ends in `.wasm` is loaded as a
//! small generated JavaScript view class whose `init` instantiates the module
//! and whose `render` records the module's description straight into this
//! runtime's arena from Rust. Everything above the engine seam — the snapshot
//! lifecycle, materialization, the plugin manifest and policy, hot reload,
//! retained entities and callback generations — is used unchanged, because to
//! all of it the guest is one more script view.
//!
//! Handlers are registered in the ordinary callback arena as JavaScript
//! functions that call the guest's `dispatch` export, so every dispatch path
//! (`dispatch_click`, `dispatch_change`, ...) reaches a guest without change.
//!
//! Known gaps, deliberately left for after the spike:
//!
//! * Guest instances live in a thread-local registry and are never released
//!   when their application generation retires (a reload leaks one).
//! * Only the `gpui-base` element set is reachable; registered
//!   `gpui-component` components, state styles, slots, async and the
//!   capability-gated host APIs (fs, storage, network) have no import yet.
//! * Descriptions and events cross as JSON; nothing has been measured.
//! * The `gpui-shell run/check` CLI resolves a fixed `main.js` entry, so a
//!   `.wasm` entry is reached through `ShellRuntime::load` (the manifest path)
//!   rather than the CLI.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::Path,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Entity, Window};
use gpui_shell_wasm_abi::{self as abi, Arg, Description, Op};
use rquickjs::{Ctx, Function, Persistent, Result as JsResult, Value, function::Func};
use smallvec::SmallVec;
use wasmtime::{Caller, Engine, Extern, Instance, Linker, Memory, Module, Store, TypedFunc};

use super::{ShellRuntime, js_error};
use crate::{
    entities::{EntityHandle, InputEventName},
    runtime::CallbackEntry,
    scope::{self, ScopePhase},
    spec::{Component, SpecId, SpecOp},
    value::Bridged,
    view::ScriptView,
};

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Compiled modules by load token, and the live guest instances.
///
/// Thread-local because `ShellRuntime` is a main-thread type and the spike
/// keeps the engine's own struct unchanged. Instances are not released when
/// their application retires; see the spike's notes.
#[derive(Default)]
struct Registry {
    engine: Option<Engine>,
    modules: Vec<Module>,
    guests: Vec<Rc<Guest>>,
}

/// Compiles `path` and returns the source of the JavaScript view class that
/// stands in for it, which the ordinary module loader then evaluates.
pub(super) fn load_module(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let engine = registry.engine.get_or_insert_with(Engine::default).clone();
        let module = Module::new(&engine, &bytes)
            .with_context(|| format!("{} is not a WebAssembly module", path.display()))?;
        registry.modules.push(module);
        Ok(shim(registry.modules.len() - 1))
    })
}

fn shim(token: usize) -> String {
    format!(
        r#"import {{ View }} from "gpui-kit";
export default class WasmGuestView extends View {{
  init(_props, _cx) {{ this.__guest = __wasm_instantiate({token}); }}
  render(_cx) {{ return {{ __id: __wasm_render(this.__guest) }}; }}
}}
"#
    )
}

/// Installs the two natives the generated view class calls.
pub(super) fn install(ctx: &Ctx<'_>, runtime: Weak<ShellRuntime>) -> JsResult<()> {
    let globals = ctx.globals();

    let instantiate_runtime = runtime.clone();
    globals.set(
        "__wasm_instantiate",
        Func::from(move |ctx: Ctx<'_>, token: u32| -> JsResult<u32> {
            let runtime = instantiate_runtime
                .upgrade()
                .ok_or_else(|| js_error::type_error(&ctx, "the shell runtime is gone"))?;
            Guest::instantiate(&runtime, token as usize)
                .map_err(|error| js_error::type_error(&ctx, &format!("{error:#}")))
        }),
    )?;

    globals.set(
        "__wasm_render",
        Func::from(move |ctx: Ctx<'_>, guest: u32| -> JsResult<u32> {
            let runtime = runtime
                .upgrade()
                .ok_or_else(|| js_error::type_error(&ctx, "the shell runtime is gone"))?;
            let guest = REGISTRY
                .with(|registry| registry.borrow().guests.get(guest as usize).cloned())
                .ok_or_else(|| js_error::type_error(&ctx, "unknown wasm guest"))?;
            let description = guest
                .render()
                .map_err(|error| js_error::type_error(&ctx, &format!("{error:#}")))?;
            // The view class wraps the id as `{ __id }`, the element shape
            // `render` is expected to return.
            Ok(record(&ctx, &runtime, &guest, &description)? as u32)
        }),
    )?;
    Ok(())
}

/// Records one guest description into the runtime's scratch arena and returns
/// the root's id.
fn record(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    description: &Description,
) -> JsResult<SpecId> {
    let fail = |message: String| js_error::type_error(ctx, &message);
    let ids: Vec<SpecId> = description
        .nodes
        .iter()
        .map(|node| runtime.push_component(component(&node.component)))
        .collect();

    for (node, &id) in description.nodes.iter().zip(&ids) {
        for op in &node.ops {
            let op = match op {
                Op::Style(name) => SpecOp::NullaryStyle(
                    crate::style::nullary_index(name)
                        .ok_or_else(|| fail(format!("unknown style `{name}`")))?,
                ),
                Op::ParamStyle(name, args) => SpecOp::ParamStyle(
                    crate::style::param_style_name(name)
                        .ok_or_else(|| fail(format!("unknown style `{name}`")))?,
                    bridged(args),
                ),
                Op::Method(name, args) => SpecOp::Method(intern(name), bridged(args)),
                Op::Callback(name, callback) => {
                    let name = match name.as_str() {
                        "on_click" => "on_click",
                        "on_change" => "on_change",
                        other => return Err(fail(format!("unsupported handler `{other}`"))),
                    };
                    SpecOp::Callback(name, register(ctx, runtime, guest, *callback)?)
                }
            };
            runtime
                .arena_mut()
                .push_op(id, op)
                .map_err(|error| fail(error.to_string()))?;
        }
        for &child in &node.children {
            let child = *ids
                .get(child as usize)
                .ok_or_else(|| fail(format!("child index {child} is out of range")))?;
            runtime
                .arena_mut()
                .attach(id, child)
                .map_err(|error| fail(error.to_string()))?;
        }
    }

    ids.get(description.root as usize)
        .copied()
        .ok_or_else(|| fail("the description has no root".to_owned()))
}

fn component(component: &abi::Component) -> Component {
    match component {
        abi::Component::Div => Component::Div,
        abi::Component::HFlex => Component::HFlex,
        abi::Component::VFlex => Component::VFlex,
        abi::Component::Text(text) => Component::Text(text.clone()),
        abi::Component::Button(id) => Component::Button(id.clone()),
        abi::Component::Checkbox(id) => Component::Checkbox(id.clone()),
        abi::Component::Input(handle) => Component::Input(EntityHandle::from(*handle)),
    }
}

fn bridged(args: &[Arg]) -> SmallVec<[Bridged; 2]> {
    args.iter()
        .map(|arg| match arg {
            Arg::Nil => Bridged::Nil,
            Arg::Bool(value) => Bridged::Bool(*value),
            Arg::Number(value) => Bridged::Number(*value),
            Arg::Str(value) => Bridged::Str(value.clone()),
        })
        .collect()
}

/// `SpecOp::Method` names are `&'static str`. A guest's method names come
/// from a closed set in practice, so interning them leaks at most one string
/// per distinct name.
fn intern(name: &str) -> &'static str {
    thread_local! {
        static NAMES: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    NAMES.with(|names| {
        *names
            .borrow_mut()
            .entry(name.to_owned())
            .or_insert_with(|| Box::leak(name.to_owned().into_boxed_str()))
    })
}

/// Puts a JavaScript function that calls the guest's `dispatch` into the
/// open callback generation, exactly where a script's handler would go.
fn register(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    callback: u32,
) -> JsResult<crate::spec::CallbackId> {
    let guest = Rc::downgrade(guest);
    let function = Function::new(
        ctx.clone(),
        move |event: rquickjs::function::Opt<Value<'_>>| -> JsResult<()> {
            let payload = match event.0 {
                Some(value) => {
                    let ctx = value.ctx().clone();
                    ctx.json_stringify(value)?
                        .map(|text| text.to_string())
                        .transpose()?
                        .unwrap_or_else(|| "null".to_owned())
                }
                None => "null".to_owned(),
            };
            let Some(guest) = guest.upgrade() else {
                return Ok(());
            };
            // A guest trap is logged rather than rethrown: the caller is the
            // host's own dispatch, which has no script to report it to.
            match guest.dispatch(callback, payload.as_bytes()) {
                Ok(true) => refresh_current_view(),
                Ok(false) => {}
                Err(error) => tracing::error!("wasm guest handler failed: {error:#}"),
            }
            Ok(())
        },
    )?;
    let saved = Persistent::save(ctx, function);
    Ok(runtime.callbacks.borrow_mut().push(CallbackEntry {
        value: saved,
        view: scope::current_view().map(|view| view.downgrade()),
        application: scope::current_application_generation(),
        registered_in: scope::current_generation(),
    }))
}

/// What a script's `cx.notify()` does for its own view: invalidate the
/// description and let GPUI schedule the frame.
fn refresh_current_view() {
    if let Some(view) = scope::current_view() {
        scope::with_current_app(|app| view.update(app, |view, cx| view.refresh(cx)));
    }
}

/// One instantiated guest module.
struct Guest {
    store: RefCell<Store<HostState>>,
    memory: Memory,
    render: TypedFunc<(), u64>,
    dispatch: TypedFunc<(u32, u32, u32), u32>,
    alloc: TypedFunc<u32, u32>,
}

struct HostState {
    runtime: Weak<ShellRuntime>,
    guest: Weak<Guest>,
    /// The view the guest was instantiated for, so a handler that runs from
    /// a native subscription (no script scope of its own) can open one.
    view: Option<Entity<ScriptView>>,
}

impl Guest {
    /// Instantiates module `token`, runs its `init`, and returns the guest id
    /// the generated view class keeps.
    fn instantiate(runtime: &Rc<ShellRuntime>, token: usize) -> Result<u32> {
        let (engine, module) = REGISTRY.with(|registry| {
            let registry = registry.borrow();
            Ok::<_, anyhow::Error>((
                registry
                    .engine
                    .clone()
                    .ok_or_else(|| anyhow!("no wasm engine"))?,
                registry
                    .modules
                    .get(token)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown wasm module {token}"))?,
            ))
        })?;

        let mut linker = Linker::new(&engine);
        link_imports(&mut linker)?;

        let mut store = Store::new(
            &engine,
            HostState {
                runtime: Rc::downgrade(runtime),
                guest: Weak::new(),
                view: scope::current_view(),
            },
        );
        let instance: Instance = linker.instantiate(&mut store, &module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("the guest exports no memory"))?;
        let render = instance.get_typed_func(&mut store, abi::export::RENDER)?;
        let dispatch = instance.get_typed_func(&mut store, abi::export::DISPATCH)?;
        let alloc = instance.get_typed_func(&mut store, abi::export::ALLOC)?;
        let init: TypedFunc<(), ()> = instance.get_typed_func(&mut store, abi::export::INIT)?;

        let guest = Rc::new(Guest {
            store: RefCell::new(store),
            memory,
            render,
            dispatch,
            alloc,
        });
        guest.store.borrow_mut().data_mut().guest = Rc::downgrade(&guest);

        // `init` runs inside the host call the view class's `init` opened, so
        // the guest may create retained state through its imports.
        init.call(&mut *guest.store.borrow_mut(), ())
            .map_err(|error| anyhow!("guest init failed: {error:#}"))?;

        let id = REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            registry.guests.push(guest);
            registry.guests.len() - 1
        });
        Ok(id as u32)
    }

    fn render(&self) -> Result<Description> {
        let mut store = self.store.borrow_mut();
        let packed = self
            .render
            .call(&mut *store, ())
            .map_err(|error| anyhow!("guest render failed: {error:#}"))?;
        let bytes = read(&self.memory, &*store, packed)?;
        serde_json::from_slice(&bytes).context("guest returned a malformed description")
    }

    /// Runs guest handler `callback`. Returns whether it asked to re-render.
    fn dispatch(&self, callback: u32, payload: &[u8]) -> Result<bool> {
        let mut store = self.store.borrow_mut();
        let len = payload.len() as u32;
        let ptr = self.alloc.call(&mut *store, len)?;
        self.memory.write(&mut *store, ptr as usize, payload)?;
        let notify = self
            .dispatch
            .call(&mut *store, (callback, ptr, len))
            .map_err(|error| anyhow!("guest handler failed: {error:#}"))?;
        Ok(notify != 0)
    }

    /// A handler that fires from a native subscription rather than from a
    /// script dispatch: open the event scope a script handler would have had.
    fn dispatch_native(self: &Rc<Self>, callback: u32, window: &mut Window, app: &mut App) {
        let view = self.store.borrow().data().view.clone();
        let Some(runtime) = self.store.borrow().data().runtime.upgrade() else {
            return;
        };
        let (_guard, _generation) =
            scope::enter_runtime(&runtime, window, app, ScopePhase::Event, view);
        match self.dispatch(callback, b"null") {
            Ok(true) => refresh_current_view(),
            Ok(false) => {}
            Err(error) => tracing::error!("wasm guest handler failed: {error:#}"),
        }
    }
}

fn read(memory: &Memory, store: impl wasmtime::AsContext, packed: u64) -> Result<Vec<u8>> {
    let (ptr, len) = abi::unpack(packed);
    let mut bytes = vec![0; len as usize];
    memory.read(&store, ptr as usize, &mut bytes)?;
    Ok(bytes)
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Result<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(anyhow!("the guest exports no memory")),
    }
}

fn read_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> Result<String> {
    let memory = guest_memory(caller)?;
    let mut bytes = vec![0; len as usize];
    memory.read(&*caller, ptr as usize, &mut bytes)?;
    Ok(String::from_utf8(bytes)?)
}

/// Copies `text` into a buffer the guest allocated and returns it packed.
fn write_string(caller: &mut Caller<'_, HostState>, text: &str) -> Result<u64> {
    let alloc = caller
        .get_export(abi::export::ALLOC)
        .and_then(Extern::into_func)
        .ok_or_else(|| anyhow!("the guest exports no allocator"))?
        .typed::<u32, u32>(&*caller)?;
    let len = text.len() as u32;
    let ptr = alloc.call(&mut *caller, len)?;
    guest_memory(caller)?.write(&mut *caller, ptr as usize, text.as_bytes())?;
    Ok(abi::pack(ptr, len))
}

fn runtime_of(caller: &Caller<'_, HostState>) -> Result<Rc<ShellRuntime>> {
    caller
        .data()
        .runtime
        .upgrade()
        .ok_or_else(|| anyhow!("the shell runtime is gone"))
}

fn input_of(runtime: &ShellRuntime, handle: u32) -> Result<Entity<gpui_base::input::InputState>> {
    runtime
        .entities()
        .input(EntityHandle::from(handle))
        .ok_or_else(|| anyhow!("input {handle} is not live"))
}

fn link_imports(linker: &mut Linker<HostState>) -> Result<()> {
    let module = abi::IMPORT_MODULE;
    linker.func_wrap(
        module,
        abi::import::INPUT_NEW,
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> Result<u32> {
            let placeholder = read_string(&mut caller, ptr, len)?;
            let runtime = runtime_of(&caller)?;
            let handle = scope::with_current(|window, cx| {
                runtime.entities().create_input(
                    (!placeholder.is_empty()).then_some(placeholder),
                    None,
                    scope::current_application_generation(),
                    window,
                    cx,
                )
            })
            .ok_or_else(|| anyhow!("input_new needs a live host call; call it from init"))?
            .map_err(|_| anyhow!("too many retained entities"))?;
            u32::try_from(handle).context("input handle does not fit the guest ABI")
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::INPUT_VALUE,
        |mut caller: Caller<'_, HostState>, handle: u32| -> Result<u64> {
            let runtime = runtime_of(&caller)?;
            let state = input_of(&runtime, handle)?;
            let text = scope::with_current_app(|cx| state.read(cx).value().to_string())
                .ok_or_else(|| anyhow!("input_value needs a live host call"))?;
            write_string(&mut caller, &text)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::INPUT_SET_VALUE,
        |mut caller: Caller<'_, HostState>, handle: u32, ptr: u32, len: u32| -> Result<()> {
            let text = read_string(&mut caller, ptr, len)?;
            let runtime = runtime_of(&caller)?;
            let state = input_of(&runtime, handle)?;
            scope::with_current(|window, cx| {
                state.update(cx, |state, cx| state.set_value(text, window, cx))
            })
            .ok_or_else(|| anyhow!("input_set_value needs a live host call"))
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::INPUT_ON_SUBMIT,
        |caller: Caller<'_, HostState>, handle: u32, callback: u32| -> Result<()> {
            let runtime = runtime_of(&caller)?;
            let guest = caller.data().guest.clone();
            let subscribed = scope::with_current(|window, cx| {
                runtime.entities().subscribe_input(
                    EntityHandle::from(handle),
                    InputEventName::Submit,
                    window,
                    cx,
                    move |_event, window, app| {
                        if let Some(guest) = guest.upgrade() {
                            guest.dispatch_native(callback, window, app);
                        }
                    },
                )
            })
            .ok_or_else(|| anyhow!("input_on_submit needs a live host call"))?;
            anyhow::ensure!(subscribed, "input {handle} is not live");
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::LOG,
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> Result<()> {
            tracing::info!(target: "wasm_guest", "{}", read_string(&mut caller, ptr, len)?);
            Ok(())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Drives the `examples/wasm_todolist` guest through the plugin load path
    //! and the host's own dispatch entry points.

    use std::{ops::Deref as _, path::PathBuf, process::Command};

    use gpui::{ClickEvent, TestAppContext, VisualTestContext};
    use gpui_base::input::InputEvent;

    use super::*;
    use crate::{RenderSnapshot, plugin::MANIFEST_FILE, spec::SpecNode};

    /// Builds the example guest and returns the module's path. The test builds
    /// it rather than reading a checked-in binary, so the guest and the host it
    /// is tested against cannot drift apart.
    fn build_guest() -> PathBuf {
        let example =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/wasm_todolist");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .arg("--manifest-path")
            .arg(example.join("Cargo.toml"))
            .status()
            .expect("run cargo for the guest");
        assert!(status.success(), "the example guest failed to build");
        example.join("target/wasm32-unknown-unknown/release/wasm_todolist.wasm")
    }

    /// An application directory holding the guest and a manifest naming it.
    fn application() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "gpui-shell-wasm-guest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(build_guest(), directory.join("todo.wasm")).unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            r#"{"id": "dev.test.wasm-todolist", "name": "Wasm Todo", "entry": "todo.wasm"}"#,
        )
        .unwrap();
        directory
    }

    struct Mounted(Entity<crate::ShellRoot>);

    impl gpui::Render for Mounted {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.0.clone()
        }
    }

    struct Harness {
        context: VisualTestContext,
        runtime: Rc<ShellRuntime>,
        view: Entity<ScriptView>,
    }

    impl Harness {
        fn load(cx: &mut TestAppContext) -> Self {
            cx.update(crate::init);
            let directory = application();
            let runtime = cx.update(ShellRuntime::new).expect("runtime");
            let window = cx.add_window(|_, _| gpui::Empty);
            let mut context = VisualTestContext::from_window(*window.deref(), cx);
            let root = context
                .update(|window, cx| runtime.try_load(&directory, window, cx))
                .expect("the guest loads through its manifest");
            let view = context.update(|_, cx| {
                root.read(cx)
                    .content()
                    .clone()
                    .downcast::<ScriptView>()
                    .expect("a guest mounts as a script view")
            });
            // Mount the loaded root so drawing the window builds snapshots.
            let mounted = root.clone();
            context.update(|window, cx| {
                window.replace_root(cx, |_, _| Mounted(mounted));
            });
            let mut harness = Self {
                context,
                runtime,
                view,
            };
            harness.settle();
            harness
        }

        /// Lets GPUI draw, which is the only thing that builds a snapshot.
        fn settle(&mut self) {
            self.context.update(|window, _| window.refresh());
            self.context.run_until_parked();
        }

        fn tree(&mut self) -> String {
            let view = self.view.clone();
            self.context.update(|_, cx| {
                view.read(cx)
                    .snapshot()
                    .map(RenderSnapshot::debug_tree)
                    .unwrap_or_default()
            })
        }

        /// The first node the predicate accepts in the published description.
        fn find<T>(&mut self, pick: impl Fn(&SpecNode) -> Option<T>) -> T {
            let view = self.view.clone();
            self.context.update(|_, cx| {
                let view = view.read(cx);
                let snapshot = view.snapshot().expect("a published snapshot");
                let arena = snapshot.arena();
                (0..arena.len())
                    .filter_map(|id| arena.node(id as SpecId))
                    .find_map(&pick)
                    .expect("no matching node")
            })
        }

        fn handler(&mut self, element: &str, event: &'static str) -> crate::spec::CallbackId {
            let element = element.to_owned();
            self.find(move |node| {
                let id = match node.component()? {
                    Component::Button(id) | Component::Checkbox(id) => id,
                    _ => return None,
                };
                (*id == element).then_some(())?;
                node.ops().iter().find_map(|op| match op {
                    SpecOp::Callback(name, callback) if *name == event => Some(*callback),
                    _ => None,
                })
            })
        }

        fn input(&mut self) -> Entity<gpui_base::input::InputState> {
            let handle = self.find(|node| match node.component()? {
                Component::Input(handle) => Some(*handle),
                _ => None,
            });
            self.runtime.entities().input(handle).expect("a live input")
        }

        fn type_text(&mut self, text: &str) {
            let input = self.input();
            let text = text.to_owned();
            self.context.update(|window, cx| {
                input.update(cx, |state, cx| state.set_value(text, window, cx))
            });
        }

        fn click(&mut self, element: &str) {
            let callback = self.handler(element, "on_click");
            let runtime = self.runtime.clone();
            self.context.update(|window, cx| {
                runtime.dispatch_click(callback, &ClickEvent::default(), window, cx)
            });
            self.settle();
        }
    }

    #[gpui::test]
    fn the_todo_guest_renders_adds_toggles_submits_and_removes(cx: &mut TestAppContext) {
        let mut app = Harness::load(cx);
        let tree = app.tree();
        assert!(tree.contains("Nothing yet"), "{tree}");
        assert!(tree.contains("Button \"add\""), "{tree}");

        // Add: the guest reads the input through a host import, appends a
        // row, clears the input through another, and asks for a re-render.
        app.type_text("Write the spike");
        app.click("add");
        let tree = app.tree();
        assert!(tree.contains("Write the spike"), "{tree}");
        assert!(tree.contains("1 of 1 remaining"), "{tree}");
        let input = app.input();
        let cleared = app
            .context
            .update(|_, cx| input.read(cx).value().to_string());
        assert_eq!(cleared, "");

        // Toggle: a checkbox handler receives the requested state.
        let toggle = app.handler("done-1", "on_change");
        let runtime = app.runtime.clone();
        app.context
            .update(|window, cx| runtime.dispatch_change(toggle, true, window, cx));
        app.settle();
        let tree = app.tree();
        assert!(tree.contains("0 of 1 remaining"), "{tree}");

        // Submit: Enter in the input reaches the guest from the native
        // subscription it registered during `init`.
        app.type_text("Second");
        let input = app.input();
        app.context.update(|_, cx| {
            input.update(cx, |_, cx| {
                cx.emit(InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                })
            })
        });
        app.settle();
        let tree = app.tree();
        assert!(tree.contains("Second"), "{tree}");
        assert!(tree.contains("1 of 2 remaining"), "{tree}");

        // Filter, then remove.
        app.click("filter-Done");
        let tree = app.tree();
        assert!(!tree.contains("Second"), "{tree}");
        app.click("filter-All");
        app.click("remove-1");
        let tree = app.tree();
        assert!(!tree.contains("Write the spike"), "{tree}");
        assert!(tree.contains("1 of 1 remaining"), "{tree}");
    }
}
