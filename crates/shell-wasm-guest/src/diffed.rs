//! The retained protocol for a view written as a GPUI view is: a struct
//! whose `render` runs again after every `notify` and returns the whole
//! element tree ([`crate::View`], [`crate::export_view_retained!`]).
//!
//! The guest compares each render with the previous one, by position, and
//! sends the host only the operations that turn the old tree into the new:
//! changed props and texts, added, removed and recreated nodes. A node keeps
//! its number while the element in its position keeps its kind.
//!
//! The comparison walks the render's own element tree and takes its values
//! rather than copying them: a render's strings move into the kept tree, and
//! one is cloned only when it changed and has to cross. Props are compared
//! in order first, which is the common case for a render of the same code.
//! Handlers are the render's own callbacks, numbered in the order the render
//! registered them, so a render of the same shape reuses the same numbers
//! and the table from a node's event to its callback is touched only where
//! one moved.
//!
//! # Child views
//!
//! A view splits itself the way a GPUI view does, into child views
//! ([`Child`], made with `cx.new`) that each hold their own state and render
//! their own subtree. A child renders again only when it notified
//! (`child.update(cx, |child, cx| { ...; cx.notify() })`, or a handler of
//! its own); a parent that renders again keeps every child it places where
//! it was, untouched. A child tells its parent something happened with
//! `cx.emit(event)`, which the parent hears through `cx.subscribe`.
//!
//! Only part 0 (the view itself) is drawn this way, and slots, nested parts,
//! docks and lists are not supported. A child view is placed inside an
//! element (not returned as a render's root), handlers a child registers
//! through a host import (an input's submit) are not routed to it, and only
//! the guest's own view can subscribe to a child.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use crate::{
    CArg, Cx, Element, Event, Kind as ElementKind, PendingOp,
    abi::{
        Component, Name, Op,
        retained::{Kind, Mutation, Prop},
    },
    plain_argument,
};

/// A child view: what a GPUI view's `Render` is. See the module docs.
pub trait Render: Sized + 'static {
    fn render(&mut self, cx: &mut Cx<Self>) -> Element;
}

/// What a view does with something a child view emitted.
pub(crate) type Subscriber<V> = Rc<dyn Fn(&mut V, &dyn Any, &mut Cx<V>)>;

thread_local! {
    static CHILDREN: RefCell<HashMap<u32, Weak<dyn ChildDyn>>> = RefCell::new(HashMap::new());
    static NEXT_CHILD: Cell<u32> = const { Cell::new(1) };
    /// Child views that notified and have not rendered since.
    static DIRTY: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// What child views emitted, not yet handed to subscribers.
    static EMITTED: RefCell<Vec<(u32, Box<dyn Any>)>> = const { RefCell::new(Vec::new()) };
}

fn child(key: u32) -> Option<Rc<dyn ChildDyn>> {
    CHILDREN.with(|children| children.borrow().get(&key).and_then(Weak::upgrade))
}

pub(crate) fn is_alive(key: u32) -> bool {
    child(key).is_some()
}

pub(crate) fn take_emitted() -> Vec<(u32, Box<dyn Any>)> {
    EMITTED.with(|emitted| std::mem::take(&mut *emitted.borrow_mut()))
}

/// Runs a child view's handler.
pub(crate) fn dispatch_child(key: u32, callback: u32, payload: Vec<u8>) {
    let event = Event(serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null));
    if let Some(child) = child(key) {
        child.dispatch(callback, &event);
    }
}

/// A child view's handle, as a GPUI `Entity` is: cloning it shares the view.
pub struct Child<C: Render>(Rc<ChildCell<C>>);

