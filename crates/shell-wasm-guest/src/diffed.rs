//! The retained protocol for a view written as a GPUI view is: a struct
//! whose `render` runs again after every `notify` and returns the whole
//! element tree ([`crate::View`], [`crate::export_view_retained!`]).
//!
//! The guest compares each render with the previous one, by position, and
//! sends the host only the operations that turn the old tree into the new:
//! changed props and texts, added, removed and recreated nodes. A node keeps
//! its number while the element in its position keeps its kind. Handlers
//! are the render's own, so after each render the table from a node and
//! event to the render's callback is rebuilt; no operation crosses for that.
//!
//! Only part 0 (the view itself) is drawn this way, and slots, nested
//! parts, docks and lists are not supported.

use std::collections::HashMap;

use crate::abi::{
    self, Component, Description, Op,
    retained::{Kind, Mutation, Prop},
};

/// One node of the last render, with the number the host knows it by.
struct Retained {
    id: u64,
    kind: Kind,
    props: Vec<(String, Prop)>,
    listeners: Vec<String>,
    children: Vec<Retained>,
}

/// One node of a fresh render, before it is matched against the last.
struct Shape {
    kind: Kind,
    props: Vec<(String, Prop)>,
    listeners: Vec<(String, u32)>,
    children: Vec<Shape>,
}

/// Turns renders into batches.
#[derive(Default)]
pub struct Differ {
    root: Option<Retained>,
    next_id: u64,
    /// The current render's callback for each node's event.
    handlers: HashMap<(u64, String), u32>,
    batch: Vec<Mutation>,
}

impl Differ {
    /// The callback a node's event reaches in the latest render.
    pub fn handler(&self, node: u64, event: &str) -> Option<u32> {
        self.handlers.get(&(node, event.to_owned())).copied()
    }

    /// Compares `description` with the last render and queues the
    /// operations between them.
    pub fn update(&mut self, description: &Description) {
        let shape = shape(description, description.root);
        self.handlers.clear();
        let old = self.root.take();
        let old_id = old.as_ref().map(|old| old.id);
        let root = self.reconcile(old, shape);
        if old_id != Some(root.id) {
            self.batch.push(Mutation::SetRoot {
                part: 0,
                id: root.id,
            });
            if let Some(old_id) = old_id {
                self.batch.push(Mutation::Dispose { id: old_id });
            }
        }
        self.root = Some(root);
    }

    /// The operations queued since the last call.
    pub fn take(&mut self) -> Vec<Mutation> {
        std::mem::take(&mut self.batch)
    }

    fn create(&mut self, shape: Shape) -> Retained {
        self.next_id += 1;
        let id = self.next_id;
        self.batch.push(Mutation::Create {
            id,
            kind: shape.kind.clone(),
        });
        for (key, prop) in &shape.props {
            self.batch.push(Mutation::SetProp {
                id,
                key: key.clone(),
                value: Some(prop.clone()),
            });
        }
        let mut listeners = Vec::new();
        for (event, callback) in shape.listeners {
            self.batch.push(Mutation::Listen {
                id,
                event: event.clone(),
            });
            self.handlers.insert((id, event.clone()), callback);
            listeners.push(event);
        }
        let mut children = Vec::new();
        for child in shape.children {
            let child = self.create(child);
            self.batch.push(Mutation::Insert {
                parent: id,
                child: child.id,
                before: None,
            });
            children.push(child);
        }
        Retained {
            id,
            kind: shape.kind,
            props: shape.props,
            listeners,
            children,
        }
    }

