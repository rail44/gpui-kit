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
//! What a guest reaches: the `gpui-base` elements, state styles, named slots,
//! registered components (validated against their schemas like a script's
//! arguments) and their retained state (methods and input events); and,
//! under the application's capabilities, the persistent and session stores,
//! the clipboard, files, processes and HTTP. Calls a script makes
//! asynchronously are asynchronous here too, callback-style (a core module
//! has no promise to settle): the work runs on the background executor and
//! the handler back on the main thread. Store writes reach the disk the same
//! way. Modules and instances are released with their application generation.
//!
//! Known gaps, deliberately left for after the spike:
//!
//! * Not reachable yet: virtualized lists, nested views, sockets, streamed
//!   processes, overlays (dialogs, sheets, toasts) and the dock. Each needs
//!   a guest-side shape of its own (a renderer called during layout, a
//!   child guest, a stream of events, a window-level surface).
//! * The JavaScript engine still hosts every guest (a generated view class
//!   per module); this is a guest *kind* next to scripts, not a second engine.
//! * Descriptions cross as postcard; handler payloads as JSON. The grid test
//!   below times the guest beside the script.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::Path,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Entity, WeakEntity, Window};
use gpui_shell_wasm_abi::{self as abi, Arg, Description, Op};
use rquickjs::{Ctx, Function, Persistent, Result as JsResult, Value, function::Func};
use smallvec::SmallVec;
use wasmtime::{Caller, Engine, Extern, Instance, Linker, Memory, Module, Store, TypedFunc};

use super::{ShellRuntime, js_error};
use crate::{
    entities::{EntityHandle, InputEventName},
    runtime::{ApplicationGeneration, CallbackEntry},
    scope::{self, ScopePhase},
    spec::{Component, SpecId, SpecOp},
    value::Bridged,
    view::ScriptView,
};

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Compiled modules by load token, and the live guest instances, each tagged
/// with the application generation that loaded it.
///
/// Thread-local because `ShellRuntime` is a main-thread type and the spike
/// keeps the engine's own struct unchanged. Slots are emptied rather than
/// removed when their generation is released, so tokens and guest ids the
/// generated view classes hold stay valid indices.
#[derive(Default)]
struct Registry {
    engine: Option<Engine>,
    modules: Vec<Option<(Module, Option<Rc<ApplicationGeneration>>)>>,
    guests: Vec<Option<Rc<Guest>>>,
}

/// Compiles `path` and returns the source of the JavaScript view class that
/// stands in for it, which the ordinary module loader then evaluates.
pub(super) fn load_module(path: &Path, application: &Rc<ApplicationGeneration>) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let engine = registry.engine.get_or_insert_with(Engine::default).clone();
        let module = Module::new(&engine, &bytes)
            .with_context(|| format!("{} is not a WebAssembly module", path.display()))?;
        registry
            .modules
            .push(Some((module, Some(application.clone()))));
        Ok(shim(registry.modules.len() - 1))
    })
}

/// Drops the modules and guest instances `application` loaded. Called where
/// the runtime releases the rest of an application generation's state.
pub(super) fn release(application: &Rc<ApplicationGeneration>) {
    let owned = |owner: &Option<Rc<ApplicationGeneration>>| {
        owner
            .as_ref()
            .is_some_and(|owner| Rc::ptr_eq(owner, application))
    };
    // Taken out of the registry first and dropped after the borrow ends: a
    // guest's store owns host closures that must not run under the borrow.
    let released: Vec<_> = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        for slot in &mut registry.modules {
            if slot.as_ref().is_some_and(|(_, owner)| owned(owner)) {
                *slot = None;
            }
        }
        registry
            .guests
            .iter_mut()
            .filter(|slot| slot.as_ref().is_some_and(|guest| owned(&guest.application)))
            .filter_map(Option::take)
            .collect()
    });
    drop(released);
}

/// How many guest instances are live, for tests.
#[cfg(test)]
fn live_guests() -> usize {
    REGISTRY.with(|registry| registry.borrow().guests.iter().flatten().count())
}

fn shim(token: usize) -> String {
    format!(
        r#"import {{ View }} from "gpui-kit";
export default class WasmGuestView extends View {{
  init(_props, _cx) {{ this.__guest = __wasm_instantiate({token}); }}
  render(_cx) {{ return {{ __id: __wasm_render(this.__guest, 0) }}; }}
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
            let id = Guest::instantiate(&runtime, token as usize)
                .map_err(|error| js_error::type_error(&ctx, &format!("{error:#}")))?;
            // A dialog the guest opened from `init`.
            if let Some(guest) = guest_by_id(id) {
                open_pending_dialogs(&ctx, &runtime, &guest)?;
            }
            Ok(id)
        }),
    )?;

    // The view class a part is shown through: what a script's dialog content
    // function or child view is, rendering the guest's part instead.
    ctx.eval::<(), _>(
        "globalThis.__wasm_part_view = (guest, part) => \
         ({ render(_cx) { return { __id: __wasm_render(guest, part) }; } });",
    )?;

    globals.set(
        "__wasm_render",
        Func::from(
            move |ctx: Ctx<'_>, guest: u32, part: u32| -> JsResult<u32> {
                let runtime = runtime
                    .upgrade()
                    .ok_or_else(|| js_error::type_error(&ctx, "the shell runtime is gone"))?;
                let guest = REGISTRY
                    .with(|registry| {
                        registry
                            .borrow()
                            .guests
                            .get(guest as usize)
                            .cloned()
                            .flatten()
                    })
                    .ok_or_else(|| js_error::type_error(&ctx, "unknown wasm guest"))?;
                let description = guest
                    .render(part)
                    .map_err(|error| js_error::type_error(&ctx, &format!("{error:#}")))?;
                // The view class wraps the id as `{ __id }`, the element shape
                // `render` is expected to return.
                Ok(record(&ctx, &runtime, &guest, &description)? as u32)
            },
        ),
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
                let target = ArgumentTarget {
                    api: export,
                    descriptors: constructor.arguments(),
                };
                let arguments = component_arguments(ctx, runtime, guest, &ids, target, args)?;
                let payload = constructor
                    .payload(&arguments)
                    .map_err(|error| fail(format!("{export}(...): {error}")))?;
                runtime.push_component(Component::Registered(
                    crate::spec::RegisteredComponentSpec::new(component_id, name, payload),
                ))
            }
            abi::Component::Nested(part) => nested_view(ctx, runtime, guest, *part)?,
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
                Op::Slot(name, index) => {
                    let element = *ids.get(*index as usize).ok_or_else(|| {
                        fail(format!("slot element {index} comes after its user"))
                    })?;
                    // The script path's own: it checks the slot name, claims
                    // the element and records the op.
                    runtime.fill_slot(ctx, id, name, element)?;
                    continue;
                }
                Op::StateStyle(name, index) => {
                    let name = match name.as_str() {
                        "hover" => "hover",
                        "active" => "active",
                        "focus" => "focus",
                        other => return Err(fail(format!("unsupported state style `{other}`"))),
                    };
                    let styles = *ids.get(*index as usize).ok_or_else(|| {
                        fail(format!("state style node {index} comes after its user"))
                    })?;
                    // Claimed: the declarations feed the style, never the tree.
                    runtime
                        .arena_mut()
                        .claim(styles)
                        .map_err(|error| fail(error.to_string()))?;
                    SpecOp::StateStyle(name, styles)
                }
                Op::Component(name, args) => {
                    let method = runtime
                        .registered_method_descriptor(id, name)
                        .ok_or_else(|| fail(format!("the component has no method `{name}`")))?;
                    let target = ArgumentTarget {
                        api: name,
                        descriptors: method.arguments(),
                    };
                    let arguments = component_arguments(ctx, runtime, guest, &ids, target, args)?;
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

/// What the arguments of one registered constructor or method are checked
/// against, and where they are going.
struct ArgumentTarget<'a> {
    api: &'a str,
    descriptors: &'a [crate::ArgumentDescriptor],
}