impl<C: Render> Clone for Child<C> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<C: Render> Child<C> {
    /// The element that draws this child where it is placed.
    pub fn element(&self) -> Element {
        Element {
            kind: ElementKind::Child(self.0.clone()),
            ops: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Reads the child's state.
    pub fn read<R>(&self, read: impl FnOnce(&C) -> R) -> R {
        read(&self.0.state.borrow().0)
    }

    /// Changes the child's state. It renders again if `update` notifies.
    pub fn update<V, R>(&self, _cx: &mut Cx<V>, update: impl FnOnce(&mut C, &mut Cx<C>) -> R) -> R {
        let result = {
            let mut state = self.0.state.borrow_mut();
            let (view, cx) = &mut *state;
            update(view, cx)
        };
        self.0.after_handler();
        result
    }
}

impl<C: Render> From<&Child<C>> for Element {
    fn from(child: &Child<C>) -> Self {
        child.element()
    }
}

impl<V: 'static> Cx<V> {
    /// Makes a child view, as GPUI's `cx.new` makes an entity. Keep the
    /// handle in the view's state and place it from `render`.
    // Named for GPUI's `cx.new`, which returns the entity, not the context.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<C: Render>(&mut self, build: impl FnOnce(&mut Cx<C>) -> C) -> Child<C> {
        let key = NEXT_CHILD.with(|next| {
            let key = next.get();
            next.set(key + 1);
            key
        });
        let mut cx = Cx::fresh();
        cx.owner = key;
        let view = build(&mut cx);
        let cell = Rc::new(ChildCell {
            key,
            state: RefCell::new((view, cx)),
            dirty: Cell::new(false),
            root: RefCell::new(None),
            parent: Cell::new(None),
            claimed: Cell::new(0),
        });
        let weak: Weak<dyn ChildDyn> = Rc::downgrade(&(cell.clone() as Rc<dyn ChildDyn>));
        CHILDREN.with(|children| children.borrow_mut().insert(key, weak));
        Child(cell)
    }

    /// Tells whoever subscribed to this view that something happened.
    pub fn emit<E: 'static>(&mut self, event: E) {
        let owner = self.owner;
        EMITTED.with(|emitted| emitted.borrow_mut().push((owner, Box::new(event))));
    }

    /// Hears what `child` emits, for as long as `child` lives.
    pub fn subscribe<C: Render, E: 'static>(
        &mut self,
        child: &Child<C>,
        on_event: impl Fn(&mut V, &E, &mut Cx<V>) + 'static,
    ) {
        self.subscriptions.push((
            child.0.key,
            Rc::new(move |view, event, cx| {
                if let Some(event) = event.downcast_ref::<E>() {
                    on_event(view, event, cx);
                }
            }),
        ));
    }
}

struct ChildCell<C> {
    key: u32,
    state: RefCell<(C, Cx<C>)>,
    dirty: Cell<bool>,
    /// The child's last render, while it is placed somewhere.
    root: RefCell<Option<Retained>>,
    /// The node the child's root is a child of.
    parent: Cell<Option<u64>>,
    /// The comparison pass that last placed the child, so a pass that finds
    /// the child's old place afterwards knows it moved rather than went.
    claimed: Cell<u64>,
}

impl<C> ChildCell<C> {
    /// Queues a render if the code that just ran notified.
    fn after_handler(&self) {
        let notified = std::mem::take(&mut self.state.borrow_mut().1.notify);
        if notified && !self.dirty.replace(true) {
            DIRTY.with(|dirty| dirty.borrow_mut().push(self.key));
        }
    }
}

impl<C> Drop for ChildCell<C> {
    fn drop(&mut self) {
        let _ = CHILDREN.try_with(|children| children.borrow_mut().remove(&self.key));
    }
}

/// A child view of any type, as the comparison handles it.
pub(crate) trait ChildDyn {
    fn key(&self) -> u32;
    fn root_id(&self) -> Option<u64>;
    fn claimed(&self) -> u64;
    fn is_dirty(&self) -> bool;
    /// Takes the child's last render away: it is no longer placed.
    fn take_root(&self) -> Option<Retained>;
    /// Records where the child is placed, in comparison pass `pass`.
    fn place(&self, parent: u64, pass: u64);
    /// Renders the child if it is new here or notified. `standalone` is a
    /// render outside its parent's: the child keeps its place itself.
    fn sync(&self, differ: &mut Differ, standalone: bool);
    fn dispatch(&self, callback: u32, event: &Event);
}

