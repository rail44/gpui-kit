//! Writing a view reactively, for the retained protocol
//! (`docs/wasm-retained-guest.md`).
//!
//! A view is a function that runs once and builds nodes. State is in
//! signals; a value that changes is passed as a closure, and only the props
//! and texts that read a changed signal are sent again, as operations on the
//! host's tree. Lists are [`Node::each`], keyed.
//!
//! ```ignore
//! use gpui_shell_wasm_guest::reactive::*;
//!
//! fn counter() -> Node {
//!     let count = create_signal(0);
//!     h_flex()
//!         .child(text(move || format!("Count: {}", count.get())))
//!         .child(button("inc").child(text("+1")).on_click(move |_| count.set(count.get() + 1)))
//! }
//!
//! export_reactive!(counter);
//! ```

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    hash::Hash,
    rc::Rc,
};

pub use sycamore_reactive::{
    MaybeDyn, ReadSignal, Signal, batch, create_effect, create_memo, create_signal, on_cleanup,
    untrack,
};
use sycamore_reactive::{RootHandle, create_root, map_keyed};

pub use crate::{CArg, Event};
use crate::{
    abi::{
        self, Arg, Name,
        retained::{Kind, Mutation, Prop, target},
    },
    host, plain_argument, take_string,
};

/// A string a prop or text takes: fixed, or a closure read again whenever
/// the signals it read change.
pub type Str = MaybeDyn<Cow<'static, str>>;

type Handler = Rc<RefCell<dyn FnMut(&Event)>>;