/// Validates and normalizes a guest's component arguments against the
/// descriptor's schemas, the way a script's arguments are: an omitted or
/// bare optional becomes `Optional(..)`, an enum may arrive as a string, a
/// number must be finite. An element index becomes the recorded node,
/// claimed so the component renders it rather than the tree; a handler
/// becomes an entry in the open callback generation.
fn component_arguments(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    target: ArgumentTarget<'_>,
    args: &[abi::ComponentArg],
) -> JsResult<Vec<crate::ComponentArgument>> {
    use crate::{ArgumentSchema, ComponentArgument as Argument};
    let ArgumentTarget { api, descriptors } = target;
    if args.len() > descriptors.len() {
        return Err(js_error::type_error(
            ctx,
            &format!("{api}(...) expects at most {} arguments", descriptors.len()),
        ));
    }
    let convert = |name: &str, schema: &ArgumentSchema, arg: &abi::ComponentArg| {
        component_argument(ctx, runtime, guest, ids, api, name, schema, arg)
    };
    descriptors
        .iter()
        .enumerate()
        .map(|(index, descriptor)| {
            let arg = args.get(index);
            if let ArgumentSchema::Optional(inner) = descriptor.schema() {
                return match arg {
                    None | Some(abi::ComponentArg::Optional(None)) => Ok(Argument::Optional(None)),
                    Some(abi::ComponentArg::Optional(Some(value))) => {
                        convert(descriptor.name(), inner, value)
                            .map(|value| Argument::Optional(Some(Box::new(value))))
                    }
                    Some(value) => convert(descriptor.name(), inner, value)
                        .map(|value| Argument::Optional(Some(Box::new(value)))),
                };
            }
            let arg = arg.ok_or_else(|| {
                js_error::type_error(
                    ctx,
                    &format!(
                        "{api}({}) expects {}",
                        descriptor.name(),
                        super::schema_name(descriptor.schema())
                    ),
                )
            })?;
            convert(descriptor.name(), descriptor.schema(), arg)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn component_argument(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    api: &str,
    name: &str,
    schema: &crate::ArgumentSchema,
    arg: &abi::ComponentArg,
) -> JsResult<crate::ComponentArgument> {
    use crate::{ArgumentSchema as Schema, ComponentArgument as Argument};
    use abi::ComponentArg as Guest_;
    let fail = |message: String| js_error::type_error(ctx, &message);
    let recurse = |schema: &Schema, arg: &Guest_| {
        component_argument(ctx, runtime, guest, ids, api, name, schema, arg)
    };
    let converted = match (schema, arg) {
        (Schema::String, Guest_::String(value)) => Some(Argument::String(value.clone())),
        (Schema::Number, Guest_::Number(value)) if value.is_finite() => {
            Some(Argument::Number(*value))
        }
        (Schema::Boolean, Guest_::Boolean(value)) => Some(Argument::Boolean(*value)),
        (Schema::Enum(values), Guest_::Enum(value) | Guest_::String(value))
            if values.contains(&value.as_str()) =>
        {
            Some(Argument::Enum(value.clone()))
        }
        (Schema::Element, Guest_::Element(index)) => {
            let element = *ids
                .get(*index as usize)
                .ok_or_else(|| fail(format!("element argument {index} comes after its user")))?;
            runtime
                .arena_mut()
                .claim(element)
                .map_err(|error| fail(error.to_string()))?;
            Some(Argument::Element(element))
        }
        (Schema::Callback(_), Guest_::Callback(callback)) => Some(Argument::Callback(register(
            ctx, runtime, guest, *callback,
        )?)),
        (Schema::Array(item), Guest_::Array(values)) => Some(Argument::Array(
            values
                .iter()
                .map(|value| recurse(item, value))
                .collect::<JsResult<_>>()?,
        )),
        (Schema::Optional(_), Guest_::Optional(None)) => Some(Argument::Optional(None)),
        (Schema::Optional(inner), Guest_::Optional(Some(value))) => {
            Some(Argument::Optional(Some(Box::new(recurse(inner, value)?))))
        }
        (Schema::Optional(inner), value) => {
            Some(Argument::Optional(Some(Box::new(recurse(inner, value)?))))
        }
        // A registered retained state, checked against the kind the schema
        // names, as a script's state object is.
        (Schema::Entity(kind), Guest_::State(handle))
            if runtime
                .component_states
                .try_borrow()
                .map_err(|_| fail("retained component state is already borrowed".into()))?
                .kind(*handle)
                == Some(*kind) =>
        {
            Some(Argument::Entity {
                kind,
                handle: *handle,
            })
        }
        _ => None,
    };
    converted.ok_or_else(|| {
        fail(format!(
            "{api}({name}) expects {}",
            super::schema_name(schema)
        ))
    })
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
        abi::Component::Registered { .. } | abi::Component::Nested(_) => {
            unreachable!("recorded by `record`")
        }
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
    let handler_runtime = Rc::downgrade(runtime);
    let function = Function::new(
        ctx.clone(),
        move |event: rquickjs::function::Opt<Value<'_>>| -> JsResult<()> {
            let (payload, ctx) = match event.0 {
                Some(value) => {
                    let ctx = value.ctx().clone();
                    let payload = ctx
                        .json_stringify(value)?
                        .map(|text| text.to_string())
                        .transpose()?
                        .unwrap_or_else(|| "null".to_owned());
                    (payload, Some(ctx))
                }
                None => ("null".to_owned(), None),
            };
            let Some(guest) = guest.upgrade() else {
                return Ok(());
            };
            // A guest trap is logged rather than rethrown: the caller is the
            // host's own dispatch, which has no script to report it to.
            match guest.dispatch(callback, payload.as_bytes()) {
                Ok(true) => refresh_guest(&guest),
                Ok(false) => {}
                Err(error) => tracing::error!("wasm guest handler failed: {error:#}"),
            }
            if let (Some(ctx), Some(runtime)) = (ctx, handler_runtime.upgrade()) {
                open_pending_dialogs(&ctx, &runtime, &guest)?;
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

/// What a script's `cx.notify()` does for its own view — invalidate the
/// description and let GPUI schedule the frame — for every view showing the
/// guest: its root and each part, since they all read the same guest state.
fn refresh_guest(guest: &Guest) {
    let views: Vec<_> = guest
        .views
        .borrow()
        .iter()
        .filter_map(WeakEntity::upgrade)
        .collect();
    let views = if views.is_empty() {
        scope::current_view().into_iter().collect()
    } else {
        views
    };
    scope::with_current_app(|app| {
        for view in views {
            view.update(app, |view, cx| view.refresh(cx));
        }
    });
}

fn guest_by_id(id: u32) -> Option<Rc<Guest>> {
    REGISTRY.with(|registry| registry.borrow().guests.get(id as usize).cloned().flatten())
}

/// A view showing one of the guest's parts, through the generated part view
/// class. `nested_in` is the view it is drawn inside, for a child view; a
/// dialog's view is a root of its own.
fn part_view(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Guest,
    part: u32,
    nested_in: Option<gpui::EntityId>,
) -> JsResult<Entity<ScriptView>> {
    let factory: Function = ctx.globals().get("__wasm_part_view")?;
    let object: rquickjs::Object = factory.call((guest.id.get(), part))?;
    let object = super::ViewObject::unscoped(Persistent::save(ctx, object));
    let policy = scope::policy();
    let runtime = runtime.clone();
    let view = scope::with_current_app(move |cx| {
        cx.new(|_| match nested_in {
            Some(parent) => ScriptView::nested(runtime, object, policy, parent),
            None => ScriptView::with_policy(runtime, object, policy),
        })
    })
    .ok_or_else(|| js_error::type_error(ctx, "a part view needs a live host call"))?;
    guest.views.borrow_mut().push(view.downgrade());
    Ok(view)
}

/// Records a part drawn in place: a child view, made once per part and kept,
/// so the part keeps its own invalidation across the parent's renders.
fn nested_view(
    ctx: &Ctx<'_>,
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    part: u32,
) -> JsResult<SpecId> {
    let fail = |message: &str| js_error::type_error(ctx, message);
    let existing = guest
        .nested
        .borrow()
        .get(&part)
        .copied()
        .and_then(|handle| runtime.entities().view(handle).map(|view| (handle, view)));
    let (handle, view) = match existing {
        Some(found) => found,
        None => {
            let parent = scope::current_view()
                .map(|view| view.entity_id())
                .ok_or_else(|| fail("a nested part needs a view to be drawn in"))?;
            let view = part_view(ctx, runtime, guest, part, Some(parent))?;
            let handle = runtime
                .entities()
                .create_view(
                    view.clone(),
                    scope::current_application_generation(),
                    runtime,
                )
                .map_err(|_| fail("too many retained entities"))?;
            guest.nested.borrow_mut().insert(part, handle);
            (handle, view)
        }
    };
    runtime
        .arena_mut()
        .push_child_view(crate::spec::ChildViewSpec::new(handle, view))
        .map_err(|error| fail(&error.to_string()))
}

/// Opens the dialogs the guest asked for. A dialog's content is a view, and
/// making one needs the JavaScript context, which a module import does not
/// have; the request waits in the guest until a caller that has it gets here.
fn open_pending_dialogs(ctx: &Ctx<'_>, runtime: &Rc<ShellRuntime>, guest: &Guest) -> JsResult<()> {
    let pending = std::mem::take(&mut *guest.pending_dialogs.borrow_mut());
    for part in pending {
        let view = part_view(ctx, runtime, guest, part, None)?;
        let opened = scope::with_current(|window, app| {
            crate::ShellRoot::update(window, app, |root, window, cx| {
                root.open_dialog_with(view.into(), crate::DialogOptions::default(), window, cx)
            })
        })
        .flatten();
        if opened.is_none() {
            return Err(js_error::type_error(
                ctx,
                "open_dialog found no ShellRoot in this window; open dialogs from a handler",
            ));
        }
    }
    Ok(())
}

/// One instantiated guest module.
struct Guest {
    store: RefCell<Store<HostState>>,
    memory: Memory,
    render: TypedFunc<u32, u64>,
    dispatch: TypedFunc<(u32, u32, u32), u32>,
    alloc: TypedFunc<u32, u32>,
    /// The application generation the guest belongs to; [`release`] drops it
    /// with that generation.
    application: Option<Rc<ApplicationGeneration>>,
    /// The guest's id in the registry, which the part view class carries.
    id: std::cell::Cell<u32>,
    /// Every view showing the guest — its root and its parts — so a notify
    /// re-renders all of them.
    views: RefCell<Vec<WeakEntity<ScriptView>>>,
    /// The child view each nested part is drawn through, by part.
    nested: RefCell<HashMap<u32, EntityHandle>>,
    /// Parts the guest asked to open as dialogs, not yet opened.
    pending_dialogs: RefCell<Vec<u32>>,
}

struct HostState {
    runtime: Weak<ShellRuntime>,
    guest: Weak<Guest>,
    /// The view the guest was instantiated for, so a handler that runs from
    /// a native subscription (no script scope of its own) can open one. Weak:
    /// the view owns the guest's lifetime, not the other way round.
    view: Option<WeakEntity<ScriptView>>,
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
                    .and_then(|slot| slot.as_ref().map(|(module, _)| module.clone()))
                    .ok_or_else(|| anyhow!("wasm module {token} is unknown or released"))?,
            ))
        })?;

        let mut linker = Linker::new(&engine);
        link_imports(&mut linker)?;

        let mut store = Store::new(
            &engine,
            HostState {
                runtime: Rc::downgrade(runtime),
                guest: Weak::new(),
                view: scope::current_view().map(|view| view.downgrade()),
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
            application: scope::current_application_generation(),
            id: std::cell::Cell::new(0),
            views: RefCell::new(
                scope::current_view()
                    .map(|view| view.downgrade())
                    .into_iter()
                    .collect(),
            ),
            nested: RefCell::new(HashMap::new()),
            pending_dialogs: RefCell::new(Vec::new()),
        });
        guest.store.borrow_mut().data_mut().guest = Rc::downgrade(&guest);

        // `init` runs inside the host call the view class's `init` opened, so
        // the guest may create retained state through its imports.
        init.call(&mut *guest.store.borrow_mut(), ())
            .map_err(|error| anyhow!("guest init failed: {error:#}"))?;

        let id = REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            registry.guests.push(Some(guest.clone()));
            registry.guests.len() - 1
        });
        guest.id.set(id as u32);
        Ok(id as u32)
    }

    /// Renders the guest's view (`part` 0) or one of its parts.
    fn render(&self, part: u32) -> Result<Description> {
        let mut store = self.store.borrow_mut();
        let packed = self
            .render
            .call(&mut *store, part)
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
        self.dispatch_native_with(callback, b"null", window, app);
    }

    /// [`Self::dispatch_native`] with a JSON payload for the handler.
    fn dispatch_native_with(
        self: &Rc<Self>,
        callback: u32,
        payload: &[u8],
        window: &mut Window,
        app: &mut App,
    ) {
        let view = self
            .store
            .borrow()
            .data()
            .view
            .as_ref()
            .and_then(WeakEntity::upgrade);
        let Some(runtime) = self.store.borrow().data().runtime.upgrade() else {
            return;
        };
        let (_guard, _generation) =
            scope::enter_runtime(&runtime, window, app, ScopePhase::Event, view);
        match self.dispatch(callback, payload) {
            Ok(true) => refresh_guest(self),
            Ok(false) => {}
            Err(error) => tracing::error!("wasm guest handler failed: {error:#}"),
        }
        // No script call is running here, so the context is free to open.
        if !self.pending_dialogs.borrow().is_empty()
            && let Err(error) = runtime.with_js(|ctx| open_pending_dialogs(ctx, &runtime, self))
        {
            tracing::error!("wasm guest dialog failed: {error:#}");
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

fn read_bytes(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let memory = guest_memory(caller)?;
    let mut bytes = vec![0; len as usize];
    memory.read(&*caller, ptr as usize, &mut bytes)?;
    Ok(bytes)
}

/// Copies `text` into a buffer the guest allocated and returns it packed.
fn write_string(caller: &mut Caller<'_, HostState>, text: &str) -> Result<u64> {
    write_bytes(caller, text.as_bytes())
}

fn write_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> Result<u64> {
    let alloc = caller
        .get_export(abi::export::ALLOC)
        .and_then(Extern::into_func)
        .ok_or_else(|| anyhow!("the guest exports no allocator"))?
        .typed::<u32, u32>(&*caller)?;
    let len = bytes.len() as u32;
    let ptr = alloc.call(&mut *caller, len)?;
    guest_memory(caller)?.write(&mut *caller, ptr as usize, bytes)?;
    Ok(abi::pack(ptr, len))
}

fn data_from_guest(data: abi::Data) -> crate::ComponentDataValue {
    use crate::ComponentDataValue as Value;
    match data {
        abi::Data::Null => Value::Null,
        abi::Data::Boolean(value) => Value::Boolean(value),
        abi::Data::Number(value) => Value::Number(value),
        abi::Data::String(value) => Value::String(value),
        abi::Data::Array(values) => Value::Array(values.into_iter().map(data_from_guest).collect()),
        abi::Data::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, data_from_guest(value)))
                .collect(),
        ),
    }
}

