//! A WebAssembly guest: one instance of an application's core module (see
//! `gpui-shell-wasm-abi`), its imports, and the recording of what it renders.
//!
//! A guest's `render` returns a whole description at once, which is recorded
//! into the runtime's spec arena here; its handlers are entries in the
//! ordinary callback arena that call its `dispatch` export.
//!
//! What a guest reaches: the `gpui-base` elements, state styles, named slots,
//! registered components (validated against their schemas like a script's
//! arguments) and their retained state (methods and input events), lazily
//! rendered lists, and *parts*: render functions shown by views of their
//! own, as nested child views, dialogs, sheets or dock panels (whose layout
//! the guest can save and restore, under a tab strip the host draws); toasts;
//! and, under the application's capabilities, the persistent and session
//! stores, the clipboard, files, processes (run, streamed, or fed on stdin),
//! HTTP and WebSockets (text and binary). Calls a script makes
//! asynchronously are asynchronous here too, callback-style (a core module
//! has no promise to settle): the work runs off the main thread and the
//! handler back on it, once per event for a stream. Processes and sockets
//! are stopped with the guest's application generation.
//!
//! Known gaps:
//!
//! * Dock chrome is the host's fixed tab strip: a guest cannot draw its own
//!   (a script's chrome handlers run in layout, which a guest would have to
//!   describe per group). Panels persist their place but no payload of their
//!   own: guest state is the guest's to keep. Panel names are global per
//!   name, so two live instances of one guest share a registration.
//! * A component callback that must return a value (a table's cell data, an
//!   element renderer) has no guest counterpart yet; a guest handler reports
//!   only whether it changed state.
//! * Descriptions cross as postcard; handler payloads as JSON.

mod retained;

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    path::Path,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Entity, WeakEntity, Window};
use gpui_shell_wasm_abi::{self as abi, Arg, Description, Op};
use serde_json::Value as Json;
use smallvec::SmallVec;
use wasmtime::{Caller, Extern, Instance, Linker, Memory, Module, Store, TypedFunc};

use super::{HandlerTarget, ShellRuntime, finish_task, own_task};
use crate::{
    entities::{EntityHandle, InputEventName},
    runtime::ApplicationGeneration,
    scope::{self, ScopePhase},
    spec::{Component, SpecId, SpecOp},
    value::Bridged,
    view::ScriptView,
};
use abi::retained::target as retained_target;

/// Read and write are separate grants, so the denial names the half that was
/// missing.
const CLIPBOARD_READ_DENIED: &str =
    "reading the clipboard is not granted; declare capabilities.clipboard.read in the manifest";
const CLIPBOARD_WRITE_DENIED: &str =
    "writing the clipboard is not granted; declare capabilities.clipboard.write in the manifest";

/// Something a guest started that runs on its own until stopped.
enum Resource {
    Process {
        killer: crate::process::Killer,
        stdin: Option<crate::process::StdinWriter>,
    },
    Socket(std::sync::mpsc::Sender<SocketCommand>),
}

impl Resource {
    fn stop(self) {
        match self {
            Resource::Process { killer, .. } => killer.kill(),
            Resource::Socket(commands) => {
                let _ = commands.send(SocketCommand::Close);
            }
        }
    }
}

enum SocketCommand {
    Send(String),
    Close,
}

/// Records one guest description into the runtime's scratch arena and returns
/// the root's id.
pub(super) fn record(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    description: &Description,
) -> Result<SpecId> {
    let ids = record_nodes(runtime, guest, &description.nodes)?;
    ids.get(description.root as usize)
        .copied()
        .ok_or_else(|| anyhow!("the description has no root"))
}

/// Records nodes into the runtime's scratch arena and returns each one's id.
pub(super) fn record_nodes(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    nodes: &[abi::Node],
) -> Result<Vec<SpecId>> {
    // Nodes arrive after everything they refer to, so each one is finished
    // (constructed, its calls recorded, its children attached) before the
    // next, exactly the order a script's builder chain records in.
    let mut ids: Vec<SpecId> = Vec::with_capacity(nodes.len());

    for node in nodes {
        let id = match &node.component {
            abi::Component::Registered { export, args } => {
                registered_node(runtime, guest, &ids, export, args)?
            }
            abi::Component::Nested(part) => nested_view(runtime, guest, *part)?,
            abi::Component::Dock(handle) => runtime
                .arena_mut()
                .push_dock_area(EntityHandle::from(*handle))
                .map_err(|error| anyhow!("{error}"))?,
            abi::Component::List {
                id,
                count,
                render_items,
                get_key,
            } => {
                // The renderer and key function go into the open callback
                // generation, exactly where a script's `list` puts its own.
                let render_items = runtime.register(guest, *render_items);
                let get_key = runtime.register(guest, *get_key);
                runtime
                    .arena_mut()
                    .push(Component::List(Rc::new(crate::spec::ListSpec::new(
                        id.clone(),
                        crate::spec::ListKind::Uniform,
                        *count as usize,
                        get_key,
                        render_items,
                    ))))
            }
            other => runtime.arena_mut().push(component(other)),
        };
        ids.push(id);

        for op in &node.ops {
            let op = match op {
                Op::Style(name) => SpecOp::NullaryStyle(
                    crate::style::nullary_index(name)
                        .ok_or_else(|| anyhow!("unknown style `{name}`"))?,
                ),
                Op::ParamStyle(name, args) => param_style(name, args)?,
                Op::Method(name, args) => SpecOp::Method(intern(name), bridged(args)),
                Op::Callback(name, callback) => {
                    SpecOp::Callback(handler_name(name)?, runtime.register(guest, *callback))
                }
                Op::Slot(name, index) => {
                    let element = *ids
                        .get(*index as usize)
                        .ok_or_else(|| anyhow!("slot element {index} comes after its user"))?;
                    let name = slot_name(name)?;
                    runtime
                        .arena_mut()
                        .claim(element)
                        .map_err(|error| anyhow!("{error}"))?;
                    SpecOp::Slot(name, element)
                }
                Op::StateStyle(name, index) => {
                    let name = state_style_name(name)?;
                    let styles = *ids
                        .get(*index as usize)
                        .ok_or_else(|| anyhow!("state style node {index} comes after its user"))?;
                    // Claimed: the declarations feed the style, never the tree.
                    runtime
                        .arena_mut()
                        .claim(styles)
                        .map_err(|error| anyhow!("{error}"))?;
                    SpecOp::StateStyle(name, styles)
                }
                Op::Component(name, args) => {
                    registered_method(runtime, guest, &ids, id, name, args)?
                }
            };
            runtime
                .arena_mut()
                .push_op(id, op)
                .map_err(|error| anyhow!("{error}"))?;
        }
        for &child in &node.children {
            let child = *ids
                .get(child as usize)
                .ok_or_else(|| anyhow!("child index {child} is out of range"))?;
            runtime
                .arena_mut()
                .attach(id, child)
                .map_err(|error| anyhow!("{error}"))?;
        }
    }

    Ok(ids)
}

/// A registered component's node, its constructor arguments checked against
/// the catalog's schemas. `ids` resolves element arguments by index.
fn registered_node(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    export: &str,
    args: &[abi::ComponentArg],
) -> Result<SpecId> {
    let (component_id, name, constructor) = runtime
        .components
        .registered()
        .find_map(|(component_id, descriptor)| {
            descriptor
                .constructors()
                .iter()
                .find(|constructor| constructor.export() == export)
                .map(|constructor| (component_id, descriptor.name(), constructor.clone()))
        })
        .ok_or_else(|| anyhow!("no registered component exports `{export}`"))?;
    let target = ArgumentTarget {
        api: export,
        descriptors: constructor.arguments(),
    };
    let arguments = component_arguments(runtime, guest, ids, target, args)?;
    let payload = constructor
        .payload(&arguments)
        .map_err(|error| anyhow!("{export}(...): {error}"))?;
    Ok(runtime.arena_mut().push(Component::Registered(
        crate::spec::RegisteredComponentSpec::new(component_id, name, payload),
    )))
}

