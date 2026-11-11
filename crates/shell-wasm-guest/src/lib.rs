//! Write a gpui-shell view in Rust, compile it to `wasm32-unknown-unknown`,
//! and point a `gpui-shell.json` manifest's `entry` at the `.wasm` file.
//!
//! ```ignore
//! use gpui_shell_wasm_guest::prelude::*;
//!
//! struct Counter { count: u32 }
//!
//! impl View for Counter {
//!     fn init(_cx: &mut Cx<Self>) -> Self { Counter { count: 0 } }
//!     fn render(&mut self, cx: &mut Cx<Self>) -> Element {
//!         v_flex().gap(8.)
//!             .child(text(format!("Count: {}", self.count)))
//!             .child(button("inc").child(text("+")).on_click(cx.listener(|this, _, cx| {
//!                 this.count += 1;
//!                 cx.notify();
//!             })))
//!     }
//! }
//! export_view!(Counter);
//! ```
//!
//! Building an element calls nothing on the host: [`Element`] is a tree in
//! guest memory, flattened into one [`abi::Description`] when `render`
//! returns. Handlers stay in the guest too; the description carries only their
//! numbers, and the host calls back with that number when one fires. Retained
//! text inputs live on the host (they own the caret and IME state), so an
//! [`InputState`] is a handle.

use std::rc::Rc;

use abi::{Arg, Component, ComponentArg, Description, Node, Op};
pub use gpui_shell_wasm_abi as abi;
use serde_json::Value;

pub mod diffed;
pub mod reactive;

pub mod prelude {
    pub use crate::{
        CArg, ComponentState, Cx, Data, Dock, Element, Event, Handler, InputState, Part,
        ToastLevel, View, button, checkbox, component, div, dock, export_view, h_flex, input,
        nested, text, v_flex,
    };
}

/// A guest view: the Rust counterpart of a script's `class extends View`.
pub trait View: Sized + 'static {
    /// Constructs the view. Retained state ([`InputState::new`]) is created
    /// here, as a script creates it in `init`.
    fn init(cx: &mut Cx<Self>) -> Self;
    /// Describes the view. Called only when the view was invalidated, never
    /// per frame.
    fn render(&mut self, cx: &mut Cx<Self>) -> Element;
}

/// A handler's payload, as the host serialized it (a checkbox's new state,
/// a click event, or `null`).
pub struct Event(pub Value);

impl Event {
    pub fn as_bool(&self) -> Option<bool> {
        self.0.as_bool()
    }
}

type Callback<V> = Rc<dyn Fn(&mut V, &Event, &mut Cx<V>)>;

type PartRenderer<V> = Rc<dyn Fn(&mut V, &mut Cx<V>) -> Element>;
type ItemRenderer<V> = Rc<dyn Fn(&mut V, std::ops::Range<u32>, &mut Cx<V>) -> Vec<Element>>;
type ItemKey<V> = Rc<dyn Fn(&V, u32) -> String>;

/// The high bit marks a handler registered outside a render (an input's
/// submit handler), which lives as long as the view rather than one render.
const PERSISTENT: u32 = 1 << 31;
/// A render handler's id carries the part that rendered it in these bits, so
/// one part's render never discards another part's handlers.
const PART_SHIFT: u32 = 24;
const INDEX_MASK: u32 = (1 << PART_SHIFT) - 1;

/// What a view's `init`, `render` and handlers are handed.
pub struct Cx<V> {
    /// Handlers registered by the latest render of each part (0 is the view).
    per_render: Vec<Vec<Callback<V>>>,
    persistent: Vec<Callback<V>>,
    parts: Vec<PartRenderer<V>>,
    /// Lists' item renderers and key functions from the latest render of
    /// each part, numbered like render handlers.
    item_renderers: Vec<Vec<ItemRenderer<V>>>,
    item_keys: Vec<Vec<ItemKey<V>>>,
    rendering: Option<u32>,
    notify: bool,
}

/// A registered handler, ready to pass to [`Element::on_click`] and friends.
#[derive(Clone, Copy, Debug)]
pub struct Handler(u32);

/// A separately rendered piece of the view: shown in a dialog
/// ([`Cx::open_dialog`]) or nested in the tree ([`nested`]), each with its own
/// invalidation, as a script's child view or dialog content function is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part(u32);