thread_local! {
    static QUEUE: RefCell<Vec<Mutation>> = const { RefCell::new(Vec::new()) };
    static NEXT_NODE: Cell<u64> = const { Cell::new(1) };
    static NODE_HANDLERS: RefCell<HashMap<(u64, String), Handler>> = RefCell::new(HashMap::new());
    static TOKEN_HANDLERS: RefCell<HashMap<u32, Handler>> = RefCell::new(HashMap::new());
    static NEXT_TOKEN: Cell<u32> = const { Cell::new(0) };
    static ROOT: Cell<Option<RootHandle>> = const { Cell::new(None) };
    /// The last batch handed to the host, kept until the next so the
    /// pointer stays valid while the host reads it.
    static LAST: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn emit(mutation: Mutation) {
    QUEUE.with(|queue| queue.borrow_mut().push(mutation));
}

/// One node of the host's tree, by the number this guest gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node(u64);

fn create(kind: Kind) -> Node {
    let id = NEXT_NODE.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    emit(Mutation::Create { id, kind });
    Node(id)
}

pub fn div() -> Node {
    create(Kind::Div)
}

pub fn h_flex() -> Node {
    create(Kind::HFlex)
}

pub fn v_flex() -> Node {
    create(Kind::VFlex)
}

pub fn button(id: impl Into<String>) -> Node {
    create(Kind::Button(id.into()))
}

pub fn checkbox(id: impl Into<String>) -> Node {
    create(Kind::Checkbox(id.into()))
}

/// A registered component, by its constructor's export name.
pub fn component(export: &str, args: impl IntoIterator<Item = CArg>) -> Node {
    create(Kind::Registered {
        export: export.to_owned(),
        args: args.into_iter().map(plain_argument).collect(),
    })
}

/// A text node; a closure keeps it up to date.
pub fn text(value: impl Into<Str>) -> Node {
    let value = value.into();
    match value.as_static() {
        Some(text) => create(Kind::Text(text.to_string())),
        None => {
            let node = create(Kind::Text(String::new()));
            create_effect(move || {
                emit(Mutation::SetText {
                    id: node.0,
                    text: value.get_clone().into_owned(),
                })
            });
            node
        }
    }
}

/// An input drawing `state`.
pub fn input(state: &InputState) -> Node {
    create(Kind::Input(state.0))
}

impl Node {
    /// Sets the prop under `key` from `value`, again whenever a dynamic
    /// `value` changes.
    fn bind<T>(self, key: &str, value: MaybeDyn<T>, make: impl Fn(T) -> Prop + 'static) -> Self
    where
        T: Clone + Into<MaybeDyn<T>> + 'static,
    {
        let id = self.0;
        let key = key.to_owned();
        match value.as_static() {
            Some(value) => emit(Mutation::SetProp {
                id,
                key: key.into(),
                value: Some(make(value.clone())),
            }),
            None => create_effect(move || {
                emit(Mutation::SetProp {
                    id,
                    key: key.clone().into(),
                    value: Some(make(value.get_clone())),
                })
            }),
        }
        self
    }

    /// A no-argument style (`size_full`, `items_center`, ...).
    pub fn style(self, name: &str) -> Self {
        emit(Mutation::SetProp {
            id: self.0,
            key: Name::from(name.to_owned()),
            value: Some(Prop::Style(Name::from(name.to_owned()))),
        });
        self
    }

    /// A style taking one number, in pixels.
    pub fn style_number(self, name: &'static str, value: impl Into<MaybeDyn<f64>>) -> Self {
        self.bind(name, value.into(), move |value| {
            Prop::ParamStyle(Name::from(name), vec![Arg::Number(value)])
        })
    }

    /// A style taking one string: a color token or `#rrggbb`, a family.
    pub fn style_str(self, name: &'static str, value: impl Into<Str>) -> Self {
        self.bind(name, value.into(), move |value| {
            Prop::ParamStyle(Name::from(name), vec![Arg::Str(value)])
        })
    }

    /// A style taking one string that is there only while `value` gives one:
    /// `None` removes it. A removed style set again comes after the node's
    /// other props.
    pub fn style_str_opt(
        self,
        name: &'static str,
        value: impl Fn() -> Option<&'static str> + 'static,
    ) -> Self {
        let id = self.0;
        create_effect(move || {
            emit(Mutation::SetProp {
                id,
                key: Name::from(name.to_owned()),
                value: value().map(|value| {
                    Prop::ParamStyle(Name::from(name), vec![Arg::Str(Name::from(value))])
                }),
            })
        });
        self
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
    pub fn gap(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("gap", px)
    }
    pub fn p(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("p", px)
    }
    pub fn px(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("px", px)
    }
    pub fn py(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("py", px)
    }
    pub fn h(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("h", px)
    }
    pub fn rounded(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("rounded", px)
    }
    pub fn border(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("border", px)
    }
    pub fn text_size(self, px: impl Into<MaybeDyn<f64>>) -> Self {
        self.style_number("text_size", px)
    }
    pub fn bg(self, color: impl Into<Str>) -> Self {
        self.style_str("bg", color)
    }
    pub fn text_color(self, color: impl Into<Str>) -> Self {
        self.style_str("text_color", color)
    }
    pub fn border_color(self, color: impl Into<Str>) -> Self {
        self.style_str("border_color", color)
    }

    /// A checkbox's state.
    pub fn checked(self, value: impl Into<MaybeDyn<bool>>) -> Self {
        self.bind("checked", value.into(), |value| {
            Prop::Method(Name::from("checked"), vec![Arg::Bool(value)])
        })
    }

    /// A method a registered component declares, with fixed arguments.
    pub fn call(self, method: &str, args: impl IntoIterator<Item = CArg>) -> Self {
        emit(Mutation::SetProp {
            id: self.0,
            key: Name::from(method.to_owned()),
            value: Some(Prop::Component(
                Name::from(method.to_owned()),
                args.into_iter().map(plain_argument).collect(),
            )),
        });
        self
    }

    /// A method a registered component declares, with arguments read again
    /// whenever the signals `args` reads change.
    pub fn call_with(self, method: &str, args: impl Fn() -> Vec<CArg> + 'static) -> Self {
        let id = self.0;
        let method = method.to_owned();
        create_effect(move || {
            emit(Mutation::SetProp {
                id,
                key: method.clone().into(),
                value: Some(Prop::Component(
                    method.clone().into(),
                    args().into_iter().map(plain_argument).collect(),
                )),
            })
        });
        self
    }

    /// Styles applied while the pointer is over the node.
    pub fn hover(self, styles: impl FnOnce(Styles) -> Styles) -> Self {
        emit(Mutation::SetProp {
            id: self.0,
            key: Name::from("hover"),
            value: Some(Prop::StateStyle(
                Name::from("hover"),
                styles(Styles(Vec::new())).0,
            )),
        });
        self
    }

    pub fn child(self, child: Node) -> Self {
        emit(Mutation::Insert {
            parent: self.0,
            child: child.0,
            before: None,
        });
        self
    }

    pub fn children(self, children: impl IntoIterator<Item = Node>) -> Self {
        children.into_iter().fold(self, Node::child)
    }

    /// Makes the node's children one row per item of `items`, identified by
    /// `key`: a row is built once per key, moved when its item moves, and
    /// disposed (with everything it registered) when its item goes.
    pub fn each<T, K>(
        self,
        items: impl Into<MaybeDyn<Vec<T>>> + 'static,
        key: impl Fn(&T) -> K + 'static,
        mut row: impl FnMut(T) -> Node + 'static,
    ) -> Self
    where
        T: PartialEq + Clone + 'static,
        K: Eq + Hash,
    {
        let rows = map_keyed(
            items,
            move |item| {
                let node = row(item);
                on_cleanup(move || emit(Mutation::Dispose { id: node.0 }));
                node
            },
            key,
        );
        let parent = self.0;
        let mut shown: Vec<Node> = Vec::new();
        create_effect(move || {
            let now = rows.get_clone();
            // Everything from the first difference on is inserted again, in
            // order: an attached row moves, a new one is added. A removed
            // row was disposed by its scope.
            let same = shown
                .iter()
                .zip(&now)
                .take_while(|(old, new)| old == new)
                .count();
            if same == now.len() && shown.len() == now.len() {
                return;
            }
            for node in &now[same..] {
                emit(Mutation::Insert {
                    parent,
                    child: node.0,
                    before: None,
                });
            }
            shown = now;
        });
        self
    }

    fn listen(self, event: &str, handler: impl FnMut(&Event) + 'static) -> Self {
        let key = (self.0, event.to_owned());
        let handler: Handler = Rc::new(RefCell::new(handler));
        NODE_HANDLERS.with(|handlers| handlers.borrow_mut().insert(key.clone(), handler));
        on_cleanup(move || {
            NODE_HANDLERS.with(|handlers| handlers.borrow_mut().remove(&key));
        });
        emit(Mutation::Listen {
            id: self.0,
            event: Name::from(event.to_owned()),
        });
        self
    }

    pub fn on_click(self, handler: impl FnMut(&Event) + 'static) -> Self {
        self.listen("on_click", handler)
    }

    /// A checkbox's change; the event is the requested state.
    pub fn on_change(self, handler: impl FnMut(&Event) + 'static) -> Self {
        self.listen("on_change", handler)
    }
}

/// The styles of a state style ([`Node::hover`]).
pub struct Styles(Vec<Prop>);

impl Styles {
    pub fn bg(mut self, color: &str) -> Self {
        self.0.push(Prop::ParamStyle(
            Name::from("bg"),
            vec![Arg::Str(Name::from(color.to_owned()))],
        ));
        self
    }

    pub fn text_color(mut self, color: &str) -> Self {
        self.0.push(Prop::ParamStyle(
            Name::from("text_color"),
            vec![Arg::Str(Name::from(color.to_owned()))],
        ));
        self
    }
}

/// A handler the host reaches through an import's callback id rather than a
/// node: an input's submit, a stream, a response.
fn register_token(handler: impl FnMut(&Event) + 'static) -> u32 {
    let token = NEXT_TOKEN.with(|next| {
        let token = next.get();
        next.set(token + 1);
        token
    });
    let handler: Handler = Rc::new(RefCell::new(handler));
    TOKEN_HANDLERS.with(|handlers| handlers.borrow_mut().insert(token, handler));
    on_cleanup(move || {
        TOKEN_HANDLERS.with(|handlers| handlers.borrow_mut().remove(&token));
    });
    token
}

/// A retained text input.
#[derive(Clone, Copy)]
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

    /// Enter in the input.
    pub fn on_submit(&self, handler: impl FnMut(&Event) + 'static) {
        let token = register_token(handler);
        unsafe { host::input_on_submit(self.0, token) }
    }
}

/// Hands the host the operations queued since the last call.
fn flush() -> u64 {
    let batch = QUEUE.with(|queue| std::mem::take(&mut *queue.borrow_mut()));
    let bytes = postcard::to_allocvec(&batch).expect("a batch encodes");
    LAST.with(|last| {
        let mut last = last.borrow_mut();
        *last = bytes;
        abi::pack(last.as_ptr() as u32, last.len() as u32)
    })
}

/// Builds the view under a reactive root and returns the first batch.
/// Backs [`export_reactive!`]; not for direct use.
#[doc(hidden)]
pub fn mount(view: impl FnOnce() -> Node) -> u64 {
    let root = create_root(|| {
        let node = view();
        emit(Mutation::SetRoot {
            part: 0,
            id: node.0,
        });
    });
    ROOT.with(|slot| slot.set(Some(root)));
    flush()
}

/// Runs the handler an event is addressed to and returns its batch. Backs
/// [`export_reactive!`]; not for direct use.
#[doc(hidden)]
pub fn event(kind: u32, target: u64, name: Vec<u8>, payload: Vec<u8>) -> u64 {
    let event = Event(serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null));
    let handler = match kind {
        target::NODE => {
            let key = (target, String::from_utf8(name).unwrap_or_default());
            NODE_HANDLERS.with(|handlers| handlers.borrow().get(&key).cloned())
        }
        target::HANDLER => {
            TOKEN_HANDLERS.with(|handlers| handlers.borrow().get(&(target as u32)).cloned())
        }
        _ => None,
    };
    // A handler for a node disposed since the host saw it has gone; the
    // event is dropped.
    if let (Some(handler), Some(root)) = (handler, ROOT.with(Cell::get)) {
        root.run_in(|| (handler.borrow_mut())(&event));
    }
    flush()
}

/// Generates the retained protocol's exports for a view function.
#[macro_export]
macro_rules! export_reactive {
    ($view:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_shell_alloc(len: u32) -> u32 {
            let mut buffer = ::std::vec::Vec::<u8>::with_capacity(len as usize);
            let ptr = buffer.as_mut_ptr();
            ::std::mem::forget(buffer);
            ptr as u32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn gpui_retained_init() -> u64 {
            $crate::reactive::mount($view)
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
            $crate::reactive::event(kind, target, name, payload)
        }
    };
}