fn data_to_guest(data: crate::ComponentDataValue) -> abi::Data {
    use crate::ComponentDataValue as Value;
    match data {
        Value::Null => abi::Data::Null,
        Value::Boolean(value) => abi::Data::Boolean(value),
        Value::Number(value) => abi::Data::Number(value),
        Value::String(value) => abi::Data::String(value),
        Value::Array(values) => abi::Data::Array(values.into_iter().map(data_to_guest).collect()),
        Value::Object(fields) => abi::Data::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, data_to_guest(value)))
                .collect(),
        ),
    }
}

/// A state constructor's arguments, checked against its schemas. Plain
/// values only: nothing a state is built from needs a render's elements or
/// handlers, and outside a render there is no callback generation open.
fn state_arguments(
    api: &str,
    descriptors: &[crate::ArgumentDescriptor],
    args: &[abi::ComponentArg],
) -> Result<Vec<crate::ComponentArgument>> {
    use crate::{ArgumentSchema as Schema, ComponentArgument as Argument};
    fn plain(schema: &Schema, arg: &abi::ComponentArg) -> Option<Argument> {
        use abi::ComponentArg as Guest_;
        Some(match (schema, arg) {
            (Schema::String, Guest_::String(value)) => Argument::String(value.clone()),
            (Schema::Number, Guest_::Number(value)) if value.is_finite() => {
                Argument::Number(*value)
            }
            (Schema::Boolean, Guest_::Boolean(value)) => Argument::Boolean(*value),
            (Schema::Enum(values), Guest_::Enum(value) | Guest_::String(value))
                if values.contains(&value.as_str()) =>
            {
                Argument::Enum(value.clone())
            }
            (Schema::Array(item), Guest_::Array(values)) => Argument::Array(
                values
                    .iter()
                    .map(|value| plain(item, value))
                    .collect::<Option<_>>()?,
            ),
            (Schema::Optional(_), Guest_::Optional(None)) => Argument::Optional(None),
            (Schema::Optional(inner), Guest_::Optional(Some(value))) => {
                Argument::Optional(Some(Box::new(plain(inner, value)?)))
            }
            (Schema::Optional(inner), value) => {
                Argument::Optional(Some(Box::new(plain(inner, value)?)))
            }
            _ => return None,
        })
    }
    anyhow::ensure!(
        args.len() <= descriptors.len(),
        "{api}(...) expects at most {} arguments",
        descriptors.len()
    );
    descriptors
        .iter()
        .enumerate()
        .map(|(index, descriptor)| {
            let converted = match (descriptor.schema(), args.get(index)) {
                (Schema::Optional(_), None) => Some(Argument::Optional(None)),
                (_, None) => None,
                (schema, Some(arg)) => plain(schema, arg),
            };
            converted.ok_or_else(|| {
                anyhow!(
                    "{api}({}) expects {}",
                    descriptor.name(),
                    super::schema_name(descriptor.schema())
                )
            })
        })
        .collect()
}