impl<C: Render> ChildDyn for ChildCell<C> {
    fn key(&self) -> u32 {
        self.key
    }

    fn root_id(&self) -> Option<u64> {
        self.root.borrow().as_ref().map(|root| root.id)
    }

    fn claimed(&self) -> u64 {
        self.claimed.get()
    }

    fn is_dirty(&self) -> bool {
        self.dirty.get()
    }

    fn take_root(&self) -> Option<Retained> {
        self.root.borrow_mut().take()
    }

    fn place(&self, parent: u64, pass: u64) {
        self.parent.set(Some(parent));
        self.claimed.set(pass);
    }

    fn sync(&self, differ: &mut Differ, standalone: bool) {
        let old = self.root.borrow_mut().take();
        if old.is_some() && !self.dirty.get() {
            *self.root.borrow_mut() = old;
            return;
        }
        self.dirty.set(false);
        let element = {
            let mut state = self.state.borrow_mut();
            let (view, cx) = &mut *state;
            cx.per_render[0].clear();
            cx.rendering = Some(0);
            let element = view.render(cx);
            cx.rendering = None;
            element
        };
        let owner = std::mem::replace(&mut differ.owner, self.key);
        let (root, displaced) = differ.root_node(old, element);
        differ.owner = owner;
        if let Some(displaced) = displaced {
            // The root became another kind of element: the new node takes
            // the old one's place, which the parent's own comparison does
            // when this is part of it.
            if standalone && let Some(parent) = self.parent.get() {
                differ.batch.push(Mutation::Insert {
                    parent,
                    child: root.id,
                    before: Some(displaced.id),
                });
            }
            differ.batch.push(Mutation::Dispose { id: displaced.id });
            differ.forget(displaced);
        }
        *self.root.borrow_mut() = Some(root);
    }

    fn dispatch(&self, callback: u32, event: &Event) {
        {
            let mut state = self.state.borrow_mut();
            let (view, cx) = &mut *state;
            let Some(handler) = cx.handler(callback) else {
                return;
            };
            handler(view, event, cx);
        }
        self.after_handler();
    }
}

/// One node of the last render, with the number the host knows it by.
pub(crate) struct Retained {
    id: u64,
    kind: Kind,
    props: Vec<(Name, Prop)>,
    listeners: Vec<(Name, u32)>,
    children: Vec<Kept>,
}

/// What sits in a node's child position: a node, or a child view's root.
enum Kept {
    Node(Retained),
    Child(Rc<dyn ChildDyn>),
}

impl Kept {
    fn id(&self) -> u64 {
        match self {
            Kept::Node(node) => node.id,
            Kept::Child(child) => child.root_id().unwrap_or(0),
        }
    }
}

/// Turns renders into batches.
#[derive(Default)]
pub struct Differ {
    root: Option<Retained>,
    next_id: u64,
    /// For each node with handlers: the view that registered them (0 is the
    /// guest's own, otherwise a child view's key) and its callback for each
    /// event, as of the latest render.
    handlers: HashMap<u64, (u32, Vec<(Name, u32)>)>,
    batch: Vec<Mutation>,
    /// The view whose render is being compared.
    owner: u32,
    /// Counts comparisons, so a child placed in this one is told from one
    /// placed before.
    pass: u64,
}

impl Differ {
    /// The view and callback a node's event reaches in the latest render.
    pub fn handler(&self, node: u64, event: &str) -> Option<(u32, u32)> {
        let (owner, listeners) = self.handlers.get(&node)?;
        listeners
            .iter()
            .find_map(|(listened, callback)| (listened == event).then_some((*owner, *callback)))
    }

    /// Compares the view's render with its last and queues the operations
    /// between them.
    pub fn update(&mut self, element: Element) {
        self.pass += 1;
        let old = self.root.take();
        let first = old.is_none();
        let (root, displaced) = self.root_node(old, element);
        if first || displaced.is_some() {
            self.batch.push(Mutation::SetRoot {
                part: 0,
                id: root.id,
            });
        }
        if let Some(displaced) = displaced {
            self.batch.push(Mutation::Dispose { id: displaced.id });
            self.forget(displaced);
        }
        self.root = Some(root);
    }