/// How urgent a toast is.
#[derive(Clone, Copy, Debug)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl<V: 'static> Cx<V> {
    fn new() -> Self {
        Self {
            per_render: vec![Vec::new()],
            persistent: Vec::new(),
            parts: Vec::new(),
            item_renderers: vec![Vec::new()],
            item_keys: vec![Vec::new()],
            rendering: None,
            notify: false,
        }
    }

    /// Wraps a handler that gets the view mutably, the way GPUI's
    /// `cx.listener` does. Inside a render it lives until that part renders
    /// again; elsewhere it lives as long as the view.
    pub fn listener(&mut self, f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static) -> Handler {
        let f: Callback<V> = Rc::new(f);
        match self.rendering {
            Some(part) => {
                let handlers = &mut self.per_render[part as usize];
                handlers.push(f);
                Handler((part << PART_SHIFT) | (handlers.len() as u32 - 1))
            }
            None => {
                self.persistent.push(f);
                Handler(PERSISTENT | (self.persistent.len() as u32 - 1))
            }
        }
    }

    /// Asks the host to render the view, and every part of it, again.
    pub fn notify(&mut self) {
        self.notify = true;
    }

    /// Registers a part: a render function for a piece shown on its own.
    pub fn part(&mut self, render: impl Fn(&mut V, &mut Cx<V>) -> Element + 'static) -> Part {
        self.parts.push(Rc::new(render));
        self.per_render.push(Vec::new());
        self.item_renderers.push(Vec::new());
        self.item_keys.push(Vec::new());
        Part(self.parts.len() as u32)
    }

    /// A lazily rendered list of `count` items of one height, as a script's
    /// `list`: only the visible range is described, during layout, by
    /// `render` (one element per index in the range it is given); `key`
    /// names each item so the list keeps the reader's place when items come
    /// and go. From a render only.
    pub fn list(
        &mut self,
        id: &str,
        count: u32,
        render: impl Fn(&mut V, std::ops::Range<u32>, &mut Cx<V>) -> Vec<Element> + 'static,
        key: impl Fn(&V, u32) -> String + 'static,
    ) -> Element {
        let part = self.rendering.expect("a list is described from a render");
        let renderers = &mut self.item_renderers[part as usize];
        renderers.push(Rc::new(render));
        let render_items = (part << PART_SHIFT) | (renderers.len() as u32 - 1);
        let keys = &mut self.item_keys[part as usize];
        keys.push(Rc::new(key));
        let get_key = (part << PART_SHIFT) | (keys.len() as u32 - 1);
        element(Component::List {
            id: id.to_owned(),
            count,
            render_items,
            get_key,
        })
    }

    /// Opens `part` as a dialog over the window. From a handler only.
    pub fn open_dialog(&mut self, part: Part) {
        unsafe { host::open_dialog(part.0) }
    }

    /// Closes the topmost dialog.
    pub fn close_dialog(&mut self) {
        unsafe { host::close_dialog() }
    }

    /// Opens `part` as a sheet at `placement` (`"left"`, `"right"`, `"top"`
    /// or `"bottom"`). From a handler only.
    pub fn open_sheet(&mut self, part: Part, placement: &str) {
        unsafe { host::open_sheet(part.0, placement.as_ptr() as u32, placement.len() as u32) }
    }

    /// Closes the open sheet.
    pub fn close_sheet(&mut self) {
        unsafe { host::close_sheet() }
    }

    /// Shows a toast, as a script's `window.push_toast`.
    pub fn push_toast(&mut self, title: &str, description: Option<&str>, level: ToastLevel) {
        let level = match level {
            ToastLevel::Info => "info",
            ToastLevel::Success => "success",
            ToastLevel::Warning => "warning",
            ToastLevel::Error => "error",
        };
        let toast = serde_json::json!({
            "title": title,
            "description": description,
            "level": level,
        })
        .to_string();
        unsafe { host::push_toast(toast.as_ptr() as u32, toast.len() as u32) }
    }
}

/// A dock area: panels docked left, right, bottom and center, which the user
/// can drag and resize — a script's `DockArea`. Created from `init` or a
/// handler, drawn with [`dock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dock(u64);

impl Dock {
    pub fn new(id: &str) -> Self {
        Self(unsafe { host::dock_new(id.as_ptr() as u32, id.len() as u32) })
    }

    /// Docks `part` as a panel named `name` at `placement` (`"left"`,
    /// `"right"`, `"bottom"` or `"center"`).
    pub fn add_panel(&self, part: Part, name: &str, placement: &str) {
        unsafe {
            host::dock_add_panel(
                self.0,
                part.0,
                name.as_ptr() as u32,
                name.len() as u32,
                placement.as_ptr() as u32,
                placement.len() as u32,
            )
        }
    }

    /// The current layout, as JSON to keep (in `storage`, say) and hand back
    /// to [`Dock::load`]: a script's `dock.dump()`.
    pub fn dump(&self) -> String {
        take_string(unsafe { host::dock_dump(self.0) })
    }

    /// Restores a layout [`Dock::dump`] wrote. Add the panels first, under
    /// the names the layout was written with: a script's `dock.load(state)`.
    pub fn load(&self, layout: &str) {
        unsafe { host::dock_load(self.0, layout.as_ptr() as u32, layout.len() as u32) }
    }
}

/// Draws a dock area.
pub fn dock(dock: &Dock) -> Element {
    element(Component::Dock(dock.0))
}

/// A part drawn in place, with its own invalidation: a script's child view.
pub fn nested(part: Part) -> Element {
    element(Component::Nested(part.0))
}

/// One element under construction.
pub struct Element {
    kind: Kind,
    ops: Vec<PendingOp>,
    children: Vec<Element>,
}

enum Kind {
    Base(Component),
    Registered { export: String, args: Vec<CArg> },
}

enum PendingOp {
    Op(Op),
    Component(String, Vec<CArg>),
    StateStyle(&'static str, Element),
    Slot(String, Element),
}

/// An argument to a registered component, before the tree is flattened:
/// an element argument is still an [`Element`] here.
pub enum CArg {
    String(String),
    Number(f64),
    Boolean(bool),
    Enum(String),
    Element(Element),
    Handler(Handler),
    /// A registered component's retained state.
    State(u64),
    Array(Vec<CArg>),
    Optional(Option<Box<CArg>>),
}

impl From<&ComponentState> for CArg {
    fn from(state: &ComponentState) -> Self {
        CArg::State(state.handle)
    }
}

pub use abi::Data;

/// A state constructor's argument: plain values only. An element or a
/// handler has no meaning outside a render, so it becomes `null` and the
/// host's schema check reports it.
pub(crate) fn plain_argument(arg: CArg) -> ComponentArg {
    match arg {
        CArg::String(value) => ComponentArg::String(value),
        CArg::Number(value) => ComponentArg::Number(value),
        CArg::Boolean(value) => ComponentArg::Boolean(value),
        CArg::Enum(value) => ComponentArg::Enum(value),
        CArg::State(handle) => ComponentArg::State(handle),
        CArg::Array(values) => {
            ComponentArg::Array(values.into_iter().map(plain_argument).collect())
        }
        CArg::Optional(value) => {
            ComponentArg::Optional(value.map(|value| Box::new(plain_argument(*value))))
        }
        CArg::Element(_) | CArg::Handler(_) => ComponentArg::Optional(None),
    }
}

/// A registered component's retained state (a `gpui-component` `InputState`,
/// ...), owned by the host like a script's: `ComponentState::new("InputState",
/// [...])` is a script's `InputState.new(...)` from `"gpui-component"`.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentState {
    export: String,
    handle: u64,
}