/// Runs `body` against the current application's persistent store, under the
/// same capability a script's `localStorage` is.
fn with_local_storage<R>(body: impl FnOnce(&mut crate::storage::Storage) -> R) -> Result<R> {
    let policy = scope::policy();
    anyhow::ensure!(
        policy.capabilities().has_storage(),
        "{}",
        crate::capability::CapabilityError::StorageDenied
    );
    policy.with_local_storage(body).ok_or_else(|| {
        anyhow!(
            "storage has no backing file; the host must call set_storage_path \
             before the application runs"
        )
    })
}

/// Schedules the store's pending revision to reach the disk.
///
/// The cache already answered; the write happens on the background executor,
/// one at a time through the store's own queue (as a script's do), and each
/// completion drives the queue again so a `set` made while a write was in
/// flight is picked up by the next one.
fn persist_local_storage() -> Result<()> {
    let policy = scope::policy();
    scope::with_current_app(|cx| drive_store(policy, cx))
        .ok_or_else(|| anyhow!("storage writes need a live host call"))
}

fn drive_store(policy: Rc<crate::policy::Policy>, cx: &mut App) {
    let stalled = policy
        .with_local_storage(crate::storage::Storage::take_stalled)
        .unwrap_or_default();
    for wake in stalled {
        wake();
    }
    let Some(Some(pending)) = policy.with_local_storage(crate::storage::Storage::begin_write)
    else {
        return;
    };
    let revision = pending.revision();
    let (path, body) = pending.into_parts();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { crate::storage::persist(&path, body) })
            .await;
        if let Err(error) = &result {
            tracing::error!("wasm guest storage write failed: {error}");
        }
        cx.update(|cx| {
            let woken = policy
                .with_local_storage(|store| store.finish_write(revision, result))
                .unwrap_or_default();
            for wake in woken {
                wake();
            }
            drive_store(policy, cx);
        });
    })
    .detach();
}

/// Runs `work` on the background executor and hands its JSON result to the
/// guest's handler `callback` back on the main thread — the shape of every
/// asynchronous import, since a core module has no promise to settle. A guest
/// released while the work was in flight (its application retired) is simply
/// not called.
fn run_in_background(
    guest: Weak<Guest>,
    callback: u32,
    work: impl FnOnce() -> serde_json::Value + Send + 'static,
) -> Result<()> {
    scope::with_current(|window, cx| {
        window
            .spawn(cx, async move |cx| {
                let payload = cx
                    .background_executor()
                    .spawn(async move { work() })
                    .await
                    .to_string();
                let _ = cx.update(|window, app| {
                    if let Some(guest) = guest.upgrade() {
                        guest.dispatch_native_with(callback, payload.as_bytes(), window, app);
                    }
                });
            })
            .detach();
    })
    .ok_or_else(|| anyhow!("an asynchronous call needs a live host call"))
}

fn http_result(result: std::result::Result<(u16, Vec<u8>), String>) -> serde_json::Value {
    match result {
        Ok((status, body)) => serde_json::json!({
            "status": status,
            "body": String::from_utf8_lossy(&body),
        }),
        Err(error) => serde_json::json!({ "error": error }),
    }
}