    /// Renders every placed child view that notified since its last render.
    pub fn refresh_children(&mut self) {
        loop {
            let dirty = DIRTY.with(|dirty| std::mem::take(&mut *dirty.borrow_mut()));
            if dirty.is_empty() {
                return;
            }
            self.pass += 1;
            for key in dirty {
                // A child that is not placed renders when it is.
                if let Some(child) = child(key)
                    && child.is_dirty()
                    && child.root_id().is_some()
                {
                    child.sync(self, true);
                }
            }
        }
    }

    /// The operations queued since the last call.
    pub fn take(&mut self) -> Vec<Mutation> {
        std::mem::take(&mut self.batch)
    }

    /// The node for a render's root: `old` updated in place when it is the
    /// same kind of element; otherwise a new node, and `old` handed back for
    /// the caller to replace.
    fn root_node(
        &mut self,
        old: Option<Retained>,
        element: Element,
    ) -> (Retained, Option<Retained>) {
        assert!(
            !matches!(element.kind, ElementKind::Child(_)),
            "a render returns an element; place a child view inside one"
        );
        match old {
            Some(old) if same_kind(&old.kind, &element) => (self.reconcile(old, element), None),
            old => (self.create(element), old),
        }
    }

    /// Forgets the handlers of a subtree the host has disposed, and that the
    /// child views inside it are placed.
    fn forget(&mut self, node: Retained) {
        self.handlers.remove(&node.id);
        for child in node.children {
            match child {
                Kept::Node(node) => self.forget(node),
                Kept::Child(child) => {
                    // A child placed elsewhere in this pass was detached
                    // when it was, and lives on.
                    if child.claimed() != self.pass
                        && let Some(root) = child.take_root()
                    {
                        self.forget(root);
                    }
                }
            }
        }
    }

    /// Disposes what was in a position that something else now takes.
    fn dispose(&mut self, kept: Kept) {
        match kept {
            Kept::Node(node) => {
                self.batch.push(Mutation::Dispose { id: node.id });
                self.forget(node);
            }
            Kept::Child(child) => {
                if child.claimed() != self.pass
                    && let Some(root) = child.take_root()
                {
                    self.batch.push(Mutation::Dispose { id: root.id });
                    self.forget(root);
                }
            }
        }
    }

    /// What goes in a child position of node `parent`: `old` kept when the
    /// element is the same child view or the same kind of element.
    fn place(&mut self, old: Option<Kept>, element: Element, parent: u64) -> Kept {
        if matches!(element.kind, ElementKind::Child(_)) {
            let ElementKind::Child(child) = element.kind else {
                unreachable!()
            };
            let same = matches!(&old, Some(Kept::Child(old)) if old.key() == child.key());
            if !same {
                if let Some(old) = old {
                    self.dispose(old);
                }
                // Placed somewhere else until now: detached first, so that
                // disposing its old surroundings cannot take it along.
                if let Some(root) = child.root_id() {
                    self.batch.push(Mutation::Remove { id: root });
                }
            }
            child.place(parent, self.pass);
            child.sync(self, false);
            return Kept::Child(child);
        }
        match old {
            Some(Kept::Node(node)) if same_kind(&node.kind, &element) => {
                Kept::Node(self.reconcile(node, element))
            }
            old => {
                if let Some(old) = old {
                    self.dispose(old);
                }
                Kept::Node(self.create(element))
            }
        }
    }