impl ComponentState {
    /// Creates the state. Like a script's, only from `init` or a handler.
    pub fn new(export: &str, args: impl IntoIterator<Item = CArg>) -> Self {
        let args: Vec<ComponentArg> = args.into_iter().map(plain_argument).collect();
        let args = postcard::to_allocvec(&args).unwrap_or_default();
        let handle = unsafe {
            host::state_new(
                export.as_ptr() as u32,
                export.len() as u32,
                args.as_ptr() as u32,
                args.len() as u32,
            )
        };
        Self {
            export: export.to_owned(),
            handle,
        }
    }

    /// Calls one of the state's methods (`value`, `set_value`, ...).
    pub fn call(&self, method: &str, args: impl IntoIterator<Item = Data>) -> Data {
        let args = postcard::to_allocvec(&args.into_iter().collect::<Vec<_>>()).unwrap_or_default();
        let packed = unsafe {
            host::state_call(
                self.handle,
                self.export.as_ptr() as u32,
                self.export.len() as u32,
                method.as_ptr() as u32,
                method.len() as u32,
                args.as_ptr() as u32,
                args.len() as u32,
            )
        };
        let bytes = take_bytes(packed);
        postcard::from_bytes(&bytes).unwrap_or(Data::Null)
    }

    /// Runs `f` on one of the state's input events (`"submit"`, `"change"`,
    /// `"focus"`, `"blur"`).
    pub fn on<V: 'static>(
        &self,
        event: &str,
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) {
        let handler = cx.listener(f);
        unsafe {
            host::state_on(
                self.handle,
                self.export.as_ptr() as u32,
                self.export.len() as u32,
                event.as_ptr() as u32,
                event.len() as u32,
                handler.0,
            )
        }
    }
}

impl From<&str> for CArg {
    fn from(value: &str) -> Self {
        CArg::String(value.to_owned())
    }
}
impl From<String> for CArg {
    fn from(value: String) -> Self {
        CArg::String(value)
    }
}
impl From<f64> for CArg {
    fn from(value: f64) -> Self {
        CArg::Number(value)
    }
}
impl From<bool> for CArg {
    fn from(value: bool) -> Self {
        CArg::Boolean(value)
    }
}
impl From<Handler> for CArg {
    fn from(value: Handler) -> Self {
        CArg::Handler(value)
    }
}
impl From<Element> for CArg {
    fn from(value: Element) -> Self {
        CArg::Element(value)
    }
}

fn element(component: Component) -> Element {
    Element {
        kind: Kind::Base(component),
        ops: Vec::new(),
        children: Vec::new(),
    }
}

/// A component the host's catalog registered, by its constructor's export
/// name: `component("Button", ["add".into()])` is a script's
/// `Button.new("add")` from `"gpui-component"`.
pub fn component(export: &str, args: impl IntoIterator<Item = CArg>) -> Element {
    Element {
        kind: Kind::Registered {
            export: export.to_owned(),
            args: args.into_iter().collect(),
        },
        ops: Vec::new(),
        children: Vec::new(),
    }
}

pub fn div() -> Element {
    element(Component::Div)
}

pub fn h_flex() -> Element {
    element(Component::HFlex)
}

pub fn v_flex() -> Element {
    element(Component::VFlex)
}

pub fn text(text: impl Into<String>) -> Element {
    element(Component::Text(text.into()))
}

/// An unstyled `gpui-base` button: its look comes from the styles you give it
/// and its label from its children, as in a script.
pub fn button(id: impl Into<String>) -> Element {
    element(Component::Button(id.into()))
}

pub fn checkbox(id: impl Into<String>) -> Element {
    element(Component::Checkbox(id.into()))
}

pub fn input(state: &InputState) -> Element {
    element(Component::Input(state.0))
}

impl Element {
    /// A no-argument style method by name (`"size_full"`, `"items_center"`).
    pub fn style(mut self, name: &str) -> Self {
        self.ops.push(PendingOp::Op(Op::Style(name.to_owned())));
        self
    }

    /// A style method with arguments by name (`"gap"`, `"bg"`).
    pub fn style_with(mut self, name: &str, args: impl IntoIterator<Item = Arg>) -> Self {
        self.ops.push(PendingOp::Op(Op::ParamStyle(
            name.to_owned(),
            args.into_iter().collect(),
        )));
        self
    }

    /// Styles applied while the pointer is over the element:
    /// `.hover(|style| style.bg("muted"))`.
    pub fn hover(self, styles: impl FnOnce(Element) -> Element) -> Self {
        self.state_style("hover", styles)
    }
    /// Styles applied while the element is pressed.
    pub fn active(self, styles: impl FnOnce(Element) -> Element) -> Self {
        self.state_style("active", styles)
    }
    /// Styles applied while the element has focus.
    pub fn focus(self, styles: impl FnOnce(Element) -> Element) -> Self {
        self.state_style("focus", styles)
    }

    fn state_style(mut self, name: &'static str, styles: impl FnOnce(Element) -> Element) -> Self {
        // The declarations are collected on a detached element, as a script's
        // are; only its style calls mean anything.
        self.ops.push(PendingOp::StateStyle(name, styles(div())));
        self
    }

    /// Fills a named slot the component renders in a place of its own
    /// (`content`, `trigger`, `header`, ...), as a script's `.content(el)`.
    pub fn slot(mut self, name: &str, element: Element) -> Self {
        self.ops.push(PendingOp::Slot(name.to_owned(), element));
        self
    }

    /// A method the registered component declares (`label`, `primary`,
    /// `value`): `.call("label", ["Add".into()])`.
    pub fn call(mut self, name: &str, args: impl IntoIterator<Item = CArg>) -> Self {
        self.ops.push(PendingOp::Component(
            name.to_owned(),
            args.into_iter().collect(),
        ));
        self
    }

    fn number(self, name: &str, value: f64) -> Self {
        self.style_with(name, [Arg::Number(value)])
    }

    fn string(self, name: &str, value: &str) -> Self {
        self.style_with(name, [Arg::Str(value.to_owned())])
    }