/// An HTTP request a guest describes as JSON.
#[derive(serde::Deserialize)]
struct GuestHttpRequest {
    #[serde(default = "get_method")]
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

fn get_method() -> String {
    "GET".to_owned()
}

/// A toast a guest asks for, as JSON.
#[derive(serde::Deserialize)]
struct GuestToast {
    title: String,
    description: Option<String>,
    level: Option<String>,
}

/// Overlays change only while handling an event or a task, as a script's do.
fn ensure_overlay_phase(api: &str) -> Result<()> {
    let phase = scope::current_phase();
    anyhow::ensure!(
        phase.is_some_and(ScopePhase::allows_notify),
        "{api} is not allowed during the `{}` phase; overlays may only be opened or \
         closed while handling an event or a task",
        phase.map(ScopePhase::as_str).unwrap_or("none")
    );
    Ok(())
}

/// A process a guest asks to run, as JSON.
#[derive(serde::Deserialize)]
struct GuestProcess {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

fn state_descriptor(runtime: &ShellRuntime, export: &str) -> Result<crate::StateDescriptor> {
    runtime
        .components
        .states()
        .find(|descriptor| descriptor.export() == export)
        .cloned()
        .ok_or_else(|| anyhow!("no registered state exports `{export}`"))
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
    // Network: `fetch`'s authorization on this thread (a denial traps the
    // call), the request on the background executor, and the guest's handler
    // back on the main thread with the result — a callback, because a core
    // module has no promise to settle.
    linker.func_wrap(
        module,
        abi::import::HTTP_GET,
        |mut caller: Caller<'_, HostState>,
         url_ptr: u32,
         url_len: u32,
         callback: u32|
         -> Result<()> {
            let url = read_string(&mut caller, url_ptr, url_len)?;
            let capabilities = scope::policy().capabilities().clone();
            let request =
                super::standard::fetch::authorize(&capabilities, "GET", &url, &[], Vec::new())
                    .map_err(|error| anyhow!(error))?;
            run_in_background(caller.data().guest.clone(), callback, move || {
                http_result(super::standard::fetch::send(capabilities, request))
            })
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::HTTP_REQUEST,
        |mut caller: Caller<'_, HostState>,
         request_ptr: u32,
         request_len: u32,
         callback: u32|
         -> Result<()> {
            let request: GuestHttpRequest =
                serde_json::from_slice(&read_bytes(&mut caller, request_ptr, request_len)?)?;
            let capabilities = scope::policy().capabilities().clone();
            let request = super::standard::fetch::authorize(
                &capabilities,
                &request.method,
                &request.url,
                &request.headers,
                request.body.into_bytes(),
            )
            .map_err(|error| anyhow!(error))?;
            run_in_background(caller.data().guest.clone(), callback, move || {
                http_result(super::standard::fetch::send(capabilities, request))
            })
        },
    )?;
    // Processes: the same bounded runner and execute grant a script's
    // `process.run` uses, off the main thread.
    linker.func_wrap(
        module,
        abi::import::PROCESS_RUN,
        |mut caller: Caller<'_, HostState>,
         request_ptr: u32,
         request_len: u32,
         callback: u32|
         -> Result<()> {
            let request: GuestProcess =
                serde_json::from_slice(&read_bytes(&mut caller, request_ptr, request_len)?)?;
            anyhow::ensure!(
                scope::policy().capabilities().may_run(&request.command),
                "{}",
                crate::capability::CapabilityError::ExecuteDenied(request.command.clone())
            );
            run_in_background(caller.data().guest.clone(), callback, move || {
                match crate::process::run_bounded(
                    &request.command,
                    &request.args,
                    None,
                    crate::process::Limits::default(),
                    crate::process::Cancellation::new(),
                ) {
                    Ok(output) => serde_json::json!({
                        "code": output.code,
                        "stdout": output.stdout,
                        "stderr": output.stderr,
                    }),
                    Err(error) => serde_json::json!({ "error": error }),
                }
            })
        },
    )?;
    // Overlays, from a handler or a task only, as a script's are.
    linker.func_wrap(
        module,
        abi::import::OPEN_DIALOG,
        |caller: Caller<'_, HostState>, part: u32| -> Result<()> {
            ensure_overlay_phase("open_dialog")?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            guest.pending_dialogs.borrow_mut().push(part);
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::CLOSE_DIALOG,
        |_caller: Caller<'_, HostState>| -> Result<()> {
            ensure_overlay_phase("close_dialog")?;
            scope::with_current(|window, app| {
                crate::ShellRoot::update(window, app, |root, window, cx| {
                    root.close_dialog(window, cx)
                })
            })
            .flatten()
            .map(|_| ())
            .ok_or_else(|| anyhow!("close_dialog found no ShellRoot in this window"))
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::PUSH_TOAST,
        |mut caller: Caller<'_, HostState>, toast_ptr: u32, toast_len: u32| -> Result<()> {
            ensure_overlay_phase("push_toast")?;
            let toast: GuestToast =
                serde_json::from_slice(&read_bytes(&mut caller, toast_ptr, toast_len)?)?;
            let level = match toast.level.as_deref() {
                Some("success") => crate::ToastLevel::Success,
                Some("warning") => crate::ToastLevel::Warning,
                Some("error") => crate::ToastLevel::Error,
                _ => crate::ToastLevel::Info,
            };
            let mut request = crate::ToastRequest::new(toast.title).with_level(level);
            if let Some(description) = toast.description {
                request = request.with_description(description);
            }
            scope::with_current(|window, app| {
                crate::ShellRoot::update(window, app, |root, window, cx| {
                    root.push_toast(request, window, cx)
                })
            })
            .flatten()
            .ok_or_else(|| anyhow!("push_toast found no ShellRoot in this window"))
        },
    )?;
    // Session storage: the policy's in-memory store, which a script's
    // `sessionStorage` reaches without a capability.
    linker.func_wrap(
        module,
        abi::import::SESSION_GET,
        |mut caller: Caller<'_, HostState>, key_ptr: u32, key_len: u32| -> Result<u64> {
            let key = read_string(&mut caller, key_ptr, key_len)?;
            let value = scope::policy()
                .with_session_storage(|store| {
                    store.values().map(|values| {
                        values.get(&key).map(|value| match value {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                    })
                })
                .ok_or_else(|| anyhow!("no session storage"))?
                .map_err(|error| anyhow!(error))?;
            match value {
                Some(value) => write_string(&mut caller, &value),
                None => Ok(abi::ABSENT),
            }
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::SESSION_SET,
        |mut caller: Caller<'_, HostState>,
         key_ptr: u32,
         key_len: u32,
         value_ptr: u32,
         value_len: u32|
         -> Result<()> {
            let key = read_string(&mut caller, key_ptr, key_len)?;
            let value = read_string(&mut caller, value_ptr, value_len)?;
            scope::policy()
                .with_session_storage(|store| store.set(key, serde_json::Value::String(value)))
                .ok_or_else(|| anyhow!("no session storage"))?
                .map_err(|error| anyhow!(error))
        },
    )?;
    // The clipboard, under its read and write grants.
    linker.func_wrap(
        module,
        abi::import::CLIPBOARD_READ,
        |mut caller: Caller<'_, HostState>| -> Result<u64> {
            anyhow::ensure!(
                scope::policy().capabilities().is_clipboard_readable(),
                "{}",
                super::host::CLIPBOARD_READ_DENIED
            );
            let text =
                scope::with_current_app(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
                    .ok_or_else(|| anyhow!("clipboard_read needs a live host call"))?;
            match text {
                Some(text) => write_string(&mut caller, &text),
                None => Ok(abi::ABSENT),
            }
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::CLIPBOARD_WRITE,
        |mut caller: Caller<'_, HostState>, text_ptr: u32, text_len: u32| -> Result<()> {
            anyhow::ensure!(
                scope::policy().capabilities().is_clipboard_writable(),
                "{}",
                super::host::CLIPBOARD_WRITE_DENIED
            );
            let text = read_string(&mut caller, text_ptr, text_len)?;
            scope::with_current_app(|cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text))
            })
            .ok_or_else(|| anyhow!("clipboard_write needs a live host call"))
        },
    )?;
    // Asynchronous files: the grant is resolved here, the I/O on the
    // background executor through the directory handle it opened.
    linker.func_wrap(
        module,
        abi::import::FS_READ_TEXT_ASYNC,
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         callback: u32|
         -> Result<()> {
            let path = read_string(&mut caller, path_ptr, path_len)?;
            let grant = scope::policy()
                .capabilities()
                .open(Path::new(&path), crate::capability::Access::Read)
                .map_err(|error| anyhow!("{error}"))?;
            let (dir, relative) = grant.into_parts();
            run_in_background(caller.data().guest.clone(), callback, move || {
                match dir.read_to_string(&relative) {
                    Ok(text) => serde_json::json!({ "text": text }),
                    Err(error) => {
                        serde_json::json!({ "error": format!("cannot read `{path}`: {error}") })
                    }
                }
            })
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::FS_WRITE_TEXT_ASYNC,
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         text_ptr: u32,
         text_len: u32,
         callback: u32|
         -> Result<()> {
            let path = read_string(&mut caller, path_ptr, path_len)?;
            let text = read_bytes(&mut caller, text_ptr, text_len)?;
            let grant = scope::policy()
                .capabilities()
                .open(Path::new(&path), crate::capability::Access::Write)
                .map_err(|error| anyhow!("{error}"))?;
            let (dir, relative) = grant.into_parts();
            run_in_background(caller.data().guest.clone(), callback, move || {
                match dir.write(&relative, text) {
                    Ok(()) => serde_json::json!({ "ok": true }),
                    Err(error) => {
                        serde_json::json!({ "error": format!("cannot write `{path}`: {error}") })
                    }
                }
            })
        },
    )?;
    // Files: the same resolver a script's `fs` goes through, which opens the
    // granted directory once and works on that handle. A script's call runs
    // on a blocking task; the spike's runs inline.
    linker.func_wrap(
        module,
        abi::import::FS_READ_TEXT,
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> Result<u64> {
            use std::io::Read as _;
            const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
            let path = read_string(&mut caller, path_ptr, path_len)?;
            let grant = scope::policy()
                .capabilities()
                .open(Path::new(&path), crate::capability::Access::Read)
                .map_err(|error| anyhow!("{error}"))?;
            let (dir, relative) = grant.into_parts();
            let mut text = String::new();
            dir.open(&relative)?
                .take(MAX_READ_BYTES + 1)
                .read_to_string(&mut text)?;
            anyhow::ensure!(
                text.len() as u64 <= MAX_READ_BYTES,
                "`{path}` is larger than {MAX_READ_BYTES} bytes"
            );
            write_string(&mut caller, &text)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::FS_WRITE_TEXT,
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         text_ptr: u32,
         text_len: u32|
         -> Result<()> {
            let path = read_string(&mut caller, path_ptr, path_len)?;
            let text = read_bytes(&mut caller, text_ptr, text_len)?;
            let grant = scope::policy()
                .capabilities()
                .open(Path::new(&path), crate::capability::Access::Write)
                .map_err(|error| anyhow!("{error}"))?;
            let (dir, relative) = grant.into_parts();
            dir.write(&relative, text)?;
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STORAGE_GET,
        |mut caller: Caller<'_, HostState>, key_ptr: u32, key_len: u32| -> Result<u64> {
            let key = read_string(&mut caller, key_ptr, key_len)?;
            let value = with_local_storage(|store| {
                store.values().map(|values| {
                    values.get(&key).map(|value| match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                })
            })?
            .map_err(|error| anyhow!(error))?;
            match value {
                Some(value) => write_string(&mut caller, &value),
                None => Ok(abi::ABSENT),
            }
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STORAGE_SET,
        |mut caller: Caller<'_, HostState>,
         key_ptr: u32,
         key_len: u32,
         value_ptr: u32,
         value_len: u32|
         -> Result<()> {
            let key = read_string(&mut caller, key_ptr, key_len)?;
            let value = read_string(&mut caller, value_ptr, value_len)?;
            with_local_storage(|store| store.set(key, serde_json::Value::String(value)))?
                .map_err(|error| anyhow!(error))?;
            persist_local_storage()
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STORAGE_REMOVE,
        |mut caller: Caller<'_, HostState>, key_ptr: u32, key_len: u32| -> Result<()> {
            let key = read_string(&mut caller, key_ptr, key_len)?;
            with_local_storage(|store| {
                store.values().map(|values| {
                    values.remove(&key);
                })?;
                store.touch();
                Ok::<_, String>(())
            })?
            .map_err(|error| anyhow!(error))?;
            persist_local_storage()
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STATE_NEW,
        |mut caller: Caller<'_, HostState>,
         export_ptr: u32,
         export_len: u32,
         args_ptr: u32,
         args_len: u32|
         -> Result<u64> {
            let export = read_string(&mut caller, export_ptr, export_len)?;
            let args: Vec<abi::ComponentArg> =
                postcard::from_bytes(&read_bytes(&mut caller, args_ptr, args_len)?)?;
            let runtime = runtime_of(&caller)?;
            anyhow::ensure!(
                !matches!(
                    scope::current_phase(),
                    Some(ScopePhase::Render | ScopePhase::Layout)
                ),
                "{export}.new(...) cannot run during render; create state in init or a handler"
            );
            let descriptor = state_descriptor(&runtime, &export)?;
            let arguments = state_arguments(&export, descriptor.arguments(), &args)?;
            runtime.flush_component_state_releases();
            let state = scope::with_current(|window, cx| descriptor.create(&arguments, window, cx))
                .ok_or_else(|| anyhow!("{export}.new(...) needs a live host call"))?
                .map_err(|error| anyhow!("{export}.new(...): {error}"))?;
            runtime
                .component_states
                .try_borrow_mut()
                .map_err(|_| anyhow!("retained component state is already borrowed"))?
                .insert(
                    descriptor.kind(),
                    scope::current_application_generation(),
                    state,
                )
                .map_err(|error| anyhow!(error))
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STATE_CALL,
        |mut caller: Caller<'_, HostState>,
         handle: u64,
         export_ptr: u32,
         export_len: u32,
         method_ptr: u32,
         method_len: u32,
         args_ptr: u32,
         args_len: u32|
         -> Result<u64> {
            let export = read_string(&mut caller, export_ptr, export_len)?;
            let name = read_string(&mut caller, method_ptr, method_len)?;
            let args: Vec<abi::Data> =
                postcard::from_bytes(&read_bytes(&mut caller, args_ptr, args_len)?)?;
            let runtime = runtime_of(&caller)?;
            let descriptor = state_descriptor(&runtime, &export)?;
            let method = descriptor
                .methods()
                .iter()
                .find(|method| method.name() == name)
                .cloned()
                .ok_or_else(|| anyhow!("`{export}` has no method `{name}`"))?;
            anyhow::ensure!(
                method.is_readonly()
                    || !matches!(
                        scope::current_phase(),
                        Some(ScopePhase::Render | ScopePhase::Layout)
                    ),
                "{export}.{name}(...) changes state and cannot run during render"
            );
            runtime.flush_component_state_releases();
            // Prepared under the store's borrow and run after it, so the call
            // can reach GPUI without the state table held.
            let call = {
                let store = runtime
                    .component_states
                    .try_borrow()
                    .map_err(|_| anyhow!("retained component state is already borrowed"))?;
                method.prepare(
                    &store,
                    handle,
                    descriptor.kind(),
                    args.into_iter().map(data_from_guest).collect(),
                )?
            };
            let result = scope::with_current(move |window, cx| call(window, cx))
                .ok_or_else(|| anyhow!("{export}.{name}(...) needs a live host call"))??;
            write_bytes(&mut caller, &postcard::to_allocvec(&data_to_guest(result))?)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::STATE_ON,
        |mut caller: Caller<'_, HostState>,
         handle: u64,
         export_ptr: u32,
         export_len: u32,
         event_ptr: u32,
         event_len: u32,
         callback: u32|
         -> Result<()> {
            let export = read_string(&mut caller, export_ptr, export_len)?;
            let event = read_string(&mut caller, event_ptr, event_len)?;
            let event = InputEventName::from_name(&event).ok_or_else(|| {
                anyhow!("`{event}` is not an input event; expected change, submit, focus or blur")
            })?;
            let runtime = runtime_of(&caller)?;
            let descriptor = state_descriptor(&runtime, &export)?;
            let guest = caller.data().guest.clone();
            let handler: crate::component_registry::InputEventHandler =
                Box::new(move |_event, window, app| {
                    if let Some(guest) = guest.upgrade() {
                        guest.dispatch_native(callback, window, app);
                    }
                });
            let subscription = scope::with_current(|window, cx| {
                runtime
                    .component_states
                    .try_borrow()
                    .map_err(|_| anyhow!("retained component state is already borrowed"))?
                    .subscribe(handle, &descriptor, event, handler, window, cx)
            })
            .ok_or_else(|| anyhow!("{export}.on(...) needs a live host call"))??;
            runtime
                .component_states
                .try_borrow_mut()
                .map_err(|_| anyhow!("retained component state is already borrowed"))?
                .attach(handle, subscription)
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

    /// An application directory holding example `example`'s guest and a
    /// manifest naming it as the entry.
    fn application(example: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "gpui-shell-{example}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(build_guest(example), directory.join("guest.wasm")).unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            format!(
                r#"{{"id": "dev.test.{example}", "name": "{example}", "entry": "guest.wasm"}}"#
            ),
        )
        .unwrap();
        directory
    }

    struct Harness {
        context: VisualTestContext,
        runtime: Rc<ShellRuntime>,
        view: Entity<ScriptView>,
        root: Entity<crate::ShellRoot>,
    }

    impl Harness {
        fn load(cx: &mut TestAppContext) -> Self {
            Self::load_example(cx, "wasm_todolist")
        }

        fn load_example(cx: &mut TestAppContext, example: &str) -> Self {
            let directory = application(example);
            // The runtime the shipped command builds: with the catalog.
            let components = crate::components::components().expect("catalog");
            cx.update(|cx| crate::init_with_components(cx, &components));
            let runtime = cx
                .update(|cx| ShellRuntime::new_with_components(cx, components))
                .expect("runtime");
            // The loaded root *is* the window's root, as the shipped command
            // opens it: overlays look for it there.
            let load_runtime = runtime.clone();
            let window = cx
                .update(|cx| {
                    cx.open_window(gpui::WindowOptions::default(), move |window, cx| {
                        load_runtime
                            .try_load(&directory, window, cx)
                            .expect("the guest loads through its manifest")
                    })
                })
                .expect("a window");
            let mut context = VisualTestContext::from_window(*window.deref(), cx);
            let root = window.root(&mut context).expect("the window's root");
            let view = context.update(|_, cx| {
                root.read(cx)
                    .content()
                    .clone()
                    .downcast::<ScriptView>()
                    .expect("a guest mounts as a script view")
            });
            let mut harness = Self {
                context,
                runtime,
                view,
                root,
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
            self.tree_of(&view)
        }

        fn tree_of(&mut self, view: &Entity<ScriptView>) -> String {
            let view = view.clone();
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
            self.find_in(&view, pick)
        }

        fn find_in<T>(
            &mut self,
            view: &Entity<ScriptView>,
            pick: impl Fn(&SpecNode) -> Option<T>,
        ) -> T {
            let view = view.clone();
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
            let view = self.view.clone();
            self.handler_in(&view, element, event)
        }

        fn handler_in(
            &mut self,
            view: &Entity<ScriptView>,
            element: &str,
            event: &'static str,
        ) -> crate::spec::CallbackId {
            let element = element.to_owned();
            self.find_in(view, move |node| {
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
        // The row's hover style, collected on a detached node.
        assert!(tree.contains(":hover(.bg[Str(\"#"), "{tree}");
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

        // Retiring the application releases its guest with the rest of its
        // state, as a reload does to the generation it replaces.
        let live = live_guests();
        assert!(live >= 1);
        let view = app.view.clone();
        let runtime = app.runtime.clone();
        app.context.update(|_, cx| {
            let application = view
                .read(cx)
                .application_generation()
                .expect("a loaded application has a generation");
            runtime.release_application_generation(&application, cx);
        });
        assert_eq!(live_guests(), live - 1);
    }

    /// Every live view showing guest `id` other than its root: its parts.
    fn part_views(
        cx: &mut VisualTestContext,
        root: &Entity<ScriptView>,
    ) -> Vec<Entity<ScriptView>> {
        let root = root.entity_id();
        let views: Vec<_> = REGISTRY.with(|registry| {
            registry
                .borrow()
                .guests
                .iter()
                .flatten()
                .flat_map(|guest| guest.views.borrow().clone())
                .collect()
        });
        cx.update(|_, _| {
            views
                .iter()
                .filter_map(WeakEntity::upgrade)
                .filter(|view| view.entity_id() != root)
                .collect()
        })
    }

    #[gpui::test]
    fn a_guest_nests_parts_opens_dialogs_and_pushes_toasts(cx: &mut TestAppContext) {
        let mut app = Harness::load_example(cx, "wasm_parts");
        let tree = app.tree();
        assert!(tree.contains("Total: 0"), "{tree}");

        // The counter is a child view of its own, with its own description.
        let parts = part_views(&mut app.context, &app.view);
        assert_eq!(parts.len(), 1, "one nested part");
        let counter = parts[0].clone();
        let counter_tree = app.tree_of(&counter);
        assert!(counter_tree.contains("Count: 0"), "{counter_tree}");

        // A handler in the part changes shared state; every view re-renders.
        let bump = app.handler_in(&counter, "bump", "on_click");
        app.dispatch_click(bump);
        let counter_tree = app.tree_of(&counter);
        assert!(counter_tree.contains("Count: 1"), "{counter_tree}");
        let tree = app.tree();
        assert!(tree.contains("Total: 1"), "{tree}");

        // A part opened as a dialog over the window.
        app.click("open");
        let root = app.root.clone();
        let dialogs = app.context.update(|_, cx| root.read(cx).dialog_count());
        assert_eq!(dialogs, 1);
        let dialog = part_views(&mut app.context, &app.view)
            .into_iter()
            .find(|view| view.entity_id() != counter.entity_id())
            .expect("the dialog's view");
        app.settle();
        let dialog_tree = app.tree_of(&dialog);
        assert!(dialog_tree.contains("Reset 1?"), "{dialog_tree}");

        // Its handler resets the count, closes itself and pushes a toast.
        let reset = app.handler_in(&dialog, "reset", "on_click");
        app.dispatch_click(reset);
        let (dialogs, toasts) = app
            .context
            .update(|_, cx| (root.read(cx).dialog_count(), root.read(cx).toast_count()));
        assert_eq!(dialogs, 0);
        assert_eq!(toasts, 1);
        let tree = app.tree();
        assert!(tree.contains("Total: 0"), "{tree}");
    }

    #[gpui::test]
    fn a_guest_keeps_registered_state_and_calls_its_methods(cx: &mut TestAppContext) {
        let mut app = Harness::load_example(cx, "wasm_state");
        let tree = app.tree();
        assert!(tree.contains("Input"), "{tree}");
        assert!(tree.contains("Echo: "), "{tree}");

        // The catalog's own InputState, created by the guest's `state_new`.
        let runtime = app.runtime.clone();
        let input = {
            let store = runtime.component_states.borrow();
            // Handles are numbered from zero.
            let handle = (0..64)
                .find(|handle| store.kind(*handle) == Some("InputState"))
                .expect("the guest created an InputState");
            store
                .with::<Entity<gpui_component::input::InputState>, _>(
                    handle,
                    "InputState",
                    Clone::clone,
                )
                .expect("a gpui-component InputState")
        };

        // Echo reads the state through its `value()` method.
        app.context.update(|window, cx| {
            input.update(cx, |state, cx| state.set_value("hello", window, cx))
        });
        app.click_registered("Button");
        let tree = app.tree();
        assert!(tree.contains("Echo: hello"), "{tree}");

        // Enter reaches the guest through the state's `submit` event; the
        // handler reads the value and clears it with `set_value`.
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
        assert!(tree.contains("Submitted: hello"), "{tree}");
        let cleared = app
            .context
            .update(|_, cx| input.read(cx).value().to_string());
        assert_eq!(cleared, "");
    }

    /// The storage guest, loaded into a runtime without the catalog (it draws
    /// only base elements), under the host's default policy.
    fn start_storage_guest(cx: &mut TestAppContext) -> Result<String> {
        let directory = application("wasm_storage");
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        cx.update(|cx| runtime.set_global(cx));
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let view_type = runtime.load_app(&directory, "guest.wasm")?;
        let object = context.update(|window, cx| runtime.instantiate(&view_type, window, cx))?;
        let snapshot = context.update(|window, cx| {
            runtime.build_snapshot(&object, None, crate::policy::default(), window, cx)
        })?;
        Ok(snapshot.debug_tree())
    }

    #[gpui::test]
    fn a_guest_persists_through_the_storage_capability(cx: &mut TestAppContext) {
        let file = std::env::temp_dir().join(format!(
            "gpui-shell-wasm-storage-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        cx.update(|cx| crate::init(cx));
        crate::set_capabilities(crate::Capabilities::new().storage(true));
        crate::set_storage_path(file.clone());

        let first = start_storage_guest(cx).expect("first start");
        assert!(first.contains("Visits: 1"), "{first}");
        let second = start_storage_guest(cx).expect("second start");
        assert!(second.contains("Visits: 2"), "{second}");
        // The cache answered at once; the write reaches the disk off the
        // main thread.
        cx.run_until_parked();
        let stored = std::fs::read_to_string(&file).expect("the store reached the disk");
        assert!(stored.contains("\"visits\""), "{stored}");
    }

    #[gpui::test]
    fn a_guest_reads_and_writes_files_inside_its_fs_grants(cx: &mut TestAppContext) {
        let directory = std::env::temp_dir().join(format!(
            "gpui-shell-wasm-fs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let notes = directory.join("notes.txt");
        std::fs::write(&notes, "remember the milk").unwrap();
        let store = directory.join("store.json");
        std::fs::write(
            &store,
            serde_json::json!({ "notes_path": notes.to_string_lossy() }).to_string(),
        )
        .unwrap();

        cx.update(|cx| crate::init(cx));
        crate::set_storage_path(store);
        crate::set_capabilities(
            crate::Capabilities::new()
                .storage(true)
                .read_roots([directory.clone()])
                .write_roots([directory.clone()]),
        );
        let tree = start_storage_guest(cx).expect("start");
        assert!(tree.contains("Notes: remember the milk"), "{tree}");
        let copy = std::fs::read_to_string(directory.join("notes.txt.copy")).unwrap();
        assert_eq!(copy, "REMEMBER THE MILK");

        // Without a read grant the same call traps with the capability's
        // message.
        crate::set_capabilities(crate::Capabilities::new().storage(true));
        let error = start_storage_guest(cx).expect_err("no fs grant");
        let message = format!("{error:#}");
        assert!(
            message.contains("filesystem read is not granted; declare capabilities.fs.read"),
            "{message}"
        );
    }

    /// A loopback server that answers one request with `body`.
    fn serve_once(body: &'static str) -> u16 {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        port
    }

    /// The fetch guest mounted as a view, with its store naming `url`.
    fn start_fetch_guest(
        cx: &mut TestAppContext,
        url: &str,
        capabilities: crate::Capabilities,
    ) -> Result<(VisualTestContext, Entity<ScriptView>)> {
        let directory = application("wasm_fetch");
        let store = directory.join("store.json");
        std::fs::write(&store, serde_json::json!({ "url": url }).to_string()).unwrap();
        cx.update(|cx| crate::init(cx));
        crate::set_storage_path(store);
        crate::set_capabilities(capabilities.storage(true));
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        cx.update(|cx| runtime.set_global(cx));
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let view_type = runtime.load_app(&directory, "guest.wasm")?;
        let view = context.update(|window, cx| runtime.instantiate_view(&view_type, window, cx))?;
        Ok((context, view))
    }

    fn drawn_tree(context: &mut VisualTestContext, view: &Entity<ScriptView>) -> String {
        let element = view.clone();
        context.draw(
            gpui::Point::default(),
            gpui::size(gpui::px(400.), gpui::px(300.)),
            move |_, _| element.into_any_element(),
        );
        let view = view.clone();
        context.update(|_, cx| {
            view.read(cx)
                .snapshot()
                .map(RenderSnapshot::debug_tree)
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn a_guest_fetches_without_blocking_and_hears_back(cx: &mut TestAppContext) {
        let port = serve_once("hello from the network");
        let (mut context, view) = start_fetch_guest(
            cx,
            &format!("http://127.0.0.1:{port}/greeting"),
            crate::Capabilities::new().network_hosts(["127.0.0.1".to_owned()]),
        )
        .expect("start");

        // `init` returned before the response: the first frame is pending.
        let first = drawn_tree(&mut context, &view);
        assert!(first.contains("Status: pending"), "{first}");

        // The response arrives off the main thread; the handler runs back on
        // it and asks for a render.
        let mut tree = String::new();
        for _ in 0..200 {
            context.run_until_parked();
            tree = drawn_tree(&mut context, &view);
            if tree.contains("Status: 200") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(tree.contains("Status: 200"), "{tree}");
        assert!(tree.contains("Body: hello from the network"), "{tree}");
    }

    #[gpui::test]
    fn a_request_outside_the_network_grants_is_refused(cx: &mut TestAppContext) {
        let error = start_fetch_guest(
            cx,
            "http://127.0.0.1:9/greeting",
            crate::Capabilities::new(),
        )
        .err()
        .expect("no network grant");
        let message = format!("{error:#}");
        assert!(
            message.contains("HTTP request GET http://127.0.0.1:9/greeting is not granted"),
            "{message}"
        );
    }

    /// A loopback server that answers one request with its own method and
    /// body, and whether the guest's header arrived.
    fn serve_echo_once() -> u16 {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + length {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
            }
            let method = head
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            let body = String::from_utf8_lossy(&request[header_end..header_end + length]);
            let header = head.contains("x-guest: wasm");
            let reply = format!("{method} {body} header={header}");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        });
        port
    }

    #[gpui::test]
    fn a_guest_reaches_the_remaining_host_services(cx: &mut TestAppContext) {
        let port = serve_echo_once();
        let directory = application("wasm_services");
        let data = directory.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let data = data.canonicalize().unwrap();
        let notes = data.join("notes.txt");
        std::fs::write(&notes, "read off the main thread").unwrap();
        let store = directory.join("store.json");
        std::fs::write(
            &store,
            serde_json::json!({
                "url": format!("http://127.0.0.1:{port}/echo"),
                "notes_path": notes.to_string_lossy(),
            })
            .to_string(),
        )
        .unwrap();

        let components = crate::components::components().expect("catalog");
        cx.update(|cx| crate::init_with_components(cx, &components));
        crate::set_storage_path(store);
        crate::set_capabilities(
            crate::Capabilities::new()
                .storage(true)
                .network_hosts(["127.0.0.1".to_owned()])
                .read_roots([data.clone()])
                .write_roots([data.clone()])
                .execute(crate::ExecuteGrant::Allowed(vec!["echo".to_owned()]))
                .clipboard_read(true)
                .clipboard_write(true),
        );
        let runtime = cx
            .update(|cx| ShellRuntime::new_with_components(cx, components))
            .expect("runtime");
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let view_type = runtime.load_app(&directory, "guest.wasm").expect("load");
        let view = context
            .update(|window, cx| runtime.instantiate_view(&view_type, window, cx))
            .expect("instantiate");

        // The synchronous services answered in `init`, and the slot is
        // rendered by the component rather than among its children.
        let first = drawn_tree(&mut context, &view);
        assert!(first.contains("Session: hello session"), "{first}");
        assert!(first.contains("Clipboard: copied text"), "{first}");
        assert!(first.contains("@content"), "{first}");

        // The asynchronous ones fill in as their handlers run.
        let mut tree = String::new();
        for _ in 0..300 {
            context.run_until_parked();
            tree = drawn_tree(&mut context, &view);
            if tree.contains("Http: 200")
                && tree.contains("Process: from a process")
                && tree.contains("Notes: read off the main thread")
                && tree.contains("Wrote: true")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(tree.contains("Process: from a process"), "{tree}");
        assert!(tree.contains("Http: 200 POST ping header=true"), "{tree}");
        assert!(tree.contains("Notes: read off the main thread"), "{tree}");
        assert!(tree.contains("Wrote: true"), "{tree}");
        assert_eq!(
            std::fs::read_to_string(data.join("notes.txt.async")).unwrap(),
            "ASYNC"
        );
    }

    #[gpui::test]
    fn storage_without_the_capability_is_refused(cx: &mut TestAppContext) {
        cx.update(|cx| crate::init(cx));
        crate::set_capabilities(crate::Capabilities::new());
        let error = start_storage_guest(cx).expect_err("no storage grant");
        let message = format!("{error:#}");
        assert!(
            message.contains(&crate::capability::CapabilityError::StorageDenied.to_string()),
            "{message}"
        );
    }

    #[gpui::test]
    fn a_mistyped_component_argument_is_refused_with_the_schema(cx: &mut TestAppContext) {
        let directory = std::env::temp_dir().join(format!(
            "gpui-shell-wasm-bad-args-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(build_guest("wasm_bad_args"), directory.join("bad.wasm")).unwrap();

        let components = crate::components::components().expect("catalog");
        cx.update(|cx| crate::init_with_components(cx, &components));
        let runtime = cx
            .update(|cx| ShellRuntime::new_with_components(cx, components))
            .expect("runtime");
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let view_type = runtime.load_app(&directory, "bad.wasm").expect("load");
        let object = context
            .update(|window, cx| runtime.instantiate(&view_type, window, cx))
            .expect("instantiate");
        let error = context
            .update(|window, cx| {
                runtime.build_snapshot(&object, None, crate::policy::default(), window, cx)
            })
            .err()
            .expect("a mistyped argument fails the render");
        let message = format!("{error:#}");
        assert!(
            message.contains("Progress(id) expects a string"),
            "{message}"
        );
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
        let instance = REGISTRY.with(|registry| {
            registry
                .borrow()
                .guests
                .iter()
                .flatten()
                .last()
                .cloned()
                .unwrap()
        });
        let mut bytes = 0;
        let guest_only = best_per_call(|| {
            let description = instance.render(0).expect("render");
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
        // Both orders: whichever is timed first has been seen to come out
        // slower, so each side's figure is its better reading of the two.
        let guest_first = materialize(&mut context, &guest_snapshot);
        let script_second = materialize(&mut context, &script_snapshot);
        let script_first = materialize(&mut context, &script_snapshot);
        let guest_second = materialize(&mut context, &guest_snapshot);
        println!(
            "\n[B order] guest then script: {:.3} / {:.3} ms; script then guest: {:.3} / {:.3} ms",
            guest_first.as_secs_f64() * 1000.0,
            script_second.as_secs_f64() * 1000.0,
            script_first.as_secs_f64() * 1000.0,
            guest_second.as_secs_f64() * 1000.0,
        );
        let guest_materialize = guest_first.min(guest_second);
        let script_materialize = script_first.min(script_second);

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