    fn create(&mut self, element: Element) -> Retained {
        let (kind, props, listeners, children) = parts(element);
        self.next_id += 1;
        let id = self.next_id;
        self.batch.push(Mutation::Create {
            id,
            kind: kind.clone(),
        });
        for (key, prop) in &props {
            self.batch.push(Mutation::SetProp {
                id,
                key: key.clone(),
                value: Some(prop.clone()),
            });
        }
        for (event, _) in &listeners {
            self.batch.push(Mutation::Listen {
                id,
                event: event.clone(),
            });
        }
        if !listeners.is_empty() {
            self.handlers.insert(id, (self.owner, listeners.clone()));
        }
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let child = self.place(None, child, id);
            self.batch.push(Mutation::Insert {
                parent: id,
                child: child.id(),
                before: None,
            });
            kept.push(child);
        }
        Retained {
            id,
            kind,
            props,
            listeners,
            children: kept,
        }
    }

    /// Updates `old`, which is the same kind of element, to `element`.
    fn reconcile(&mut self, mut old: Retained, element: Element) -> Retained {
        let id = old.id;
        let (kind, props, listeners, children) = parts(element);
        if old.kind != kind {
            if let Kind::Text(text) | Kind::TextView { text, .. } = &kind {
                self.batch.push(Mutation::SetText {
                    id,
                    text: text.clone(),
                });
            }
            old.kind = kind;
        }

        // Props: in order when the keys line up, by key otherwise.
        let aligned = old.props.len() == props.len()
            && old
                .props
                .iter()
                .zip(&props)
                .all(|((old_key, _), (key, _))| old_key == key);
        if aligned {
            for ((_, old_prop), (key, prop)) in old.props.iter().zip(&props) {
                if old_prop != prop {
                    self.batch.push(Mutation::SetProp {
                        id,
                        key: key.clone(),
                        value: Some(prop.clone()),
                    });
                }
            }
        } else {
            for (key, prop) in &props {
                let unchanged = old
                    .props
                    .iter()
                    .any(|(old_key, old_prop)| old_key == key && old_prop == prop);
                if !unchanged {
                    self.batch.push(Mutation::SetProp {
                        id,
                        key: key.clone(),
                        value: Some(prop.clone()),
                    });
                }
            }
            for (key, _) in &old.props {
                if !props.iter().any(|(new_key, _)| new_key == key) {
                    self.batch.push(Mutation::SetProp {
                        id,
                        key: key.clone(),
                        value: None,
                    });
                }
            }
        }
        old.props = props;

        // Listeners: marks cross only when an event is added or dropped; the
        // callback table changes only when a number moved.
        if old.listeners != listeners {
            for (event, _) in &listeners {
                if !old
                    .listeners
                    .iter()
                    .any(|(old_event, _)| old_event == event)
                {
                    self.batch.push(Mutation::Listen {
                        id,
                        event: event.clone(),
                    });
                }
            }
            for (event, _) in &old.listeners {
                if !listeners.iter().any(|(new_event, _)| new_event == event) {
                    self.batch.push(Mutation::Unlisten {
                        id,
                        event: event.clone(),
                    });
                }
            }
            if listeners.is_empty() {
                self.handlers.remove(&id);
            } else {
                self.handlers.insert(id, (self.owner, listeners.clone()));
            }
            old.listeners = listeners;
        }

        // Children, by position.
        let before: Vec<u64> = old.children.iter().map(Kept::id).collect();
        let mut olds = std::mem::take(&mut old.children).into_iter();
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let previous = olds.next();
            kept.push(self.place(previous, child, id));
        }
        for left in olds {
            self.dispose(left);
        }
        // From the first position that changed on, every child is inserted
        // again in order: a kept child moves, a new one is added.
        let same_prefix = before
            .iter()
            .zip(&kept)
            .take_while(|(old, new)| **old == new.id())
            .count();
        if same_prefix < kept.len() {
            for child in &kept[same_prefix..] {
                self.batch.push(Mutation::Insert {
                    parent: id,
                    child: child.id(),
                    before: None,
                });
            }
        }
        old.children = kept;
        old
    }
}