    pub fn size_full(self) -> Self {
        self.style("size_full")
    }
    pub fn w_full(self) -> Self {
        self.style("w_full")
    }
    pub fn flex_1(self) -> Self {
        self.style("flex_1")
    }
    pub fn items_center(self) -> Self {
        self.style("items_center")
    }
    pub fn justify_between(self) -> Self {
        self.style("justify_between")
    }
    pub fn cursor_pointer(self) -> Self {
        self.style("cursor_pointer")
    }
    pub fn gap(self, px: f64) -> Self {
        self.number("gap", px)
    }
    pub fn p(self, px: f64) -> Self {
        self.number("p", px)
    }
    pub fn px(self, px: f64) -> Self {
        self.number("px", px)
    }
    pub fn py(self, px: f64) -> Self {
        self.number("py", px)
    }
    pub fn h(self, px: f64) -> Self {
        self.number("h", px)
    }
    pub fn rounded(self, px: f64) -> Self {
        self.number("rounded", px)
    }
    pub fn text_size(self, px: f64) -> Self {
        self.number("text_size", px)
    }
    pub fn border(self, px: f64) -> Self {
        self.number("border", px)
    }
    /// A semantic color token (`"background"`, `"primary"`) or `#rrggbb`.
    pub fn bg(self, color: &str) -> Self {
        self.string("bg", color)
    }
    pub fn text_color(self, color: &str) -> Self {
        self.string("text_color", color)
    }
    pub fn border_color(self, color: &str) -> Self {
        self.string("border_color", color)
    }

    /// A behavior method by name (`"checked"`, `"disabled"`).
    pub fn method(mut self, name: &str, args: impl IntoIterator<Item = Arg>) -> Self {
        self.ops.push(PendingOp::Op(Op::Method(
            name.to_owned(),
            args.into_iter().collect(),
        )));
        self
    }
    pub fn checked(self, checked: bool) -> Self {
        self.method("checked", [Arg::Bool(checked)])
    }

    pub fn on_click(mut self, handler: Handler) -> Self {
        self.ops.push(PendingOp::Op(Op::Callback(
            "on_click".to_owned(),
            handler.0,
        )));
        self
    }
    /// A checkbox's toggle; the [`Event`] carries the requested state.
    pub fn on_change(mut self, handler: Handler) -> Self {
        self.ops.push(PendingOp::Op(Op::Callback(
            "on_change".to_owned(),
            handler.0,
        )));
        self
    }

    pub fn child(mut self, child: Element) -> Self {
        self.children.push(child);
        self
    }
    pub fn children(mut self, children: impl IntoIterator<Item = Element>) -> Self {
        self.children.extend(children);
        self
    }

    /// Flattens the tree into the description the host records.
    pub fn into_description(self) -> Description {
        let mut nodes = Vec::new();
        let root = self.flatten_into(&mut nodes);
        Description { root, nodes }
    }

    /// Appends the tree to `nodes` and returns the root's index.
    fn flatten_into(self, nodes: &mut Vec<Node>) -> u32 {
        // Post-order: children and element arguments land before the node
        // that refers to them, which is the order the host records in.
        fn argument(arg: CArg, nodes: &mut Vec<Node>) -> ComponentArg {
            match arg {
                CArg::String(value) => ComponentArg::String(value),
                CArg::Number(value) => ComponentArg::Number(value),
                CArg::Boolean(value) => ComponentArg::Boolean(value),
                CArg::Enum(value) => ComponentArg::Enum(value),
                CArg::Element(element) => ComponentArg::Element(flatten(element, nodes)),
                CArg::Handler(handler) => ComponentArg::Callback(handler.0),
                CArg::State(handle) => ComponentArg::State(handle),
                CArg::Array(values) => ComponentArg::Array(
                    values
                        .into_iter()
                        .map(|value| argument(value, nodes))
                        .collect(),
                ),
                CArg::Optional(value) => {
                    ComponentArg::Optional(value.map(|value| Box::new(argument(*value, nodes))))
                }
            }
        }
        fn flatten(element: Element, nodes: &mut Vec<Node>) -> u32 {
            let component = match element.kind {
                Kind::Base(component) => component,
                Kind::Registered { export, args } => Component::Registered {
                    export,
                    args: args.into_iter().map(|arg| argument(arg, nodes)).collect(),
                },
            };
            let ops = element
                .ops
                .into_iter()
                .map(|op| match op {
                    PendingOp::Op(op) => op,
                    PendingOp::Component(name, args) => Op::Component(
                        name,
                        args.into_iter().map(|arg| argument(arg, nodes)).collect(),
                    ),
                    PendingOp::StateStyle(name, styles) => {
                        Op::StateStyle(name.to_owned(), flatten(styles, nodes))
                    }
                    PendingOp::Slot(name, element) => Op::Slot(name, flatten(element, nodes)),
                })
                .collect();
            let children = element
                .children
                .into_iter()
                .map(|child| flatten(child, nodes))
                .collect();
            nodes.push(Node {
                component,
                ops,
                children,
            });
            nodes.len() as u32 - 1
        }
        flatten(self, nodes)
    }
}

/// A list's items, flattened into one node list with a root per item.
fn items(elements: Vec<Element>) -> abi::Items {
    let mut nodes = Vec::new();
    let roots = elements
        .into_iter()
        .map(|element| element.flatten_into(&mut nodes))
        .collect();
    abi::Items { roots, nodes }
}

/// A retained text input, owned by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputState(u32);

impl InputState {
    pub fn new(placeholder: &str) -> Self {
        Self(unsafe { host::input_new(placeholder.as_ptr() as u32, placeholder.len() as u32) })
    }

    pub fn value(&self) -> String {
        take_string(unsafe { host::input_value(self.0) })
    }

    pub fn set_value(&self, value: &str) {
        unsafe { host::input_set_value(self.0, value.as_ptr() as u32, value.len() as u32) }
    }