    /// The node for `shape`: `old` updated in place when it is the same kind
    /// of element, a new one otherwise (the caller disposes `old`).
    fn reconcile(&mut self, old: Option<Retained>, shape: Shape) -> Retained {
        let Some(mut old) = old.filter(|old| same_kind(&old.kind, &shape.kind)) else {
            return self.create(shape);
        };
        let id = old.id;
        if old.kind != shape.kind {
            if let Kind::Text(text) = &shape.kind {
                self.batch.push(Mutation::SetText {
                    id,
                    text: text.clone(),
                });
            }
            old.kind = shape.kind;
        }
        for (key, prop) in &shape.props {
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
            if !shape.props.iter().any(|(new_key, _)| new_key == key) {
                self.batch.push(Mutation::SetProp {
                    id,
                    key: key.clone(),
                    value: None,
                });
            }
        }
        old.props = shape.props;

        let mut listeners = Vec::new();
        for (event, callback) in shape.listeners {
            if !old.listeners.contains(&event) {
                self.batch.push(Mutation::Listen {
                    id,
                    event: event.clone(),
                });
            }
            self.handlers.insert((id, event.clone()), callback);
            listeners.push(event);
        }
        for event in &old.listeners {
            if !listeners.contains(event) {
                self.batch.push(Mutation::Unlisten {
                    id,
                    event: event.clone(),
                });
            }
        }
        old.listeners = listeners;

        let before: Vec<u64> = old.children.iter().map(|child| child.id).collect();
        let mut olds = std::mem::take(&mut old.children).into_iter();
        let mut children = Vec::new();
        for shape in shape.children {
            let previous = olds.next();
            let previous_id = previous.as_ref().map(|previous| previous.id);
            let child = self.reconcile(previous, shape);
            if let Some(previous_id) = previous_id
                && previous_id != child.id
            {
                self.batch.push(Mutation::Dispose { id: previous_id });
            }
            children.push(child);
        }
        for left in olds {
            self.batch.push(Mutation::Dispose { id: left.id });
        }
        let after: Vec<u64> = children.iter().map(|child| child.id).collect();
        // From the first position that changed on, every child is inserted
        // again in order: a kept child moves, a new one is added.
        let same = before
            .iter()
            .zip(&after)
            .take_while(|(old, new)| old == new)
            .count();
        for child in &after[same..] {
            self.batch.push(Mutation::Insert {
                parent: id,
                child: *child,
                before: None,
            });
        }
        old.children = children;
        old
    }
}

/// Whether a node of `old` can become one of `new` in place: the same
/// variant, and for anything but text the same identity (element id,
/// input, constructor arguments).
fn same_kind(old: &Kind, new: &Kind) -> bool {
    match (old, new) {
        (Kind::Text(_), Kind::Text(_)) => true,
        (old, new) => old == new,
    }
}

fn shape(description: &Description, index: u32) -> Shape {
    let node = &description.nodes[index as usize];
    let kind = match &node.component {
        Component::Div => Kind::Div,
        Component::HFlex => Kind::HFlex,
        Component::VFlex => Kind::VFlex,
        Component::Text(text) => Kind::Text(text.clone()),
        Component::Button(id) => Kind::Button(id.clone()),
        Component::Checkbox(id) => Kind::Checkbox(id.clone()),
        Component::Input(handle) => Kind::Input(*handle),
        Component::Registered { export, args } => Kind::Registered {
            export: export.clone(),
            args: args.clone(),
        },
        other => panic!("a retained view written as a GPUI view cannot draw {other:?} yet"),
    };
    let mut props = Vec::new();
    let mut listeners = Vec::new();
    for op in &node.ops {
        match op {
            Op::Callback(event, callback) => listeners.push((event.clone(), *callback)),
            other => {
                let (key, prop) = prop(description, other);
                props.push((key, prop));
            }
        }
    }
    let children = node
        .children
        .iter()
        .map(|child| shape(description, *child))
        .collect();
    Shape {
        kind,
        props,
        listeners,
        children,
    }
}

fn prop(description: &Description, op: &Op) -> (String, Prop) {
    match op {
        Op::Style(name) => (name.clone(), Prop::Style(name.clone())),
        Op::ParamStyle(name, args) => (name.clone(), Prop::ParamStyle(name.clone(), args.clone())),
        Op::Method(name, args) => (name.clone(), Prop::Method(name.clone(), args.clone())),
        Op::Component(name, args) => (name.clone(), Prop::Component(name.clone(), args.clone())),
        Op::StateStyle(name, index) => {
            let styles = description.nodes[*index as usize]
                .ops
                .iter()
                .map(|op| prop(description, op).1)
                .collect();
            (name.clone(), Prop::StateStyle(name.clone(), styles))
        }
        Op::Slot(..) | Op::Callback(..) => {
            panic!("a retained view written as a GPUI view cannot use {op:?} yet")
        }
    }
}

/// Encodes a batch for the host, keeping the buffer alive until the next.
pub fn pack(batch: &[Mutation], last: &mut Vec<u8>) -> u64 {
    *last = postcard::to_allocvec(batch).expect("a batch encodes");
    abi::pack(last.as_ptr() as u32, last.len() as u32)
}