/// A method call on the registered component node `id`.
fn registered_method(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    id: SpecId,
    name: &str,
    args: &[abi::ComponentArg],
) -> Result<SpecOp> {
    let method = registered_method_descriptor(runtime, id, name)
        .ok_or_else(|| anyhow!("the component has no method `{name}`"))?;
    let target = ArgumentTarget {
        api: name,
        descriptors: method.arguments(),
    };
    let arguments = component_arguments(runtime, guest, ids, target, args)?;
    let payload = method
        .record(&arguments)
        .map_err(|error| anyhow!("{name}(...): {error}"))?;
    Ok(SpecOp::RegisteredMethod(
        crate::RecordedComponentMethod::new(method.name(), payload),
    ))
}

/// A style with arguments. A script reads a color as
/// `cx.theme().colors.primary`; a guest names the token instead, resolved
/// here against the theme the build synced.
fn param_style(name: &str, args: &[Arg]) -> Result<SpecOp> {
    let name =
        crate::style::param_style_name(name).ok_or_else(|| anyhow!("unknown style `{name}`"))?;
    let mut args = bridged(args);
    if COLOR_STYLES.contains(&name) {
        for arg in &mut args {
            if let Bridged::Str(value) = arg
                && !value.starts_with('#')
            {
                let color = crate::theme_tokens::token_color(value)
                    .ok_or_else(|| anyhow!("unknown color token `{value}`"))?;
                *value = hex(color);
            }
        }
    }
    Ok(SpecOp::ParamStyle(name, args))
}

fn handler_name(name: &str) -> Result<&'static str> {
    Ok(match name {
        "on_click" => "on_click",
        "on_change" => "on_change",
        other => anyhow::bail!("unsupported handler `{other}`"),
    })
}

fn state_style_name(name: &str) -> Result<&'static str> {
    Ok(match name {
        "hover" => "hover",
        "active" => "active",
        "focus" => "focus",
        other => anyhow::bail!("unsupported state style `{other}`"),
    })
}

/// The element slots a component takes, by the names the script API uses.
fn slot_name(name: &str) -> Result<&'static str> {
    Ok(match name {
        "content" => "content",
        "image" => "image",
        "fallback" => "fallback",
        "header" => "header",
        "footer" => "footer",
        "panel" => "panel",
        "trigger" => "trigger",
        "input" => "input",
        "decrement_button" => "decrement_button",
        "increment_button" => "increment_button",
        other => anyhow::bail!("unknown element slot `{other}`"),
    })
}

/// The method `method` of the registered component node `id`, if it has one.
fn registered_method_descriptor(
    runtime: &ShellRuntime,
    id: SpecId,
    method: &str,
) -> Option<crate::MethodDescriptor> {
    let component_id = {
        let arena = runtime.arena.borrow();
        let Component::Registered(component) = arena.node(id)?.component()? else {
            return None;
        };
        component.id()
    };
    runtime
        .components
        .descriptor(component_id)?
        .methods()
        .iter()
        .find(|descriptor| descriptor.name() == method)
        .cloned()
}