    /// Runs `f` when Enter is pressed in this input.
    pub fn on_submit<V: 'static>(
        &self,
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) {
        let handler = cx.listener(f);
        unsafe { host::input_on_submit(self.0, handler.0) }
    }
}

/// The application's persistent store: a script's `localStorage`, under the
/// same `storage` capability. Values are strings, as there.
pub mod storage {
    use super::host;

    pub fn get(key: &str) -> Option<String> {
        let packed = unsafe { host::storage_get(key.as_ptr() as u32, key.len() as u32) };
        (packed != crate::abi::ABSENT).then(|| super::take_string(packed))
    }

    pub fn set(key: &str, value: &str) {
        unsafe {
            host::storage_set(
                key.as_ptr() as u32,
                key.len() as u32,
                value.as_ptr() as u32,
                value.len() as u32,
            )
        }
    }

    pub fn remove(key: &str) {
        unsafe { host::storage_remove(key.as_ptr() as u32, key.len() as u32) }
    }
}

/// Files inside the application's `capabilities.fs` roots, as a script's
/// `fs` module reaches them. A path outside every granted root traps the
/// guest with the capability's message.
pub mod fs {
    use super::host;

    pub fn read_text(path: &str) -> String {
        super::take_string(unsafe { host::fs_read_text(path.as_ptr() as u32, path.len() as u32) })
    }

    pub fn write_text(path: &str, text: &str) {
        unsafe {
            host::fs_write_text(
                path.as_ptr() as u32,
                path.len() as u32,
                text.as_ptr() as u32,
                text.len() as u32,
            )
        }
    }

    /// [`read_text`] off the main thread: returns at once, and `f` gets
    /// `{"text"}` or `{"error"}`. The grant is checked at the call.
    pub fn read_text_async<V: 'static>(
        path: &str,
        cx: &mut super::Cx<V>,
        f: impl Fn(&mut V, &super::Event, &mut super::Cx<V>) + 'static,
    ) {
        let handler = cx.listener(f);
        unsafe { host::fs_read_text_async(path.as_ptr() as u32, path.len() as u32, handler.0) }
    }

    /// [`write_text`] off the main thread; `f` gets `{"ok": true}` or
    /// `{"error"}`.
    pub fn write_text_async<V: 'static>(
        path: &str,
        text: &str,
        cx: &mut super::Cx<V>,
        f: impl Fn(&mut V, &super::Event, &mut super::Cx<V>) + 'static,
    ) {
        let handler = cx.listener(f);
        unsafe {
            host::fs_write_text_async(
                path.as_ptr() as u32,
                path.len() as u32,
                text.as_ptr() as u32,
                text.len() as u32,
                handler.0,
            )
        }
    }
}

/// HTTP under the application's network grants, as a script's `fetch`.
pub mod http {
    use super::{Cx, Event, host};

    /// Starts a GET and returns at once. `f` runs later, on the main thread,
    /// with `{"status": .., "body": ..}` or `{"error": ..}` as its event.
    pub fn get<V: 'static>(
        url: &str,
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) {
        let handler = cx.listener(f);
        unsafe { host::http_get(url.as_ptr() as u32, url.len() as u32, handler.0) }
    }

    /// Any method, with headers and a body: `request("POST", url,
    /// &[("content-type", "application/json")], body, cx, f)`.
    pub fn request<V: 'static>(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) {
        let request = serde_json::json!({
            "method": method,
            "url": url,
            "headers": headers,
            "body": body,
        })
        .to_string();
        let handler = cx.listener(f);
        unsafe { host::http_request(request.as_ptr() as u32, request.len() as u32, handler.0) }
    }
}

/// Commands under the application's execute grant, as a script's
/// `process.run`.
pub mod process {
    use super::{Cx, Event, host};

    /// Starts `command` and returns at once. `f` runs on the main thread with
    /// `{"code", "stdout", "stderr"}` or `{"error"}` as its event.
    pub fn run<V: 'static>(
        command: &str,
        args: &[&str],
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) {
        let request = serde_json::json!({ "command": command, "args": args }).to_string();
        let handler = cx.listener(f);
        unsafe { host::process_run(request.as_ptr() as u32, request.len() as u32, handler.0) }
    }
}

/// Something the guest started that runs until it ends or is stopped: a
/// streamed process or a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource(u32);

impl Resource {
    /// Kills the process or closes the socket.
    pub fn stop(self) {
        unsafe { host::resource_stop(self.0) }
    }

    /// Writes to a streamed process's stdin.
    pub fn write(self, text: &str) {
        unsafe { host::process_write(self.0, text.as_ptr() as u32, text.len() as u32) }
    }

    /// Closes a streamed process's stdin, so it sees the end of its input.
    pub fn close_stdin(self) {
        unsafe { host::process_close_stdin(self.0) }
    }
}

/// A process whose output arrives as it is written, as a script's
/// `process.spawn`.
pub mod process_stream {
    use super::{Cx, Event, Resource, host};

    /// Starts `command`; `f` receives each line as `{"stdout"}` or
    /// `{"stderr"}`, then `{"exit": {"code", "signal", "error"}}`.
    pub fn spawn<V: 'static>(
        command: &str,
        args: &[&str],
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) -> Resource {
        let request = serde_json::json!({ "command": command, "args": args }).to_string();
        let handler = cx.listener(f);
        Resource(unsafe {
            host::process_spawn(request.as_ptr() as u32, request.len() as u32, handler.0)
        })
    }
}

/// WebSockets under the application's network grants, as a script's
/// `WebSocket.connect`.
pub mod socket {
    use super::{Cx, Event, Resource, host};

    /// Opens `url`; `f` receives `{"open"}`, each `{"message"}`, then
    /// `{"close"}` or `{"error"}`.
    pub fn open<V: 'static>(
        url: &str,
        cx: &mut Cx<V>,
        f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static,
    ) -> Resource {
        let handler = cx.listener(f);
        Resource(unsafe { host::socket_open(url.as_ptr() as u32, url.len() as u32, handler.0) })
    }

    /// Sends a text message on an open socket.
    pub fn send(socket: Resource, text: &str) {
        unsafe { host::socket_send(socket.0, text.as_ptr() as u32, text.len() as u32) }
    }
}

