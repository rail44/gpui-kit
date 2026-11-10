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
//! * The `gpui-base` element set and stateless registered components
//!   (constructors and methods, element and handler arguments) are
//!   reachable; registered retained state, state styles, slots, async and the
//!   capability-gated host APIs (fs, storage, network) have no import yet.
//! * Registered component arguments are not validated against their schema
//!   before the adapter decodes them, as a script's are.
//! * Descriptions cross as postcard; handler payloads as JSON. The grid test
//!   below times the guest beside the script.

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
    // Nodes arrive after everything they refer to, so each one is finished —
    // constructed, its calls recorded, its children attached — before the
    // next, exactly the order a script's builder chain records in.
    let mut ids: Vec<SpecId> = Vec::with_capacity(description.nodes.len());

    for node in &description.nodes {
        let id = match &node.component {
            abi::Component::Registered { export, args } => {
                let (component_id, name, constructor) = runtime
                    .components
                    .registered()
                    .find_map(|(component_id, descriptor)| {
                        descriptor
                            .constructors()
                            .iter()
                            .find(|constructor| constructor.export() == export)
                            .map(|constructor| {
                                (component_id, descriptor.name(), constructor.clone())
                            })
                    })
                    .ok_or_else(|| fail(format!("no registered component exports `{export}`")))?;
                let arguments = component_arguments(ctx, runtime, guest, &ids, args)?;
                let payload = constructor
                    .payload(&arguments)
                    .map_err(|error| fail(format!("{export}(...): {error}")))?;
                runtime.push_component(Component::Registered(
                    crate::spec::RegisteredComponentSpec::new(component_id, name, payload),
                ))
            }
            other => runtime.push_component(component(other)),
        };
        ids.push(id);

        for op in &node.ops {
            let op = match op {
                Op::Style(name) => SpecOp::NullaryStyle(
                    crate::style::nullary_index(name)
                        .ok_or_else(|| fail(format!("unknown style `{name}`")))?,
                ),
                Op::ParamStyle(name, args) => {
                    let name = crate::style::param_style_name(name)
                        .ok_or_else(|| fail(format!("unknown style `{name}`")))?;
                    let mut args = bridged(args);
                    // A script reads a color as `cx.theme().colors.primary`; a
                    // guest names the token instead, resolved here against the
                    // theme the build synced.
                    if COLOR_STYLES.contains(&name) {
                        for arg in &mut args {
                            if let Bridged::Str(value) = arg
                                && !value.starts_with('#')
                            {
                                let color =
                                    crate::theme_tokens::token_color(value).ok_or_else(|| {
                                        fail(format!("unknown color token `{value}`"))
                                    })?;
                                *value = hex(color);
                            }
                        }
                    }
                    SpecOp::ParamStyle(name, args)
                }
                Op::Method(name, args) => SpecOp::Method(intern(name), bridged(args)),
                Op::Callback(name, callback) => {
                    let name = match name.as_str() {
                        "on_click" => "on_click",
                        "on_change" => "on_change",
                        other => return Err(fail(format!("unsupported handler `{other}`"))),
                    };
                    SpecOp::Callback(name, register(ctx, runtime, guest, *callback)?)
                }
                Op::Component(name, args) => {
                    let method = runtime
                        .registered_method_descriptor(id, name)
                        .ok_or_else(|| fail(format!("the component has no method `{name}`")))?;
                    let arguments = component_arguments(ctx, runtime, guest, &ids, args)?;
                    let payload = method
                        .record(&arguments)
                        .map_err(|error| fail(format!("{name}(...): {error}")))?;
                    SpecOp::RegisteredMethod(crate::RecordedComponentMethod::new(
                        method.name(),
                        payload,
                    ))
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

/// Converts a guest's component arguments: an element index becomes the
/// recorded node, claimed so it is rendered by the component rather than as
/// a child; a handler becomes an entry in the open callback generation.
fn component_arguments(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    args: &[abi::ComponentArg],
) -> JsResult<Vec<crate::ComponentArgument>> {
    use crate::ComponentArgument as Argument;
    let fail = |message: String| js_error::type_error(ctx, &message);
    args.iter()
        .map(|arg| {
            Ok(match arg {
                abi::ComponentArg::String(value) => Argument::String(value.clone()),
                abi::ComponentArg::Number(value) => Argument::Number(*value),
                abi::ComponentArg::Boolean(value) => Argument::Boolean(*value),
                abi::ComponentArg::Enum(value) => Argument::Enum(value.clone()),
                abi::ComponentArg::Element(index) => {
                    let element = *ids.get(*index as usize).ok_or_else(|| {
                        fail(format!("element argument {index} comes after its user"))
                    })?;
                    runtime
                        .arena_mut()
                        .claim(element)
                        .map_err(|error| fail(error.to_string()))?;
                    Argument::Element(element)
                }
                abi::ComponentArg::Callback(callback) => {
                    Argument::Callback(register(ctx, runtime, guest, *callback)?)
                }
                abi::ComponentArg::Array(values) => {
                    Argument::Array(component_arguments(ctx, runtime, guest, ids, values)?)
                }
                abi::ComponentArg::Optional(value) => Argument::Optional(match value {
                    Some(value) => Some(Box::new(
                        component_arguments(ctx, runtime, guest, ids, std::slice::from_ref(value))?
                            .remove(0),
                    )),
                    None => None,
                }),
            })
        })
        .collect()
}

/// The style methods whose string argument is a color.
const COLOR_STYLES: &[&str] = &["bg", "text_color", "text_bg", "border_color"];

fn hex(color: gpui::Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b),
        channel(rgba.a)
    )
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
        abi::Component::Registered { .. } => unreachable!("recorded by `record`"),
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
        postcard::from_bytes(&bytes).context("guest returned a malformed description")
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

// The guests use `gpui-component`'s catalog, so the tests need it.
#[cfg(all(test, feature = "components"))]
mod tests {
    //! Drives the `examples/wasm_todolist` guest through the plugin load path
    //! and the host's own dispatch entry points.

    use std::{ops::Deref as _, path::PathBuf, process::Command};

    use gpui::{ClickEvent, IntoElement as _, TestAppContext, VisualTestContext};
    use gpui_base::input::InputEvent;

    use super::*;
    use crate::{RenderSnapshot, plugin::MANIFEST_FILE, spec::SpecNode};

    /// Builds the example guest and returns the module's path. The test builds
    /// it rather than reading a checked-in binary, so the guest and the host it
    /// is tested against cannot drift apart.
    fn build_guest(example: &str) -> PathBuf {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(example);
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .arg("--manifest-path")
            .arg(directory.join("Cargo.toml"))
            .status()
            .expect("run cargo for the guest");
        assert!(
            status.success(),
            "the example guest {example} failed to build"
        );
        directory
            .join("target/wasm32-unknown-unknown/release")
            .join(format!("{example}.wasm"))
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
        std::fs::copy(build_guest("wasm_todolist"), directory.join("todo.wasm")).unwrap();
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
            let directory = application();
            // The runtime the shipped command builds: with the catalog.
            let components = crate::components::components().expect("catalog");
            cx.update(|cx| crate::init_with_components(cx, &components));
            let runtime = cx
                .update(|cx| ShellRuntime::new_with_components(cx, components))
                .expect("runtime");
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
            self.dispatch_click(callback);
        }

        /// Clicks the only registered component named `name` (its element id
        /// lives inside the adapter's payload, not in the description).
        fn click_registered(&mut self, name: &str) {
            let name = name.to_owned();
            let callback = self.find(move |node| {
                match node.component()? {
                    Component::Registered(spec) if spec.name() == name => {}
                    _ => return None,
                }
                node.ops().iter().find_map(|op| match op {
                    SpecOp::Callback("on_click", callback) => Some(*callback),
                    _ => None,
                })
            });
            self.dispatch_click(callback);
        }

        fn dispatch_click(&mut self, callback: crate::spec::CallbackId) {
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
        // Registered `gpui-component` components, with their methods recorded
        // by the catalog's own descriptors.
        assert!(
            tree.contains("Button :label(registered) :primary(registered) :on_click(fn)"),
            "{tree}"
        );
        assert!(tree.contains("Tag :variant(registered)"), "{tree}");
        assert!(tree.contains("Progress :value(registered)"), "{tree}");
        // Color tokens arrive as theme colors, which is all materialize takes.
        assert!(tree.contains(".bg[Str(\"#"), "{tree}");
        assert!(!tree.contains("Str(\"primary\")"), "{tree}");

        // Add: the guest reads the input through a host import, appends a
        // row, clears the input through another, and asks for a re-render.
        app.type_text("Write the spike");
        app.click_registered("Button");
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

    /// `tests/benchmark.rs`'s grid, verbatim at 40 x 5.
    const SCRIPT_GRID: &str = r#"
import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button } from "gpui-base";

export default class Grid extends View {
  init() { this.rows = 40; this.columns = 5; }
  cell(row, column) {
    return div().flex().items_center().justify_center().w(90).h(24).px(6).rounded(4)
      .bg(`#f8f8f8`).text_color(`#111111`).text_sm().child(`${row}:${column}`);
  }
  row(row) {
    const cells = [];
    for (let column = 0; column < this.columns; column += 1) cells.push(this.cell(row, column));
    return h_flex().gap(6).py(2).children(cells);
  }
  render(cx) {
    const rows = [];
    for (let row = 0; row < this.rows; row += 1) rows.push(this.row(row));
    return v_flex().size_full().p(12).gap(4).bg(`#ffffff`).children(rows)
      .child(Button.new("refresh").px(10).py(4).rounded(6).bg(`#2563eb`).child("Refresh"));
  }
}
"#;

    const ROUNDS: usize = 7;
    const ITERATIONS: usize = 50;

    /// Best of `ROUNDS` batches, as `tests/benchmark.rs` times its [A].
    fn best_per_call(mut body: impl FnMut()) -> std::time::Duration {
        let mut best = std::time::Duration::MAX;
        for _ in 0..ROUNDS {
            let started = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                body();
            }
            best = best.min(started.elapsed() / ITERATIONS as u32);
        }
        best
    }

    /// The wasm guest's description cost next to the script's, under the same
    /// harness and in the same process. Meaningful only in a release build:
    ///
    /// ```text
    /// cargo test -p gpui-shell --release --no-default-features \
    ///     --features quickjs,components,wasm-guest --lib wasm_guest -- --nocapture
    /// ```
    #[gpui::test]
    fn the_grid_guest_describes_the_script_grid_and_is_timed_beside_it(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let directory = std::env::temp_dir().join(format!(
            "gpui-shell-wasm-grid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let module = build_guest("wasm_grid");
        std::fs::copy(&module, directory.join("grid.wasm")).unwrap();
        let module_bytes = std::fs::metadata(&module).unwrap().len();

        let runtime = ShellRuntime::new_isolated().expect("runtime");
        cx.update(|cx| runtime.set_global(cx));
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);

        let guest_type = runtime
            .load_app(&directory, "grid.wasm")
            .expect("load guest");
        let script_type = runtime
            .load_source("script-grid", SCRIPT_GRID)
            .expect("load script");
        let guest = context
            .update(|window, cx| runtime.instantiate(&guest_type, window, cx))
            .expect("instantiate guest");
        let script = context
            .update(|window, cx| runtime.instantiate(&script_type, window, cx))
            .expect("instantiate script");

        let build = |context: &mut VisualTestContext, object: &crate::engine::ViewObject| {
            context.update(|window, cx| {
                runtime
                    .build_snapshot(object, None, crate::policy::default(), window, cx)
                    .expect("render")
            })
        };

        // The same work, or the comparison means nothing.
        let guest_snapshot = build(&mut context, &guest);
        let script_snapshot = build(&mut context, &script);
        assert_eq!(guest_snapshot.debug_tree(), script_snapshot.debug_tree());
        let nodes = guest_snapshot.len();

        let time = |context: &mut VisualTestContext, object: &crate::engine::ViewObject| {
            context.update(|window, cx| {
                best_per_call(|| {
                    runtime
                        .build_snapshot(object, None, crate::policy::default(), window, cx)
                        .expect("render");
                })
            })
        };
        let script_build = time(&mut context, &script);
        let guest_build = time(&mut context, &guest);

        // The guest's own share: running the module and decoding its JSON.
        let instance = REGISTRY.with(|registry| registry.borrow().guests.last().cloned().unwrap());
        let mut bytes = 0;
        let guest_only = best_per_call(|| {
            let description = instance.render().expect("render");
            bytes = postcard::to_allocvec(&description).unwrap().len();
        });

        let materialize = |context: &mut VisualTestContext, snapshot: &RenderSnapshot| {
            let mut elapsed = std::time::Duration::MAX;
            context.draw(
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
        };
        let guest_materialize = materialize(&mut context, &guest_snapshot);
        let script_materialize = materialize(&mut context, &script_snapshot);

        let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0;
        println!(
            "\n[wasm guest vs script] {nodes} nodes (40x5), module {module_bytes} bytes, \
             description {bytes} bytes encoded\n\
             [A] describe:    script {:.3} ms, guest {:.3} ms \
             (guest module + decode {:.3} ms, record {:.3} ms)\n\
             [B] materialize: script {:.3} ms, guest {:.3} ms",
            ms(script_build),
            ms(guest_build),
            ms(guest_only),
            ms(guest_build.saturating_sub(guest_only)),
            ms(script_materialize),
            ms(guest_materialize),
        );
    }
}