/// How a schema reads in an error message.
fn schema_name(schema: &crate::ArgumentSchema) -> String {
    use crate::ArgumentSchema;
    match schema {
        ArgumentSchema::String => "a string".into(),
        ArgumentSchema::Number => "a finite number".into(),
        ArgumentSchema::Boolean => "a boolean".into(),
        ArgumentSchema::Element => "an Element".into(),
        ArgumentSchema::Entity(kind) => format!("a {kind} entity"),
        ArgumentSchema::Callback(_) => "a function".into(),
        ArgumentSchema::Enum(values) => values
            .iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", "),
        ArgumentSchema::Array(item) => format!("an array of {}", schema_name(item)),
        ArgumentSchema::Optional(item) => format!("an optional {}", schema_name(item)),
    }
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
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    target: ArgumentTarget<'_>,
    args: &[abi::ComponentArg],
) -> Result<Vec<crate::ComponentArgument>> {
    use crate::{ArgumentSchema, ComponentArgument as Argument};
    let ArgumentTarget { api, descriptors } = target;
    anyhow::ensure!(
        args.len() <= descriptors.len(),
        "{api}(...) expects at most {} arguments",
        descriptors.len()
    );
    let convert = |name: &str, schema: &ArgumentSchema, arg: &abi::ComponentArg| {
        component_argument(runtime, guest, ids, api, name, schema, arg)
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
                anyhow!(
                    "{api}({}) expects {}",
                    descriptor.name(),
                    schema_name(descriptor.schema())
                )
            })?;
            convert(descriptor.name(), descriptor.schema(), arg)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn component_argument(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    ids: &[SpecId],
    api: &str,
    name: &str,
    schema: &crate::ArgumentSchema,
    arg: &abi::ComponentArg,
) -> Result<crate::ComponentArgument> {
    use crate::{ArgumentSchema as Schema, ComponentArgument as Argument};
    use abi::ComponentArg as Guest_;
    let recurse = |schema: &Schema, arg: &Guest_| {
        component_argument(runtime, guest, ids, api, name, schema, arg)
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
                .ok_or_else(|| anyhow!("element argument {index} comes after its user"))?;
            runtime
                .arena_mut()
                .claim(element)
                .map_err(|error| anyhow!("{error}"))?;
            Some(Argument::Element(element))
        }
        (Schema::Callback(_), Guest_::Callback(callback)) => {
            Some(Argument::Callback(runtime.register(guest, *callback)))
        }
        (Schema::Array(item), Guest_::Array(values)) => Some(Argument::Array(
            values
                .iter()
                .map(|value| recurse(item, value))
                .collect::<Result<_>>()?,
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
                .map_err(|_| anyhow!("retained component state is already borrowed"))?
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
    converted.ok_or_else(|| anyhow!("{api}({name}) expects {}", schema_name(schema)))
}

/// The style methods whose string argument is a color.
const COLOR_STYLES: &[&str] = &["bg", "text_color", "text_bg", "border_color"];

/// A theme color as the string a script's `cx.theme().colors` hands out: six
/// digits for an opaque color, so the two engines describe the same tree, and
/// eight for a translucent one, whose alpha a script's form would drop.
fn hex(color: gpui::Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    let rgb = format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    );
    match channel(rgba.a) {
        255 => rgb,
        alpha => format!("{rgb}{alpha:02x}"),
    }
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
        abi::Component::Registered { .. }
        | abi::Component::Nested(_)
        | abi::Component::Dock(_)
        | abi::Component::List { .. } => unreachable!("recorded by `record_nodes`"),
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

/// What a script's `cx.notify()` does for its own view (invalidate the
/// description and let GPUI schedule the frame) for the views showing the
/// guest's `parts`, or all of them: a description guest's parts all read the
/// same state, while a retained guest's batch names the parts it touched.
fn refresh_guest(guest: &Guest, parts: Option<&BTreeSet<u32>>) {
    let views: Vec<_> = guest
        .views
        .borrow()
        .iter()
        .filter(|(part, _)| parts.is_none_or(|parts| parts.contains(part)))
        .filter_map(|(_, view)| view.upgrade())
        .collect();
    if parts.is_some() {
        scope::with_current_app(|app| {
            for view in views {
                view.update(app, |view, cx| view.refresh(cx));
            }
        });
        return;
    }
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

/// A view showing one of the guest's parts, made under the current host
/// call. `nested_in` is the view it is drawn inside, for a child view; a
/// dialog's, sheet's or panel's view is a root of its own.
fn part_view(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    part: u32,
    nested_in: Option<gpui::EntityId>,
) -> Result<Entity<ScriptView>> {
    scope::with_current_app(|cx| runtime.part_view(guest, part, nested_in, cx))
        .ok_or_else(|| anyhow!("a part view needs a live host call"))
}

/// Records a part drawn in place: a child view, made once per part and kept,
/// so the part keeps its own invalidation across the parent's renders.
fn nested_view(runtime: &Rc<ShellRuntime>, guest: &Rc<Guest>, part: u32) -> Result<SpecId> {
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
                .ok_or_else(|| anyhow!("a nested part needs a view to be drawn in"))?;
            let view = part_view(runtime, guest, part, Some(parent))?;
            let handle = runtime
                .entities()
                .create_view(
                    view.clone(),
                    scope::current_application_generation(),
                    runtime,
                )
                .map_err(|_| anyhow!("too many retained entities"))?;
            guest.nested.borrow_mut().insert(part, handle);
            (handle, view)
        }
    };
    runtime
        .arena_mut()
        .push_child_view(crate::spec::ChildViewSpec::new(handle, view))
        .map_err(|error| anyhow!("{error}"))
}

/// Opens what the guest asked for while its handler ran: dock panels, layouts
/// to restore, sheets and dialogs. Each body is a view, made here under the
/// host call that is still open.
fn open_pending(runtime: &Rc<ShellRuntime>, guest: &Rc<Guest>) -> Result<()> {
    let panels = std::mem::take(&mut *guest.pending_panels.borrow_mut());
    for panel in panels {
        let view = part_view(runtime, guest, panel.part, None)?;
        let placement = match panel.placement.as_str() {
            "left" => gpui_base::dock::DockPlacement::Left,
            "right" => gpui_base::dock::DockPlacement::Right,
            "bottom" => gpui_base::dock::DockPlacement::Bottom,
            "center" => gpui_base::dock::DockPlacement::Center,
            other => anyhow::bail!(
                "`{other}` is not a dock placement; use left, right, bottom or center"
            ),
        };
        let area = runtime
            .entities()
            .dock(panel.dock)
            .ok_or_else(|| anyhow!("the dock area was released before its panel was added"))?;
        // Registered under its name, so a restored layout rebuilds the panel
        // from the same part.
        let script: Rc<dyn crate::dock::PanelScript> = Rc::new(GuestPanel {
            runtime: Rc::downgrade(runtime),
            guest: Rc::downgrade(guest),
            part: panel.part,
            policy: scope::policy(),
        });
        scope::with_current(move |window, cx| {
            let name = crate::dock::register_panel("wasm-guest", &panel.name, script.clone(), cx);
            let panel =
                cx.new(|cx| crate::dock::ScriptPanel::new(name, view, cx).with_script(script));
            area.update(cx, |area, cx| {
                area.add_panel(panel, placement, None, window, cx)
            });
        })
        .ok_or_else(|| anyhow!("adding a panel needs a live host call"))?;
    }

    // After the panels, so a layout restored in the same call finds them
    // registered. Deferred to the end of the event: restoring rebuilds every
    // panel, which is not something to do from inside one of them.
    let loads = std::mem::take(&mut *guest.pending_loads.borrow_mut());
    for (dock, state) in loads {
        let area = runtime
            .entities()
            .dock(dock)
            .ok_or_else(|| anyhow!("the dock area was released before its layout was loaded"))?;
        scope::with_current(move |window, cx| {
            window.defer(cx, move |window, cx| {
                if let Err(error) = area.update(cx, |area, cx| area.load(state, window, cx)) {
                    tracing::error!("a guest's dock layout could not be restored: {error}");
                }
            });
        })
        .ok_or_else(|| anyhow!("Dock::load needs a live host call"))?;
    }

    let sheets = std::mem::take(&mut *guest.pending_sheets.borrow_mut());
    for (part, placement) in sheets {
        let placement = match placement.as_str() {
            "left" => gpui_base::Placement::Left,
            "right" => gpui_base::Placement::Right,
            "top" => gpui_base::Placement::Top,
            "bottom" => gpui_base::Placement::Bottom,
            other => {
                anyhow::bail!("`{other}` is not a sheet placement; use left, right, top or bottom")
            }
        };
        let view = part_view(runtime, guest, part, None)?;
        scope::with_current(|window, app| {
            crate::ShellRoot::update(window, app, |root, window, cx| {
                root.open_sheet(placement, view.into(), window, cx)
            })
        })
        .flatten()
        .ok_or_else(|| anyhow!("open_sheet found no ShellRoot in this window"))?;
    }

    let dialogs = std::mem::take(&mut *guest.pending_dialogs.borrow_mut());
    for part in dialogs {
        let view = part_view(runtime, guest, part, None)?;
        scope::with_current(|window, app| {
            crate::ShellRoot::update(window, app, |root, window, cx| {
                root.open_dialog_with(view.into(), crate::DialogOptions::default(), window, cx)
            })
        })
        .flatten()
        .ok_or_else(|| {
            anyhow!("open_dialog found no ShellRoot in this window; open dialogs from a handler")
        })?;
    }
    Ok(())
}

/// Runs the handler `target` names with `payload`, inside the event scope
/// the caller opened: refreshes the views it changed, then opens whatever it
/// asked for.
pub(super) fn handle(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    target: &HandlerTarget,
    payload: &Json,
) -> Result<()> {
    let payload = payload.to_string();
    match (&guest.protocol, target) {
        (Protocol::Describe { .. }, HandlerTarget::Callback(handler)) => {
            if guest.dispatch(*handler, payload.as_bytes())? {
                refresh_guest(guest, None);
            }
        }
        (Protocol::Retained { .. }, HandlerTarget::Node(id, event)) => {
            guest.retained_event(retained_target::NODE, *id, event, payload.as_bytes())?;
        }
        (Protocol::Retained { .. }, HandlerTarget::Callback(token)) => {
            guest.retained_event(
                retained_target::HANDLER,
                u64::from(*token),
                "",
                payload.as_bytes(),
            )?;
        }
        (Protocol::Describe { .. }, HandlerTarget::Node(..)) => {
            anyhow::bail!("a node handler for a guest that describes its view")
        }
    }
    open_pending(runtime, guest)
}

/// Builds `part`'s description into the runtime's scratch arena: by running
/// a description guest's render, or by copying a retained guest's tree.
pub(super) fn describe(runtime: &Rc<ShellRuntime>, guest: &Rc<Guest>, part: u32) -> Result<SpecId> {
    match &guest.protocol {
        Protocol::Describe { .. } => {
            let description = guest.render(part)?;
            record(runtime, guest, &description)
        }
        Protocol::Retained { tree, .. } => retained::record(runtime, guest, &tree.borrow(), part),
    }
}

/// Runs the guest's `init` under the scope of its root view, when it has one
/// (a test may describe a guest that is drawn by nothing).
pub(super) fn initialize(
    runtime: &Rc<ShellRuntime>,
    guest: &Rc<Guest>,
    view: Option<&Entity<ScriptView>>,
) -> Result<()> {
    if let Some(view) = view {
        guest.store.borrow_mut().data_mut().view = Some(view.downgrade());
        guest.add_view(view, 0);
    }
    match &guest.protocol {
        Protocol::Describe { init, .. } => init
            .call(&mut *guest.store.borrow_mut(), ())
            .map_err(|error| anyhow!("guest init failed: {error:#}"))?,
        Protocol::Retained { init, tree, .. } => {
            let packed = init
                .call(&mut *guest.store.borrow_mut(), ())
                .map_err(|error| anyhow!("guest init failed: {error:#}"))?;
            let batch = guest.batch(packed)?;
            tree.borrow_mut().apply(batch)?;
        }
    }
    // A dialog or panel the guest opened from `init`.
    open_pending(runtime, guest)
}

/// How a guest's panel is rebuilt when a saved layout is restored: the same
/// part, drawn by a fresh view. A guest that is gone builds nothing, and the
/// panel's place in the layout is carried forward.
struct GuestPanel {
    runtime: Weak<ShellRuntime>,
    guest: Weak<Guest>,
    part: u32,
    policy: Rc<crate::policy::Policy>,
}

impl crate::dock::PanelScript for GuestPanel {
    fn build(&self, window: &mut Window, cx: &mut App) -> Option<Entity<ScriptView>> {
        let runtime = self.runtime.upgrade()?;
        let guest = self.guest.upgrade()?;
        let (_scope, _) = scope::enter_with_application(
            &runtime,
            window,
            cx,
            ScopePhase::Event,
            None,
            self.policy.clone(),
            guest.application(),
        );
        part_view(&runtime, &guest, self.part, None)
            .map_err(|error| tracing::error!("a guest panel could not be rebuilt: {error:#}"))
            .ok()
    }
}

/// The chrome a guest's dock wears: a tab strip per group, one tab per panel
/// titled with its name, a click showing that panel. Drawn by the host, so a
/// guest gets working tabs without describing any.
struct GuestTabs;

impl crate::dock::DockChrome for GuestTabs {
    fn tab_bar(
        &self,
        group: &gpui_base::dock::TabGroupContext,
        _: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        use gpui::{
            InteractiveElement as _, IntoElement as _, ParentElement as _,
            StatefulInteractiveElement as _, Styled as _, div,
        };
        use gpui_component::ActiveTheme as _;

        let theme = cx.theme();
        let (bar, active, foreground, active_foreground, border) = (
            theme.tab_bar,
            theme.tab_active,
            theme.tab_foreground,
            theme.tab_active_foreground,
            theme.border,
        );
        let tabs: Vec<_> = group
            .panels()
            .iter()
            .enumerate()
            .filter(|(_, panel)| panel.visible(cx))
            .map(|(ix, panel)| {
                let name = panel.panel_name(cx);
                let title = name.rsplit('/').next().unwrap_or(name).to_owned();
                let selected = ix == group.active_ix();
                let group = group.clone();
                div()
                    .id(("guest-tab", ix))
                    .px_2()
                    .py_1()
                    .text_sm()
                    .bg(if selected { active } else { bar })
                    .text_color(if selected {
                        active_foreground
                    } else {
                        foreground
                    })
                    .debug_selector(|| format!("guest-tab-{title}"))
                    .child(title)
                    .on_click(move |_, window, cx| group.select_tab(ix, window, cx))
            })
            .collect();
        div()
            .id("guest-tab-bar")
            .flex()
            .flex_row()
            .bg(bar)
            .border_b_1()
            .border_color(border)
            .children(tabs)
            .into_any_element()
    }
}

/// A panel a guest asked to dock.
struct PendingPanel {
    dock: EntityHandle,
    part: u32,
    name: String,
    placement: String,
}

/// Which protocol a guest module speaks, by the exports it has.
enum Protocol {
    /// The guest re-describes a part on every render.
    Describe {
        init: TypedFunc<(), ()>,
        render: TypedFunc<u32, u64>,
        render_items: TypedFunc<(u32, u32, u32), u64>,
        item_key: TypedFunc<(u32, u32), u64>,
        dispatch: TypedFunc<(u32, u32, u32), u32>,
    },
    /// The guest sends operations on a tree the host keeps.
    Retained {
        init: TypedFunc<(), u64>,
        event: TypedFunc<(u32, u64, u32, u32, u32, u32), u64>,
        tree: RefCell<retained::Tree>,
    },
}

/// One instantiated guest module.
pub(crate) struct Guest {
    store: RefCell<Store<HostState>>,
    memory: Memory,
    protocol: Protocol,
    alloc: TypedFunc<u32, u32>,
    /// The application generation the guest belongs to; its release stops
    /// what the guest started.
    application: Rc<ApplicationGeneration>,
    /// Every view showing the guest (its root and its parts), with the part
    /// each shows.
    views: RefCell<Vec<(u32, WeakEntity<ScriptView>)>>,
    /// The child view each nested part is drawn through, by part.
    nested: RefCell<HashMap<u32, EntityHandle>>,
    /// Parts the guest asked to open as dialogs, not yet opened.
    pending_dialogs: RefCell<Vec<u32>>,
    /// Panels the guest asked to dock, not yet added.
    pending_panels: RefCell<Vec<PendingPanel>>,
    /// Parts the guest asked to open as sheets, with their placement.
    pending_sheets: RefCell<Vec<(u32, String)>>,
    /// The dock areas the guest created.
    docks: RefCell<Vec<EntityHandle>>,
    /// Layouts the guest asked to restore, by dock, not yet applied.
    pending_loads: RefCell<Vec<(EntityHandle, gpui_base::dock::DockAreaState)>>,
    /// Processes and sockets the guest started, by the id it was handed.
    resources: RefCell<Vec<Option<Resource>>>,
}

struct HostState {
    runtime: Weak<ShellRuntime>,
    guest: Weak<Guest>,
    /// The guest's root view, so a handler that runs from a native
    /// subscription (no host call of its own) can open a scope for it. Weak:
    /// the view owns the guest's lifetime, not the other way round.
    view: Option<WeakEntity<ScriptView>>,
}

impl Guest {
    /// Instantiates `module` for `application` without running its `init`,
    /// which [`initialize`] runs once the guest's view exists.
    pub(super) fn new(
        runtime: &Rc<ShellRuntime>,
        module: &Module,
        application: Rc<ApplicationGeneration>,
    ) -> Result<Rc<Self>> {
        let engine = module.engine();
        let mut linker = Linker::new(engine);
        link_imports(&mut linker)?;

        let mut store = Store::new(
            engine,
            HostState {
                runtime: Rc::downgrade(runtime),
                guest: Weak::new(),
                view: None,
            },
        );
        let instance: Instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("the guest exports no memory"))?;
        let protocol = if instance
            .get_export(&mut store, abi::retained::export::INIT)
            .is_some()
        {
            Protocol::Retained {
                init: instance.get_typed_func(&mut store, abi::retained::export::INIT)?,
                event: instance.get_typed_func(&mut store, abi::retained::export::EVENT)?,
                tree: RefCell::new(retained::Tree::default()),
            }
        } else {
            Protocol::Describe {
                init: instance.get_typed_func(&mut store, abi::export::INIT)?,
                render: instance.get_typed_func(&mut store, abi::export::RENDER)?,
                render_items: instance.get_typed_func(&mut store, abi::export::RENDER_ITEMS)?,
                item_key: instance.get_typed_func(&mut store, abi::export::ITEM_KEY)?,
                dispatch: instance.get_typed_func(&mut store, abi::export::DISPATCH)?,
            }
        };
        let guest = Rc::new(Guest {
            protocol,
            alloc: instance.get_typed_func(&mut store, abi::export::ALLOC)?,
            store: RefCell::new(store),
            memory,
            application,
            views: RefCell::new(Vec::new()),
            nested: RefCell::new(HashMap::new()),
            pending_dialogs: RefCell::new(Vec::new()),
            pending_panels: RefCell::new(Vec::new()),
            pending_sheets: RefCell::new(Vec::new()),
            docks: RefCell::new(Vec::new()),
            pending_loads: RefCell::new(Vec::new()),
            resources: RefCell::new(Vec::new()),
        });
        guest.store.borrow_mut().data_mut().guest = Rc::downgrade(&guest);
        runtime.guests.borrow_mut().push(guest.clone());
        Ok(guest)
    }

    pub(super) fn application(&self) -> Option<Rc<ApplicationGeneration>> {
        Some(self.application.clone())
    }

    pub(super) fn add_view(&self, view: &Entity<ScriptView>, part: u32) {
        self.views.borrow_mut().push((part, view.downgrade()));
    }

    /// Reads a batch a retained guest returned.
    fn batch(&self, packed: u64) -> Result<abi::retained::Batch> {
        let bytes = read(&self.memory, &*self.store.borrow(), packed)?;
        postcard::from_bytes(&bytes).context("guest returned a malformed batch")
    }

    /// Copies `bytes` into a buffer the guest allocated; the guest owns it.
    fn give(&self, bytes: &[u8]) -> Result<(u32, u32)> {
        let mut store = self.store.borrow_mut();
        let len = bytes.len() as u32;
        let ptr = self.alloc.call(&mut *store, len)?;
        self.memory.write(&mut *store, ptr as usize, bytes)?;
        Ok((ptr, len))
    }

    /// Runs a retained guest's handler and applies the batch it returns,
    /// refreshing the parts it touched.
    fn retained_event(&self, kind: u32, target: u64, name: &str, payload: &[u8]) -> Result<()> {
        let Protocol::Retained { event, tree, .. } = &self.protocol else {
            anyhow::bail!("not a retained guest");
        };
        let (name_ptr, name_len) = self.give(name.as_bytes())?;
        let (payload_ptr, payload_len) = self.give(payload)?;
        let packed = event
            .call(
                &mut *self.store.borrow_mut(),
                (kind, target, name_ptr, name_len, payload_ptr, payload_len),
            )
            .map_err(|error| anyhow!("guest handler failed: {error:#}"))?;
        let batch = self.batch(packed)?;
        let parts = tree.borrow_mut().apply(batch)?;
        refresh_guest(self, Some(&parts));
        Ok(())
    }

    /// Stops every process and socket the guest started.
    pub(super) fn stop_resources(&self) {
        let resources: Vec<_> = self.resources.borrow_mut().drain(..).flatten().collect();
        for resource in resources {
            resource.stop();
        }
    }

    fn add_resource(&self, resource: Resource) -> u32 {
        let mut resources = self.resources.borrow_mut();
        resources.push(Some(resource));
        resources.len() as u32 - 1
    }

    /// Renders the guest's view (`part` 0) or one of its parts.
    pub(super) fn render(&self, part: u32) -> Result<Description> {
        let Protocol::Describe { render, .. } = &self.protocol else {
            anyhow::bail!("a retained guest has no render");
        };
        let mut store = self.store.borrow_mut();
        let packed = render
            .call(&mut *store, part)
            .map_err(|error| anyhow!("guest render failed: {error:#}"))?;
        let bytes = read(&self.memory, &*store, packed)?;
        postcard::from_bytes(&bytes).context("guest returned a malformed description")
    }

    /// Describes items `start..end` of one of the guest's lists.
    pub(super) fn render_items(&self, renderer: u32, start: u32, end: u32) -> Result<abi::Items> {
        let Protocol::Describe { render_items, .. } = &self.protocol else {
            anyhow::bail!("a retained guest has no item renderer");
        };
        let mut store = self.store.borrow_mut();
        let packed = render_items
            .call(&mut *store, (renderer, start, end))
            .map_err(|error| anyhow!("guest item renderer failed: {error:#}"))?;
        let bytes = read(&self.memory, &*store, packed)?;
        postcard::from_bytes(&bytes).context("guest returned malformed items")
    }

    /// The key of one of the guest's list items.
    pub(super) fn item_key(&self, get_key: u32, index: u32) -> Result<String> {
        let Protocol::Describe { item_key, .. } = &self.protocol else {
            anyhow::bail!("a retained guest has no key function");
        };
        let mut store = self.store.borrow_mut();
        let packed = item_key
            .call(&mut *store, (get_key, index))
            .map_err(|error| anyhow!("guest item key failed: {error:#}"))?;
        Ok(String::from_utf8(read(&self.memory, &*store, packed)?)?)
    }

    /// Runs guest handler `callback`. Returns whether it asked to re-render.
    fn dispatch(&self, callback: u32, payload: &[u8]) -> Result<bool> {
        let Protocol::Describe { dispatch, .. } = &self.protocol else {
            anyhow::bail!("a retained guest has no dispatch");
        };
        let (ptr, len) = self.give(payload)?;
        let mut store = self.store.borrow_mut();
        let notify = dispatch
            .call(&mut *store, (callback, ptr, len))
            .map_err(|error| anyhow!("guest handler failed: {error:#}"))?;
        Ok(notify != 0)
    }

    /// A handler that fires from a native subscription rather than from a
    /// rendered callback: open the event scope a rendered one would have had.
    fn dispatch_native(self: &Rc<Self>, callback: u32, window: &mut Window, app: &mut App) {
        self.dispatch_native_with(callback, &Json::Null, window, app);
    }

    /// [`Self::dispatch_native`] with a JSON payload for the handler.
    fn dispatch_native_with(
        self: &Rc<Self>,
        callback: u32,
        payload: &Json,
        window: &mut Window,
        app: &mut App,
    ) {
        if !self.application.is_active() {
            return;
        }
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
        let policy = view
            .as_ref()
            .map(|view| view.read(app).policy())
            .unwrap_or_else(crate::policy::default);
        let (_guard, _) = scope::enter_with_application(
            &runtime,
            window,
            app,
            ScopePhase::Event,
            view,
            policy,
            self.application(),
        );
        if let Err(error) = handle(&runtime, self, &HandlerTarget::Callback(callback), payload) {
            tracing::error!("wasm guest handler failed: {error:#}");
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
                    schema_name(descriptor.schema())
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
        own_task(|task| {
            window.spawn(cx, async move |cx| {
                let payload = cx.background_executor().spawn(async move { work() }).await;
                let _ = cx.update(|window, app| {
                    if let Some(guest) = guest.upgrade() {
                        guest.dispatch_native_with(callback, &payload, window, app);
                    }
                    finish_task(task);
                });
            })
        });
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

/// Hands each event from `events` to the guest's handler `callback` on the
/// main thread, until `to_json` marks one as the last or the guest is gone:
/// the shape of every stream a guest starts (a process's output, a socket's
/// messages), since a core module has no async iterator to drive.
fn stream_to_guest<T: 'static>(
    guest: Weak<Guest>,
    callback: u32,
    events: async_channel::Receiver<T>,
    to_json: impl Fn(T) -> (serde_json::Value, bool) + 'static,
) -> Result<()> {
    // Polled rather than awaited, as a script's streams are: the producer is
    // an OS thread, and a wake from one is not a foreground schedule GPUI's
    // deterministic test executor accepts.
    const STREAM_POLL: std::time::Duration = std::time::Duration::from_millis(16);
    scope::with_current(|window, cx| {
        own_task(|task| {
            window.spawn(cx, async move |cx| {
                loop {
                    let event = match events.try_recv() {
                        Ok(event) => event,
                        Err(async_channel::TryRecvError::Empty) => {
                            cx.background_executor().timer(STREAM_POLL).await;
                            continue;
                        }
                        Err(async_channel::TryRecvError::Closed) => break,
                    };
                    let (payload, last) = to_json(event);
                    let delivered = cx.update(|window, app| match guest.upgrade() {
                        Some(guest) => {
                            guest.dispatch_native_with(callback, &payload, window, app);
                            true
                        }
                        None => false,
                    });
                    if last || !matches!(delivered, Ok(true)) {
                        break;
                    }
                }
                let _ = cx.update(|_, _| finish_task(task));
            })
        });
    })
    .ok_or_else(|| anyhow!("a stream needs a live host call"))
}

/// A socket's owner: one thread that interleaves reads (on the read slice the
/// handshake set) with the guest's sends, until either side closes.
fn run_guest_socket(
    url: String,
    host_name: String,
    port: u16,
    commands: std::sync::mpsc::Receiver<SocketCommand>,
    events: async_channel::Sender<serde_json::Value>,
) {
    use tungstenite::Message;
    let mut socket = match crate::net::websocket::guest_handshake(url, host_name, port) {
        Ok(socket) => socket,
        Err(error) => {
            let _ = events.send_blocking(serde_json::json!({ "error": error }));
            return;
        }
    };
    if events
        .send_blocking(serde_json::json!({ "open": true }))
        .is_err()
    {
        return;
    }
    loop {
        match commands.try_recv() {
            Ok(SocketCommand::Send(text)) => {
                if let Err(error) = socket.send(Message::text(text)) {
                    let _ = events.send_blocking(serde_json::json!({ "error": error.to_string() }));
                    return;
                }
            }
            Ok(SocketCommand::Close) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let _ = socket.close(None);
                let _ = socket.flush();
                let _ = events.send_blocking(serde_json::json!({ "close": true }));
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if events
                    .send_blocking(serde_json::json!({ "message": text.as_str() }))
                    .is_err()
                {
                    return;
                }
            }
            Ok(Message::Close(_)) => {
                let _ = events.send_blocking(serde_json::json!({ "close": true }));
                return;
            }
            // A binary message reaches the guest as its bytes.
            Ok(Message::Binary(bytes)) => {
                if events
                    .send_blocking(serde_json::json!({ "binary": bytes.to_vec() }))
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                let _ = events.send_blocking(serde_json::json!({ "close": true }));
                return;
            }
            Err(error) => {
                let _ = events.send_blocking(serde_json::json!({ "error": error.to_string() }));
                return;
            }
        }
    }
}

/// A process a guest streams, as JSON.
#[derive(serde::Deserialize)]
struct GuestSpawn {
    command: String,
    #[serde(default)]
    args: Vec<String>,
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
            let request = crate::net::http::authorize(&capabilities, "GET", &url, &[], Vec::new())
                .map_err(|error| anyhow!(error))?;
            run_in_background(caller.data().guest.clone(), callback, move || {
                http_result(crate::net::http::send(capabilities, request))
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
            let request = crate::net::http::authorize(
                &capabilities,
                &request.method,
                &request.url,
                &request.headers,
                request.body.into_bytes(),
            )
            .map_err(|error| anyhow!(error))?;
            run_in_background(caller.data().guest.clone(), callback, move || {
                http_result(crate::net::http::send(capabilities, request))
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
    // Streams: a process's output line by line, and a WebSocket's messages.
    // Each hands every event to the same guest handler until it ends.
    linker.func_wrap(
        module,
        abi::import::PROCESS_SPAWN,
        |mut caller: Caller<'_, HostState>,
         request_ptr: u32,
         request_len: u32,
         callback: u32|
         -> Result<u32> {
            let request: GuestSpawn =
                serde_json::from_slice(&read_bytes(&mut caller, request_ptr, request_len)?)?;
            anyhow::ensure!(
                scope::policy().capabilities().may_run(&request.command),
                "{}",
                crate::capability::CapabilityError::ExecuteDenied(request.command.clone())
            );
            let spawned = crate::process::spawn_streaming(
                &request.command,
                &request.args,
                crate::process::SpawnOptions {
                    cwd: None,
                    stdin: true,
                    timeout: None,
                    lines: true,
                },
            )
            .map_err(|error| anyhow!(error))?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            let id = guest.add_resource(Resource::Process {
                killer: spawned.killer.clone(),
                stdin: spawned.stdin,
            });
            stream_to_guest(Rc::downgrade(&guest), callback, spawned.events, |event| {
                use crate::process::ProcessEvent;
                match event {
                    ProcessEvent::Stdout(line) => (serde_json::json!({ "stdout": line }), false),
                    ProcessEvent::Stderr(line) => (serde_json::json!({ "stderr": line }), false),
                    ProcessEvent::Exit {
                        code,
                        signal,
                        error,
                    } => (
                        serde_json::json!({ "exit": { "code": code, "signal": signal, "error": error } }),
                        true,
                    ),
                }
            })?;
            Ok(id)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::RESOURCE_STOP,
        |caller: Caller<'_, HostState>, id: u32| -> Result<()> {
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            let resource = guest
                .resources
                .borrow_mut()
                .get_mut(id as usize)
                .and_then(Option::take);
            if let Some(resource) = resource {
                resource.stop();
            }
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::SOCKET_OPEN,
        |mut caller: Caller<'_, HostState>,
         url_ptr: u32,
         url_len: u32,
         callback: u32|
         -> Result<u32> {
            let url = read_string(&mut caller, url_ptr, url_len)?;
            let (url, host_name, port) =
                crate::net::websocket::authorize_guest_socket(scope::policy().capabilities(), &url)
                    .map_err(|error| anyhow!(error))?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            let (commands, received) = std::sync::mpsc::channel();
            let (events, incoming) = async_channel::unbounded();
            std::thread::Builder::new()
                .name("gpui-wasm-guest-socket".to_owned())
                .spawn(move || run_guest_socket(url, host_name, port, received, events))?;
            let id = guest.add_resource(Resource::Socket(commands));
            stream_to_guest(Rc::downgrade(&guest), callback, incoming, |event| {
                let last = event.get("close").is_some() || event.get("error").is_some();
                (event, last)
            })?;
            Ok(id)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::SOCKET_SEND,
        |mut caller: Caller<'_, HostState>, id: u32, text_ptr: u32, text_len: u32| -> Result<()> {
            let text = read_string(&mut caller, text_ptr, text_len)?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            match guest.resources.borrow().get(id as usize) {
                Some(Some(Resource::Socket(commands))) => commands
                    .send(SocketCommand::Send(text))
                    .map_err(|_| anyhow!("socket {id} is closed")),
                _ => Err(anyhow!("no open socket {id}")),
            }
        },
    )?;
    // Docks: an area made from `init` or a handler, and panels whose bodies
    // are the guest's parts.
    linker.func_wrap(
        module,
        abi::import::DOCK_NEW,
        |mut caller: Caller<'_, HostState>, id_ptr: u32, id_len: u32| -> Result<u64> {
            let id = read_string(&mut caller, id_ptr, id_len)?;
            anyhow::ensure!(
                !matches!(
                    scope::current_phase(),
                    Some(ScopePhase::Render | ScopePhase::Layout)
                ),
                "Dock::new cannot run during render; create a dock in init or a handler"
            );
            let runtime = runtime_of(&caller)?;
            let handle = scope::with_current(|window, cx| {
                runtime.entities().create_dock(
                    &id,
                    None,
                    crate::dock::ScriptDockSkin::new(Rc::new(GuestTabs)),
                    scope::current_application_generation(),
                    window,
                    cx,
                )
            })
            .ok_or_else(|| anyhow!("Dock::new needs a live host call"))?
            .map_err(|_| anyhow!("too many retained entities"))?;
            if let Some(guest) = caller.data().guest.upgrade() {
                guest.docks.borrow_mut().push(handle);
            }
            Ok(handle.into())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::DOCK_ADD_PANEL,
        |mut caller: Caller<'_, HostState>,
         dock: u64,
         part: u32,
         name_ptr: u32,
         name_len: u32,
         placement_ptr: u32,
         placement_len: u32|
         -> Result<()> {
            let name = read_string(&mut caller, name_ptr, name_len)?;
            let placement = read_string(&mut caller, placement_ptr, placement_len)?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            guest.pending_panels.borrow_mut().push(PendingPanel {
                dock: EntityHandle::from(dock),
                part,
                name,
                placement,
            });
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::DOCK_DUMP,
        |mut caller: Caller<'_, HostState>, dock: u64| -> Result<u64> {
            let runtime = runtime_of(&caller)?;
            let area = runtime
                .entities()
                .dock(EntityHandle::from(dock))
                .ok_or_else(|| anyhow!("the dock area was released"))?;
            let state = scope::with_current_app(|cx| area.read(cx).dump(cx))
                .ok_or_else(|| anyhow!("Dock::dump needs a live host call"))?;
            let layout = serde_json::to_string(&state)?;
            write_string(&mut caller, &layout)
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::DOCK_LOAD,
        |mut caller: Caller<'_, HostState>,
         dock: u64,
         layout_ptr: u32,
         layout_len: u32|
         -> Result<()> {
            let layout = read_string(&mut caller, layout_ptr, layout_len)?;
            let state: gpui_base::dock::DockAreaState =
                serde_json::from_str(&layout).context("this is not a layout Dock::dump wrote")?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            guest
                .pending_loads
                .borrow_mut()
                .push((EntityHandle::from(dock), state));
            Ok(())
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
        abi::import::PROCESS_WRITE,
        |mut caller: Caller<'_, HostState>, id: u32, text_ptr: u32, text_len: u32| -> Result<()> {
            let text = read_string(&mut caller, text_ptr, text_len)?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            let mut resources = guest.resources.borrow_mut();
            match resources.get_mut(id as usize) {
                Some(Some(Resource::Process {
                    stdin: Some(stdin), ..
                })) => stdin.write(text).map_err(|error| anyhow!(error)),
                _ => Err(anyhow!("process {id} has no open stdin")),
            }
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::PROCESS_CLOSE_STDIN,
        |caller: Caller<'_, HostState>, id: u32| -> Result<()> {
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            if let Some(Some(Resource::Process { stdin, .. })) =
                guest.resources.borrow_mut().get_mut(id as usize)
                && let Some(mut stdin) = stdin.take()
            {
                stdin.end();
            }
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::OPEN_SHEET,
        |mut caller: Caller<'_, HostState>,
         part: u32,
         placement_ptr: u32,
         placement_len: u32|
         -> Result<()> {
            ensure_overlay_phase("open_sheet")?;
            let placement = read_string(&mut caller, placement_ptr, placement_len)?;
            let guest = caller
                .data()
                .guest
                .upgrade()
                .ok_or_else(|| anyhow!("the guest is gone"))?;
            guest.pending_sheets.borrow_mut().push((part, placement));
            Ok(())
        },
    )?;
    linker.func_wrap(
        module,
        abi::import::CLOSE_SHEET,
        |_caller: Caller<'_, HostState>| -> Result<()> {
            ensure_overlay_phase("close_sheet")?;
            scope::with_current(|window, app| {
                crate::ShellRoot::update(window, app, |root, window, cx| {
                    root.close_sheet(window, cx)
                })
            })
            .flatten()
            .map(|_| ())
            .ok_or_else(|| anyhow!("close_sheet found no ShellRoot in this window"))
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
                CLIPBOARD_READ_DENIED
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
                CLIPBOARD_WRITE_DENIED
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
        let live = app.runtime.live_guests();
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
        assert_eq!(app.runtime.live_guests(), live - 1);
    }

    /// Every live view showing guest `id` other than its root: its parts.
    fn part_views(
        cx: &mut VisualTestContext,
        root: &Entity<ScriptView>,
    ) -> Vec<Entity<ScriptView>> {
        let runtime = cx.update(|_, cx| root.read(cx).runtime());
        let root = root.entity_id();
        let views: Vec<_> = runtime
            .guests
            .borrow()
            .iter()
            .flat_map(|guest| {
                guest
                    .views
                    .borrow()
                    .iter()
                    .map(|(_, view)| view.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
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

        // A part opened as a sheet from the window's edge, which its own
        // handler closes.
        app.click("details");
        let sheet_open = app.context.update(|_, cx| root.read(cx).sheet().is_some());
        assert!(sheet_open);
        app.settle();
        let details = part_views(&mut app.context, &app.view)
            .into_iter()
            .find(|view| app.tree_of(view).contains("Details: 0"))
            .expect("the sheet's view");
        let close = app.handler_in(&details, "close-details", "on_click");
        app.dispatch_click(close);
        let sheet_open = app.context.update(|_, cx| root.read(cx).sheet().is_some());
        assert!(!sheet_open);
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

    /// What the shipped command's `check` does: load through the manifest,
    /// render once and materialize once, in a window nobody draws.
    #[gpui::test]
    fn the_cli_check_renders_a_guest_through_its_manifest(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let directory = application("wasm_grid");
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let description = context
            .update(|window, cx| runtime.check(&directory, window, cx))
            .expect("check the guest");
        assert!(description.contains("\"39:4\""), "{description}");
        let metrics = runtime.metrics().read();
        assert_eq!(metrics.script_renders(), 1);
        assert_eq!(metrics.materializations(), 1);
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

    /// A WebSocket server for one client: greets it, echoes one message back
    /// with a prefix, then waits for the close.
    fn serve_websocket_once() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.send(tungstenite::Message::text("hello")).unwrap();
            loop {
                match socket.read() {
                    Ok(tungstenite::Message::Text(text)) => {
                        socket
                            .send(tungstenite::Message::text(format!(
                                "echo {}",
                                text.as_str()
                            )))
                            .unwrap();
                        socket
                            .send(tungstenite::Message::binary(vec![1u8, 2, 3]))
                            .unwrap();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        port
    }

    #[gpui::test]
    fn a_guest_receives_streams_event_by_event(cx: &mut TestAppContext) {
        let port = serve_websocket_once();
        let directory = application("wasm_streams");
        let store = directory.join("store.json");
        std::fs::write(
            &store,
            serde_json::json!({ "socket_url": format!("ws://127.0.0.1:{port}/") }).to_string(),
        )
        .unwrap();
        cx.update(|cx| crate::init(cx));
        crate::set_storage_path(store);
        crate::set_capabilities(
            crate::Capabilities::new()
                .storage(true)
                .network_hosts(["127.0.0.1".to_owned()])
                .execute(crate::ExecuteGrant::Allowed(vec![
                    "seq".to_owned(),
                    "cat".to_owned(),
                ])),
        );
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        cx.update(|cx| runtime.set_global(cx));
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut context = VisualTestContext::from_window(*window.deref(), cx);
        let view_type = runtime.load_app(&directory, "guest.wasm").expect("load");
        let view = context
            .update(|window, cx| runtime.instantiate_view(&view_type, window, cx))
            .expect("instantiate");

        let mut tree = String::new();
        for _ in 0..300 {
            // The streams are polled on a timer, which the test clock drives.
            context
                .executor()
                .advance_clock(std::time::Duration::from_millis(20));
            context.run_until_parked();
            tree = drawn_tree(&mut context, &view);
            if tree.contains("Exit: 0")
                && tree.contains("Closed: true")
                && tree.contains("Piped: piped")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // One handler call per line, then one for the exit.
        assert!(tree.contains("Lines: 1,2,3"), "{tree}");
        assert!(tree.contains("Exit: 0"), "{tree}");
        // What the guest wrote to `cat`'s stdin came back on its stdout.
        assert!(tree.contains("Piped: piped through stdin"), "{tree}");
        // The greeting, the echo of the guest's reply, the binary message,
        // then the close the guest asked for.
        assert!(tree.contains("Messages: hello,echo ping"), "{tree}");
        assert!(tree.contains("Binary: 1,2,3"), "{tree}");
        assert!(tree.contains("Closed: true"), "{tree}");
    }

    #[gpui::test]
    fn a_guest_docks_its_parts_as_panels(cx: &mut TestAppContext) {
        let mut app = Harness::load_example(cx, "wasm_dock");
        let tree = app.tree();
        assert!(tree.contains("dock"), "{tree}");

        // The host draws a tab per center panel; clicking one shows it.
        let center_tab = |app: &mut Harness| {
            let runtime = app.runtime.clone();
            let dock = runtime
                .guests
                .borrow()
                .iter()
                .find_map(|guest| guest.docks.borrow().first().copied())
                .expect("the guest created a dock");
            let area = runtime.entities().dock(dock).expect("a live dock");
            let state = app.context.update(|_, cx| area.read(cx).dump(cx));
            serde_json::to_value(&state).unwrap()["center"]["children"][0]["info"]["tabs"]["active_index"]
                .as_u64()
        };
        assert_eq!(center_tab(&mut app), Some(1), "the last added tab shows");
        let editor_tab = app
            .context
            .debug_bounds("guest-tab-editor")
            .expect("the host drew a tab for the editor panel");
        app.context
            .simulate_click(editor_tab.center(), gpui::Modifiers::none());
        app.settle();
        assert_eq!(center_tab(&mut app), Some(0));

        // The parts are panels in the dock, drawn by it.
        let panels = part_views(&mut app.context, &app.view);
        assert_eq!(panels.len(), 3, "{tree}");
        let trees: Vec<String> = panels.iter().map(|panel| app.tree_of(panel)).collect();
        let files = panels[trees
            .iter()
            .position(|tree| tree.contains("Files"))
            .expect("files")]
        .clone();
        let editor = panels[trees
            .iter()
            .position(|tree| tree.contains("Editor"))
            .expect("editor")]
        .clone();
        assert!(app.tree_of(&editor).contains("nothing open"));

        // A click in one panel changes what the other shows.
        let open = app.handler_in(&files, "open-lib.rs", "on_click");
        app.dispatch_click(open);
        let editor_tree = app.tree_of(&editor);
        assert!(editor_tree.contains("\"lib.rs\""), "{editor_tree}");

        // A saved layout written while the editor showed, restored after the
        // notes tab was picked again, shows the editor and rebuilds panels.
        let save = app.handler_in(&files, "save-layout", "on_click");
        app.dispatch_click(save);
        let files_tree = app.tree_of(&files);
        assert!(!files_tree.contains("Saved: 0"), "{files_tree}");
        let notes_tab = app
            .context
            .debug_bounds("guest-tab-notes")
            .expect("the host drew a tab for the notes panel");
        app.context
            .simulate_click(notes_tab.center(), gpui::Modifiers::none());
        app.settle();
        assert_eq!(center_tab(&mut app), Some(1));
        let before = part_views(&mut app.context, &app.view).len();
        let restore = app.handler_in(&files, "restore-layout", "on_click");
        app.dispatch_click(restore);
        app.settle();
        assert_eq!(center_tab(&mut app), Some(0), "the saved tab shows again");
        let after = part_views(&mut app.context, &app.view).len();
        assert!(
            after > before,
            "restoring rebuilt the panels: {before} -> {after}"
        );
    }

    #[gpui::test]
    fn a_guest_list_describes_only_the_rows_on_screen(cx: &mut TestAppContext) {
        let mut app = Harness::load_example(cx, "wasm_list");
        // Layout has asked the guest for the visible rows; a render shows
        // which ones.
        let view = app.view.clone();
        app.context
            .update(|_, cx| view.update(cx, |view, cx| view.refresh(cx)));
        app.settle();
        let tree = app.tree();
        let asked = tree
            .lines()
            .find_map(|line| line.split("Asked: ").nth(1))
            .map(|range| range.trim_end_matches('"').to_owned())
            .unwrap_or_default();
        let (start, end) = asked
            .split_once("..")
            .map(|(start, end)| (start.parse::<u32>().unwrap(), end.parse::<u32>().unwrap()))
            .unwrap_or_else(|| panic!("the list asked for no rows: {tree}"));
        assert_eq!(start, 0, "{tree}");
        assert!(
            end > 0 && end < 200,
            "ten thousand rows, but only the visible few are described: {asked}"
        );
        assert!(tree.contains("uniform_list \"rows\" ×10000"), "{tree}");
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

    /// The grid guest's description and materialization cost, the figures
    /// the QuickJS-hosted spike timed beside the script grid of
    /// `tests/benchmark.rs` (there: describe, script 0.78-1.21 ms against the
    /// guest's 0.59-0.82 ms; materialize, equal once the order effect was
    /// controlled). Meaningful only in a release build:
    ///
    /// ```text
    /// cargo test -p gpui-shell --release --no-default-features \
    ///     --features wasm-engine --lib engine::wasm -- --nocapture
    /// ```
    #[gpui::test]
    fn the_grid_guest_is_described_and_timed(cx: &mut TestAppContext) {
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
        let guest = context
            .update(|window, cx| runtime.instantiate(&guest_type, window, cx))
            .expect("instantiate guest");

        let snapshot = context.update(|window, cx| {
            runtime
                .build_snapshot(&guest, None, crate::policy::default(), window, cx)
                .expect("render")
        });
        let tree = snapshot.debug_tree();
        // The last cell of a 40 x 5 grid, and the button under it.
        assert!(tree.contains("\"39:4\""), "{tree}");
        assert!(tree.contains("Button \"refresh\""), "{tree}");
        let nodes = snapshot.len();

        let describe = context.update(|window, cx| {
            best_per_call(|| {
                runtime
                    .build_snapshot(&guest, None, crate::policy::default(), window, cx)
                    .expect("render");
            })
        });
        // The guest's own share: running the module and decoding its output.
        let instance = runtime.guests.borrow().last().cloned().unwrap();
        let mut bytes = 0;
        let guest_only = best_per_call(|| {
            let description = instance.render(0).expect("render");
            bytes = postcard::to_allocvec(&description).unwrap().len();
        });
        let mut materialize = std::time::Duration::MAX;
        context.draw(
            gpui::Point::default(),
            gpui::size(gpui::px(800.), gpui::px(600.)),
            |window, cx| {
                materialize = best_per_call(|| {
                    std::hint::black_box(crate::materialize::materialize(
                        &runtime, &snapshot, window, cx,
                    ));
                });
                gpui::div().into_any_element()
            },
        );

        let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0;
        println!(
            "\n[wasm guest] {nodes} nodes (40x5), module {module_bytes} bytes, \
             description {bytes} bytes encoded\n\
             describe {:.3} ms (module + decode {:.3} ms, record {:.3} ms), \
             materialize {:.3} ms",
            ms(describe),
            ms(guest_only),
            ms(describe.saturating_sub(guest_only)),
            ms(materialize),
        );
    }
}