/// This run's in-memory store: a script's `sessionStorage`. No capability,
/// nothing reaches the disk.
pub mod session_storage {
    use super::host;

    pub fn get(key: &str) -> Option<String> {
        let packed = unsafe { host::session_get(key.as_ptr() as u32, key.len() as u32) };
        (packed != crate::abi::ABSENT).then(|| super::take_string(packed))
    }

    pub fn set(key: &str, value: &str) {
        unsafe {
            host::session_set(
                key.as_ptr() as u32,
                key.len() as u32,
                value.as_ptr() as u32,
                value.len() as u32,
            )
        }
    }
}

/// The system clipboard's text, under the `clipboard` grants.
pub mod clipboard {
    use super::host;

    pub fn read() -> Option<String> {
        let packed = unsafe { host::clipboard_read() };
        (packed != crate::abi::ABSENT).then(|| super::take_string(packed))
    }

    pub fn write(text: &str) {
        unsafe { host::clipboard_write(text.as_ptr() as u32, text.len() as u32) }
    }
}

/// Writes a line to the host log.
pub fn log(line: &str) {
    unsafe { host::log(line.as_ptr() as u32, line.len() as u32) }
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "gpui_shell")]
    unsafe extern "C" {
        pub fn input_new(ptr: u32, len: u32) -> u32;
        pub fn input_value(handle: u32) -> u64;
        pub fn input_set_value(handle: u32, ptr: u32, len: u32);
        pub fn input_on_submit(handle: u32, callback: u32);
        pub fn log(ptr: u32, len: u32);
        pub fn process_write(id: u32, text_ptr: u32, text_len: u32);
        pub fn process_close_stdin(id: u32);
        pub fn open_sheet(part: u32, placement_ptr: u32, placement_len: u32);
        pub fn close_sheet();
        pub fn dock_new(id_ptr: u32, id_len: u32) -> u64;
        pub fn dock_dump(dock: u64) -> u64;
        pub fn dock_load(dock: u64, layout_ptr: u32, layout_len: u32);
        pub fn dock_add_panel(
            dock: u64,
            part: u32,
            name_ptr: u32,
            name_len: u32,
            placement_ptr: u32,
            placement_len: u32,
        );
        pub fn process_spawn(request_ptr: u32, request_len: u32, callback: u32) -> u32;
        pub fn socket_open(url_ptr: u32, url_len: u32, callback: u32) -> u32;
        pub fn socket_send(id: u32, text_ptr: u32, text_len: u32);
        pub fn resource_stop(id: u32);
        pub fn open_dialog(part: u32);
        pub fn close_dialog();
        pub fn push_toast(toast_ptr: u32, toast_len: u32);
        pub fn http_get(url_ptr: u32, url_len: u32, callback: u32);
        pub fn http_request(request_ptr: u32, request_len: u32, callback: u32);
        pub fn process_run(request_ptr: u32, request_len: u32, callback: u32);
        pub fn fs_read_text_async(path_ptr: u32, path_len: u32, callback: u32);
        pub fn fs_write_text_async(
            path_ptr: u32,
            path_len: u32,
            text_ptr: u32,
            text_len: u32,
            callback: u32,
        );
        pub fn session_get(key_ptr: u32, key_len: u32) -> u64;
        pub fn session_set(key_ptr: u32, key_len: u32, value_ptr: u32, value_len: u32);
        pub fn clipboard_read() -> u64;
        pub fn clipboard_write(text_ptr: u32, text_len: u32);
        pub fn fs_read_text(path_ptr: u32, path_len: u32) -> u64;
        pub fn fs_write_text(path_ptr: u32, path_len: u32, text_ptr: u32, text_len: u32);
        pub fn storage_get(key_ptr: u32, key_len: u32) -> u64;
        pub fn storage_set(key_ptr: u32, key_len: u32, value_ptr: u32, value_len: u32);
        pub fn storage_remove(key_ptr: u32, key_len: u32);
        pub fn state_new(export_ptr: u32, export_len: u32, args_ptr: u32, args_len: u32) -> u64;
        pub fn state_call(
            handle: u64,
            export_ptr: u32,
            export_len: u32,
            method_ptr: u32,
            method_len: u32,
            args_ptr: u32,
            args_len: u32,
        ) -> u64;
        pub fn state_on(
            handle: u64,
            export_ptr: u32,
            export_len: u32,
            event_ptr: u32,
            event_len: u32,
            callback: u32,
        );
    }
}

