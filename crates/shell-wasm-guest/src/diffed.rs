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
//! Only part 0 (the view itself) is drawn this way, and slots, nested parts,
//! docks and lists are not supported.

use std::collections::HashMap;

use crate::{
    CArg, Element, Kind as ElementKind, PendingOp,
    abi::{
        self, Component, Name, Op,
        retained::{Kind, Mutation, Prop},
    },
    plain_argument,
};

/// One node of the last render, with the number the host knows it by.
struct Retained {
    id: u64,
    kind: Kind,
    props: Vec<(Name, Prop)>,
    listeners: Vec<(Name, u32)>,
    children: Vec<Retained>,
}

/// Turns renders into batches.
#[derive(Default)]
pub struct Differ {
    root: Option<Retained>,
    next_id: u64,
    /// The latest render's callback for each node's event.
    handlers: HashMap<u64, Vec<(Name, u32)>>,
    batch: Vec<Mutation>,
}

impl Differ {
    /// The callback a node's event reaches in the latest render.
    pub fn handler(&self, node: u64, event: &str) -> Option<u32> {
        self.handlers
            .get(&node)?
            .iter()
            .find_map(|(listened, callback)| (listened == event).then_some(*callback))
    }

    /// Compares a render with the last and queues the operations between
    /// them.
    pub fn update(&mut self, element: Element) {
        let old = self.root.take();
        let old_id = old.as_ref().map(|old| old.id);
        let root = self.reconcile(old, element);
        if old_id != Some(root.id) {
            self.batch.push(Mutation::SetRoot {
                part: 0,
                id: root.id,
            });
        }
        self.root = Some(root);
    }

    /// The operations queued since the last call.
    pub fn take(&mut self) -> Vec<Mutation> {
        std::mem::take(&mut self.batch)
    }

    /// Disposes `old`'s whole subtree and forgets its handlers.
    fn dispose(&mut self, old: Retained) {
        self.batch.push(Mutation::Dispose { id: old.id });
        let mut pending = vec![old];
        while let Some(node) = pending.pop() {
            self.handlers.remove(&node.id);
            pending.extend(node.children);
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
            self.handlers.insert(id, listeners.clone());
        }
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let child = self.create(child);
            self.batch.push(Mutation::Insert {
                parent: id,
                child: child.id,
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

    /// The node for `element`: `old` updated in place when it is the same
    /// kind of element, a new one otherwise (with `old` disposed).
    fn reconcile(&mut self, old: Option<Retained>, element: Element) -> Retained {
        let same = old
            .as_ref()
            .is_some_and(|old| same_kind(&old.kind, &element));
        let Some(mut old) = old.filter(|_| same) else {
            let created = self.create(element);
            return created;
        };
        let id = old.id;
        let (kind, props, listeners, children) = parts(element);
        if old.kind != kind {
            if let Kind::Text(text) = &kind {
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
                self.handlers.insert(id, listeners.clone());
            }
            old.listeners = listeners;
        }

        // Children, by position.
        let before: Vec<u64> = old.children.iter().map(|child| child.id).collect();
        let mut olds = std::mem::take(&mut old.children).into_iter();
        let mut kept = Vec::with_capacity(children.len());
        for child in children {
            let previous = olds.next();
            let previous_id = previous.as_ref().map(|previous| previous.id);
            let reused = previous
                .as_ref()
                .is_some_and(|previous| same_kind(&previous.kind, &child));
            let node = if reused {
                self.reconcile(previous, child)
            } else {
                if let Some(previous) = previous {
                    self.dispose(previous);
                }
                self.create(child)
            };
            debug_assert!(!reused || previous_id == Some(node.id));
            kept.push(node);
        }
        for left in olds {
            self.dispose(left);
        }
        // From the first position that changed on, every child is inserted
        // again in order: a kept child moves, a new one is added.
        let same_prefix = before
            .iter()
            .zip(&kept)
            .take_while(|(old, new)| **old == new.id)
            .count();
        if same_prefix < kept.len() || before.len() != kept.len() {
            for child in &kept[same_prefix..] {
                self.batch.push(Mutation::Insert {
                    parent: id,
                    child: child.id,
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
            Kind::Registered { export, args },
            ElementKind::Registered {
                export: new_export,
                args: new_args,
            },
        ) => {
            export == new_export && args.len() == new_args.len() && {
                args.iter()
                    .zip(new_args)
                    .all(|(old, new)| *old == plain_argument(clone_argument(new)))
            }
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
            other => panic!("a retained view written as a GPUI view cannot draw {other:?} yet"),
        },
        ElementKind::Registered { export, args } => Kind::Registered {
            export,
            args: args.into_iter().map(plain_argument).collect(),
        },
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

/// Encodes a batch for the host, keeping the buffer alive until the next.
pub fn pack(batch: &[Mutation], last: &mut Vec<u8>) -> u64 {
    *last = postcard::to_allocvec(batch).expect("a batch encodes");
    abi::pack(last.as_ptr() as u32, last.len() as u32)
}
