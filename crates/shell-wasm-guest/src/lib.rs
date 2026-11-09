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

use abi::{Arg, Component, Description, Node, Op};
pub use gpui_shell_wasm_abi as abi;
use serde_json::Value;

pub mod prelude {
    pub use crate::{
        Cx, Element, Event, Handler, InputState, View, button, checkbox, div, export_view, h_flex,
        input, text, v_flex,
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

/// The high bit marks a handler registered outside `render` (an input's
/// submit handler), which lives as long as the view rather than one render.
const PERSISTENT: u32 = 1 << 31;

/// What a view's `init`, `render` and handlers are handed.
pub struct Cx<V> {
    per_render: Vec<Callback<V>>,
    persistent: Vec<Callback<V>>,
    rendering: bool,
    notify: bool,
}

/// A registered handler, ready to pass to [`Element::on_click`] and friends.
#[derive(Clone, Copy, Debug)]
pub struct Handler(u32);

impl<V: 'static> Cx<V> {
    fn new() -> Self {
        Self {
            per_render: Vec::new(),
            persistent: Vec::new(),
            rendering: false,
            notify: false,
        }
    }

    /// Wraps a handler that gets the view mutably, the way GPUI's
    /// `cx.listener` does. Inside `render` it lives until the next render;
    /// elsewhere it lives as long as the view.
    pub fn listener(&mut self, f: impl Fn(&mut V, &Event, &mut Cx<V>) + 'static) -> Handler {
        let f: Callback<V> = Rc::new(f);
        if self.rendering {
            self.per_render.push(f);
            Handler(self.per_render.len() as u32 - 1)
        } else {
            self.persistent.push(f);
            Handler(PERSISTENT | (self.persistent.len() as u32 - 1))
        }
    }

    /// Asks the host to render the view again.
    pub fn notify(&mut self) {
        self.notify = true;
    }
}

/// One element under construction.
pub struct Element {
    component: Component,
    ops: Vec<Op>,
    children: Vec<Element>,
}

fn element(component: Component) -> Element {
    Element {
        component,
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
        self.ops.push(Op::Style(name.to_owned()));
        self
    }

    /// A style method with arguments by name (`"gap"`, `"bg"`).
    pub fn style_with(mut self, name: &str, args: impl IntoIterator<Item = Arg>) -> Self {
        self.ops
            .push(Op::ParamStyle(name.to_owned(), args.into_iter().collect()));
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
        self.ops
            .push(Op::Method(name.to_owned(), args.into_iter().collect()));
        self
    }
    pub fn checked(self, checked: bool) -> Self {
        self.method("checked", [Arg::Bool(checked)])
    }

    pub fn on_click(mut self, handler: Handler) -> Self {
        self.ops
            .push(Op::Callback("on_click".to_owned(), handler.0));
        self
    }
    /// A checkbox's toggle; the [`Event`] carries the requested state.
    pub fn on_change(mut self, handler: Handler) -> Self {
        self.ops
            .push(Op::Callback("on_change".to_owned(), handler.0));
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
        fn flatten(element: Element, nodes: &mut Vec<Node>) -> u32 {
            let children = element
                .children
                .into_iter()
                .map(|child| flatten(child, nodes))
                .collect();
            nodes.push(Node {
                component: element.component,
                ops: element.ops,
                children,
            });
            nodes.len() as u32 - 1
        }
        let mut nodes = Vec::new();
        let root = flatten(self, &mut nodes);
        Description { root, nodes }
    }
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
}

/// Reclaims a buffer the host wrote through the `alloc` export.
fn take_string(packed: u64) -> String {
    let (ptr, len) = abi::unpack(packed);
    if len == 0 {
        return String::new();
    }
    let bytes = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
    String::from_utf8(bytes).unwrap_or_default()
}

/// Backs the exports `export_view!` generates. Not for direct use.
#[doc(hidden)]
pub struct Guest<V: View> {
    view: Option<V>,
    cx: Cx<V>,
    /// The last render's encoded description, kept until the next render so
    /// the pointer handed to the host stays valid while it reads.
    last: Vec<u8>,
}

impl<V: View> Default for Guest<V> {
    fn default() -> Self {
        Self {
            view: None,
            cx: Cx::new(),
            last: Vec::new(),
        }
    }
}

impl<V: View> Guest<V> {
    pub fn init(&mut self) {
        self.view = Some(V::init(&mut self.cx));
    }

    pub fn render(&mut self) -> u64 {
        let view = self.view.as_mut().expect("render before init");
        self.cx.per_render.clear();
        self.cx.rendering = true;
        let element = view.render(&mut self.cx);
        self.cx.rendering = false;
        self.last = serde_json::to_vec(&element.into_description()).unwrap_or_default();
        abi::pack(self.last.as_ptr() as u32, self.last.len() as u32)
    }

    pub fn dispatch(&mut self, callback: u32, payload: Vec<u8>) -> u32 {
        let event = Event(serde_json::from_slice(&payload).unwrap_or(Value::Null));
        let handler = if callback & PERSISTENT != 0 {
            self.cx.persistent.get((callback & !PERSISTENT) as usize)
        } else {
            self.cx.per_render.get(callback as usize)
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
        pub extern "C" fn gpui_shell_render() -> u64 {
            GUEST.with(|guest| guest.borrow_mut().render())
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
        guest.render();
        let description: Description = serde_json::from_slice(&guest.last).unwrap();
        let root = &description.nodes[description.root as usize];
        assert_eq!(root.ops, [Op::Callback("on_click".into(), 0)]);
        assert_eq!(guest.dispatch(0, b"null".to_vec()), 1);
        assert_eq!(guest.view.as_ref().unwrap().count, 1);
        // A number this render never handed out is ignored.
        assert_eq!(guest.dispatch(7, b"null".to_vec()), 0);
    }
}