/// Native builds (for the SDK's own tests) have no host to call.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
mod host {
    pub unsafe fn input_new(_: u32, _: u32) -> u32 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn input_value(_: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn input_set_value(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn input_on_submit(_: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn log(_: u32, _: u32) {}
    pub unsafe fn process_write(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn process_close_stdin(_: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn open_sheet(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn close_sheet() {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn dock_dump(_: u64) -> u64 {
        unreachable!()
    }
    pub unsafe fn dock_load(_: u64, _: u32, _: u32) {
        unreachable!()
    }
    pub unsafe fn dock_new(_: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn dock_add_panel(_: u64, _: u32, _: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn process_spawn(_: u32, _: u32, _: u32) -> u32 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn socket_open(_: u32, _: u32, _: u32) -> u32 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn socket_send(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn resource_stop(_: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn open_dialog(_: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn close_dialog() {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn push_toast(_: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn http_get(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn http_request(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn process_run(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn fs_read_text_async(_: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn fs_write_text_async(_: u32, _: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn session_get(_: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn session_set(_: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn clipboard_read() -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn clipboard_write(_: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn fs_read_text(_: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn fs_write_text(_: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn storage_get(_: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn storage_set(_: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn storage_remove(_: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn state_new(_: u32, _: u32, _: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn state_call(_: u64, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32) -> u64 {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
    pub unsafe fn state_on(_: u64, _: u32, _: u32, _: u32, _: u32, _: u32) {
        unimplemented!("gpui-shell host imports exist only in a wasm32 guest")
    }
}

/// Reclaims a buffer the host wrote through the `alloc` export.
pub(crate) fn take_string(packed: u64) -> String {
    String::from_utf8(take_bytes(packed)).unwrap_or_default()
}

fn take_bytes(packed: u64) -> Vec<u8> {
    let (ptr, len) = abi::unpack(packed);
    if len == 0 {
        return Vec::new();
    }
    unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) }
}

/// Backs the exports `export_view!` generates. Not for direct use.
#[doc(hidden)]
pub struct Guest<V: View> {
    view: Option<V>,
    cx: Cx<V>,
    /// The last render's encoded description, kept until the next render so
    /// the pointer handed to the host stays valid while it reads.
    last: Vec<u8>,
    /// The last render, for a view exported over the retained protocol.
    differ: diffed::Differ,
    /// The same for the last item range and the last item key, which the
    /// host asks for during layout, between renders.
    last_items: Vec<u8>,
    last_key: Vec<u8>,
}

impl<V: View> Default for Guest<V> {
    fn default() -> Self {
        Self {
            view: None,
            cx: Cx::new(),
            last: Vec::new(),
            differ: diffed::Differ::default(),
            last_items: Vec::new(),
            last_key: Vec::new(),
        }
    }
}

impl<V: View> Guest<V> {
    pub fn init(&mut self) {
        self.view = Some(V::init(&mut self.cx));
    }

    /// Renders the view (`part` 0) or one of its parts.
    pub fn render(&mut self, part: u32) -> u64 {
        let Some(description) = self.describe(part) else {
            return abi::pack(0, 0);
        };
        self.last = postcard::to_allocvec(&description).unwrap_or_default();
        abi::pack(self.last.as_ptr() as u32, self.last.len() as u32)
    }

    /// Builds the view (`part` 0) or one of its parts, registering this
    /// render's handlers.
    fn describe(&mut self, part: u32) -> Option<Description> {
        let view = self.view.as_mut().expect("render before init");
        let renderer = match part {
            0 => None,
            part => Some(self.cx.parts.get(part as usize - 1)?.clone()),
        };
        self.cx.per_render[part as usize].clear();
        self.cx.item_renderers[part as usize].clear();
        self.cx.item_keys[part as usize].clear();
        self.cx.rendering = Some(part);
        let element = match renderer {
            None => view.render(&mut self.cx),
            Some(renderer) => renderer(view, &mut self.cx),
        };
        self.cx.rendering = None;
        Some(element.into_description())
    }

    /// Initializes the view and returns its first batch, for the retained
    /// protocol.
    pub fn retained_init(&mut self) -> u64 {
        self.init();
        self.retained_render()
    }

    /// Renders the view, compares it with the last render and returns the
    /// operations between them.
    fn retained_render(&mut self) -> u64 {
        if let Some(description) = self.describe(0) {
            self.differ.update(&description);
        }
        let batch = self.differ.take();
        diffed::pack(&batch, &mut self.last)
    }

    /// Runs the handler an event is addressed to, renders again if it
    /// notified, and returns the batch.
    pub fn retained_event(
        &mut self,
        kind: u32,
        target: u64,
        name: Vec<u8>,
        payload: Vec<u8>,
    ) -> u64 {
        let callback = match kind {
            abi::retained::target::NODE => {
                let name = String::from_utf8(name).unwrap_or_default();
                self.differ.handler(target, &name)
            }
            abi::retained::target::HANDLER => Some(target as u32),
            _ => None,
        };
        let notify = callback.is_some_and(|callback| self.dispatch(callback, payload) != 0);
        if notify {
            self.retained_render()
        } else {
            let batch = self.differ.take();
            diffed::pack(&batch, &mut self.last)
        }
    }

    /// Describes items `start..end` of a list, during the host's layout. The
    /// items' handlers join their part's render handlers.
    pub fn render_items(&mut self, renderer: u32, start: u32, end: u32) -> u64 {
        let part = renderer >> PART_SHIFT;
        let found = self
            .cx
            .item_renderers
            .get(part as usize)
            .and_then(|renderers| renderers.get((renderer & INDEX_MASK) as usize))
            .cloned();
        let (Some(render), Some(view)) = (found, self.view.as_mut()) else {
            return abi::pack(0, 0);
        };
        self.cx.rendering = Some(part);
        let elements = render(view, start..end, &mut self.cx);
        self.cx.rendering = None;
        self.last_items = postcard::to_allocvec(&items(elements)).unwrap_or_default();
        abi::pack(
            self.last_items.as_ptr() as u32,
            self.last_items.len() as u32,
        )
    }

    /// The key of a list's item.
    pub fn item_key(&mut self, get_key: u32, index: u32) -> u64 {
        let found = self
            .cx
            .item_keys
            .get((get_key >> PART_SHIFT) as usize)
            .and_then(|keys| keys.get((get_key & INDEX_MASK) as usize))
            .cloned();
        let (Some(key), Some(view)) = (found, self.view.as_ref()) else {
            return abi::pack(0, 0);
        };
        self.last_key = key(view, index).into_bytes();
        abi::pack(self.last_key.as_ptr() as u32, self.last_key.len() as u32)
    }

    pub fn dispatch(&mut self, callback: u32, payload: Vec<u8>) -> u32 {
        let event = Event(serde_json::from_slice(&payload).unwrap_or(Value::Null));
        let handler = if callback & PERSISTENT != 0 {
            self.cx.persistent.get((callback & !PERSISTENT) as usize)
        } else {
            self.cx
                .per_render
                .get((callback >> PART_SHIFT) as usize)
                .and_then(|handlers| handlers.get((callback & INDEX_MASK) as usize))
        }
        .cloned();
        let (Some(handler), Some(view)) = (handler, self.view.as_mut()) else {
            return 0;
        };
        self.cx.notify = false;
        handler(view, &event, &mut self.cx);
        u32::from(std::mem::take(&mut self.cx.notify))
    }
}

/// Generates the module's exports for one view type.
#[macro_export]
macro_rules! export_view {
    ($view:ty) => {
        thread_local! {
            static GUEST: ::std::cell::RefCell<$crate::Guest<$view>> =
                ::std::cell::RefCell::new($crate::Guest::default());
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_alloc(len: u32) -> u32 {
            let mut buffer = ::std::vec::Vec::<u8>::with_capacity(len as usize);
            let ptr = buffer.as_mut_ptr();
            ::std::mem::forget(buffer);
            ptr as u32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_init() {
            GUEST.with(|guest| guest.borrow_mut().init());
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_render(part: u32) -> u64 {
            GUEST.with(|guest| guest.borrow_mut().render(part))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_render_items(renderer: u32, start: u32, end: u32) -> u64 {
            GUEST.with(|guest| guest.borrow_mut().render_items(renderer, start, end))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_item_key(get_key: u32, index: u32) -> u64 {
            GUEST.with(|guest| guest.borrow_mut().item_key(get_key, index))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_dispatch(callback: u32, ptr: u32, len: u32) -> u32 {
            let payload = unsafe {
                ::std::vec::Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize)
            };
            GUEST.with(|guest| guest.borrow_mut().dispatch(callback, payload))
        }
    };
}

/// Generates the retained protocol's exports for a view written as a GPUI
/// view is ([`View`]): each render after a notify is compared with the last,
/// and only the difference crosses ([`diffed`]).
#[macro_export]
macro_rules! export_view_retained {
    ($view:ty) => {
        thread_local! {
            static GUEST: ::std::cell::RefCell<$crate::Guest<$view>> =
                ::std::cell::RefCell::new($crate::Guest::default());
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_alloc(len: u32) -> u32 {
            let mut buffer = ::std::vec::Vec::<u8>::with_capacity(len as usize);
            let ptr = buffer.as_mut_ptr();
            ::std::mem::forget(buffer);
            ptr as u32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_retained_init() -> u64 {
            GUEST.with(|guest| guest.borrow_mut().retained_init())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_retained_event(
            kind: u32,
            target: u64,
            name_ptr: u32,
            name_len: u32,
            payload_ptr: u32,
            payload_len: u32,
        ) -> u64 {
            let (name, payload) = unsafe {
                (
                    ::std::vec::Vec::from_raw_parts(
                        name_ptr as *mut u8,
                        name_len as usize,
                        name_len as usize,
                    ),
                    ::std::vec::Vec::from_raw_parts(
                        payload_ptr as *mut u8,
                        payload_len as usize,
                        payload_len as usize,
                    ),
                )
            };
            GUEST.with(|guest| {
                guest
                    .borrow_mut()
                    .retained_event(kind, target, name, payload)
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_flattens_children_before_their_parent() {
        let description = v_flex()
            .gap(8.)
            .child(text("a"))
            .child(h_flex().child(text("b")))
            .into_description();
        assert_eq!(description.nodes.len(), 4);
        let root = &description.nodes[description.root as usize];
        assert_eq!(root.component, Component::VFlex);
        assert_eq!(
            root.ops,
            [Op::ParamStyle("gap".into(), vec![Arg::Number(8.)])]
        );
        let texts: Vec<_> = root
            .children
            .iter()
            .map(|&i| description.nodes[i as usize].component.clone())
            .collect();
        assert_eq!(texts[0], Component::Text("a".into()));
        assert_eq!(texts[1], Component::HFlex);
    }

    struct Counter {
        count: u32,
    }

    impl View for Counter {
        fn init(_cx: &mut Cx<Self>) -> Self {
            Counter { count: 0 }
        }
        fn render(&mut self, cx: &mut Cx<Self>) -> Element {
            button("inc")
                .child(text(self.count.to_string()))
                .on_click(cx.listener(|this, _, cx| {
                    this.count += 1;
                    cx.notify();
                }))
        }
    }

    #[test]
    fn render_handlers_dispatch_by_number_and_request_a_rerender() {
        let mut guest = Guest::<Counter>::default();
        guest.init();
        guest.render(0);
        let description: Description = postcard::from_bytes(&guest.last).unwrap();
        let root = &description.nodes[description.root as usize];
        assert_eq!(root.ops, [Op::Callback("on_click".into(), 0)]);
        assert_eq!(guest.dispatch(0, b"null".to_vec()), 1);
        assert_eq!(guest.view.as_ref().unwrap().count, 1);
        // A number this render never handed out is ignored.
        assert_eq!(guest.dispatch(7, b"null".to_vec()), 0);
    }

    #[test]
    fn a_part_keeps_its_own_handlers_across_the_views_render() {
        struct Paged {
            count: u32,
            part: Option<Part>,
        }
        impl View for Paged {
            fn init(cx: &mut Cx<Self>) -> Self {
                let part = cx.part(|this: &mut Paged, cx| {
                    button("part")
                        .child(text(this.count.to_string()))
                        .on_click(cx.listener(|this, _, cx| {
                            this.count += 10;
                            cx.notify();
                        }))
                });
                Paged {
                    count: 0,
                    part: Some(part),
                }
            }
            fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
                nested(self.part.unwrap())
            }
        }

        let mut guest = Guest::<Paged>::default();
        guest.init();
        guest.render(1);
        let part: Description = postcard::from_bytes(&guest.last).unwrap();
        let Op::Callback(_, handler) = &part.nodes[part.root as usize].ops[0] else {
            panic!("the part's button has a handler");
        };
        // The view rendering again does not discard the part's handler.
        guest.render(0);
        let view: Description = postcard::from_bytes(&guest.last).unwrap();
        assert_eq!(
            view.nodes[view.root as usize].component,
            Component::Nested(1)
        );
        assert_eq!(guest.dispatch(*handler, b"null".to_vec()), 1);
        assert_eq!(guest.view.as_ref().unwrap().count, 10);
    }
}