/// Whether a node of `old` can become `element` in place: the same variant,
/// and for anything but text the same identity (element id, input,
/// constructor arguments).
fn same_kind(old: &Kind, element: &Element) -> bool {
    match (old, &element.kind) {
        (Kind::Text(_), ElementKind::Base(Component::Text(_))) => true,
        (Kind::Div, ElementKind::Base(Component::Div))
        | (Kind::HFlex, ElementKind::Base(Component::HFlex))
        | (Kind::VFlex, ElementKind::Base(Component::VFlex)) => true,
        (Kind::Button(old), ElementKind::Base(Component::Button(new)))
        | (Kind::Checkbox(old), ElementKind::Base(Component::Checkbox(new))) => old == new,
        (Kind::Input(old), ElementKind::Base(Component::Input(new))) => old == new,
        (
            Kind::TextView { id, markdown, .. },
            ElementKind::Base(Component::TextView {
                id: new_id,
                markdown: new_markdown,
                ..
            }),
        ) => id == new_id && markdown == new_markdown,
        (
            Kind::Registered { export, args },
            ElementKind::Registered {
                export: new_export,
                args: new_args,
            },
        ) => {
            export == new_export
                && args.len() == new_args.len()
                && args
                    .iter()
                    .zip(new_args)
                    .all(|(old, new)| *old == plain_argument(clone_argument(new)))
        }
        _ => false,
    }
}

fn clone_argument(argument: &CArg) -> CArg {
    match argument {
        CArg::String(value) => CArg::String(value.clone()),
        CArg::Number(value) => CArg::Number(*value),
        CArg::Boolean(value) => CArg::Boolean(*value),
        CArg::Enum(value) => CArg::Enum(value.clone()),
        CArg::State(handle) => CArg::State(*handle),
        CArg::Array(values) => CArg::Array(values.iter().map(clone_argument).collect()),
        CArg::Optional(value) => {
            CArg::Optional(value.as_ref().map(|value| Box::new(clone_argument(value))))
        }
        CArg::Element(_) | CArg::Handler(_) => CArg::Optional(None),
    }
}

type Parts = (Kind, Vec<(Name, Prop)>, Vec<(Name, u32)>, Vec<Element>);

/// An element taken apart: its kind, props, listeners and children, its
/// values moved rather than copied.
fn parts(element: Element) -> Parts {
    let kind = match element.kind {
        ElementKind::Base(component) => match component {
            Component::Div => Kind::Div,
            Component::HFlex => Kind::HFlex,
            Component::VFlex => Kind::VFlex,
            Component::Text(text) => Kind::Text(text),
            Component::Button(id) => Kind::Button(id),
            Component::Checkbox(id) => Kind::Checkbox(id),
            Component::Input(handle) => Kind::Input(handle),
            Component::TextView { id, markdown, text } => Kind::TextView { id, markdown, text },
            other => panic!("a retained view written as a GPUI view cannot draw {other:?} yet"),
        },
        ElementKind::Registered { export, args } => Kind::Registered {
            export,
            args: args.into_iter().map(plain_argument).collect(),
        },
        ElementKind::Child(_) => unreachable!("a child view is placed, not taken apart"),
    };
    let mut props = Vec::with_capacity(element.ops.len());
    let mut listeners = Vec::new();
    for op in element.ops {
        match op {
            PendingOp::Op(Op::Callback(event, callback)) => listeners.push((event, callback)),
            PendingOp::Op(op) => props.push(prop(op)),
            PendingOp::Component(name, args) => props.push((
                name.clone(),
                Prop::Component(name, args.into_iter().map(plain_argument).collect()),
            )),
            PendingOp::StateStyle(name, styles) => {
                let styles = styles
                    .ops
                    .into_iter()
                    .filter_map(|op| match op {
                        PendingOp::Op(op) => Some(prop(op).1),
                        _ => None,
                    })
                    .collect();
                props.push((Name::from(name), Prop::StateStyle(Name::from(name), styles)));
            }
            PendingOp::Slot(..) => {
                panic!("a retained view written as a GPUI view cannot use slots yet")
            }
        }
    }
    (kind, props, listeners, element.children)
}

fn prop(op: Op) -> (Name, Prop) {
    match op {
        Op::Style(name) => (name.clone(), Prop::Style(name)),
        Op::ParamStyle(name, args) => (name.clone(), Prop::ParamStyle(name, args)),
        Op::Method(name, args) => (name.clone(), Prop::Method(name, args)),
        Op::Component(name, args) => (name.clone(), Prop::Component(name, args)),
        other => panic!("a retained view written as a GPUI view cannot use {other:?} yet"),
    }
}
