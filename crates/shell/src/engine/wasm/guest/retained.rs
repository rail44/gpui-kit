//! The host's half of the retained protocol (`docs/wasm-retained-guest.md`):
//! the tree a guest's operations build, and its copy into the spec arena
//! when a part is drawn.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, anyhow, bail};
use gpui_shell_wasm_abi::retained::{Batch, Kind, Mutation, Prop};

use super::{
    Guest, ShellRuntime, bridged, handler_name, intern, param_style, registered_method,
    registered_node, state_style_name,
};
use crate::{
    entities::EntityHandle,
    spec::{Component, SpecId, SpecOp},
};

/// The tree one guest's operations built.
#[derive(Default)]
pub(in crate::engine::wasm) struct Tree {
    nodes: HashMap<u64, Node>,
    /// The root of each part the guest drew, by part.
    roots: BTreeMap<u32, u64>,
    /// Why the guest stopped being trusted: a batch that could not be applied
    /// leaves a tree nobody can vouch for, so no later batch is applied.
    broken: Option<String>,
}

struct Node {
    kind: Kind,
    /// One prop per key, in the order the keys were first set.
    props: Vec<(String, Prop)>,
    children: Vec<u64>,
    parent: Option<u64>,
    listeners: Vec<String>,
}

impl Tree {
    /// Applies `batch` in order, and returns the parts whose trees it
    /// touched.
    pub(in crate::engine::wasm) fn apply(&mut self, batch: Batch) -> Result<BTreeSet<u32>> {
        if let Some(reason) = &self.broken {
            bail!("the guest's earlier batch failed ({reason}); its batches are refused");
        }
        let mut touched = Vec::new();
        let mut parts = BTreeSet::new();
        for mutation in batch {
            if let Err(error) = self.apply_one(mutation, &mut touched, &mut parts) {
                self.broken = Some(format!("{error:#}"));
                return Err(error);
            }
        }
        for id in touched {
            if let Some(part) = self.part_of(id) {
                parts.insert(part);
            }
        }
        Ok(parts)
    }

    fn node(&mut self, id: u64) -> Result<&mut Node> {
        self.nodes
            .get_mut(&id)
            .ok_or_else(|| anyhow!("no node {id}"))
    }

    fn apply_one(
        &mut self,
        mutation: Mutation,
        touched: &mut Vec<u64>,
        parts: &mut BTreeSet<u32>,
    ) -> Result<()> {
        match mutation {
            Mutation::Create { id, kind } => {
                if self.nodes.contains_key(&id) {
                    bail!("node {id} already exists");
                }
                self.nodes.insert(
                    id,
                    Node {
                        kind,
                        props: Vec::new(),
                        children: Vec::new(),
                        parent: None,
                        listeners: Vec::new(),
                    },
                );
            }
            Mutation::SetProp { id, key, value } => {
                let node = self.node(id)?;
                match value {
                    Some(prop) => match node.props.iter_mut().find(|(set, _)| *set == key) {
                        Some(slot) => slot.1 = prop,
                        None => node.props.push((key, prop)),
                    },
                    None => node.props.retain(|(set, _)| *set != key),
                }
                touched.push(id);
            }
            Mutation::SetText { id, text } => {
                match &mut self.node(id)?.kind {
                    Kind::Text(value) => *value = text,
                    _ => bail!("node {id} is not text"),
                }
                touched.push(id);
            }
            Mutation::Insert {
                parent,
                child,
                before,
            } => {
                self.node(child)?;
                self.node(parent)?;
                let mut ancestor = Some(parent);
                while let Some(id) = ancestor {
                    if id == child {
                        bail!("node {child} cannot be inserted inside itself");
                    }
                    ancestor = self.nodes.get(&id).and_then(|node| node.parent);
                }
                if let Some(old) = self.detach(child) {
                    touched.push(old);
                }
                let children = &mut self.node(parent)?.children;
                let index = match before {
                    Some(before) => children
                        .iter()
                        .position(|id| *id == before)
                        .ok_or_else(|| anyhow!("node {before} is not a child of {parent}"))?,
                    None => children.len(),
                };
                children.insert(index, child);
                self.node(child)?.parent = Some(parent);
                touched.push(parent);
            }
            Mutation::Remove { id } => {
                self.node(id)?;
                if let Some(old) = self.detach(id) {
                    touched.push(old);
                }
            }
            Mutation::Dispose { id } => {
                self.node(id)?;
                if let Some(old) = self.detach(id) {
                    touched.push(old);
                }
                let mut pending = vec![id];
                while let Some(id) = pending.pop() {
                    if let Some(node) = self.nodes.remove(&id) {
                        pending.extend(node.children);
                    }
                }
                self.roots.retain(|_, root| self.nodes.contains_key(root));
            }
            Mutation::Listen { id, event } => {
                let node = self.node(id)?;
                if !node.listeners.contains(&event) {
                    node.listeners.push(event);
                }
                touched.push(id);
            }
            Mutation::Unlisten { id, event } => {
                self.node(id)?
                    .listeners
                    .retain(|listened| *listened != event);
                touched.push(id);
            }
            Mutation::SetRoot { part, id } => {
                self.node(id)?;
                self.roots.insert(part, id);
                parts.insert(part);
            }
        }
        Ok(())
    }

    /// Takes `id` out of its parent's children; returns that parent.
    fn detach(&mut self, id: u64) -> Option<u64> {
        let parent = self.nodes.get_mut(&id)?.parent.take()?;
        if let Some(node) = self.nodes.get_mut(&parent) {
            node.children.retain(|child| *child != id);
        }
        Some(parent)
    }

    /// The part whose tree holds `id`, if it is attached to one.
    fn part_of(&self, mut id: u64) -> Option<u32> {
        while let Some(parent) = self.nodes.get(&id)?.parent {
            id = parent;
        }
        self.roots
            .iter()
            .find_map(|(part, root)| (*root == id).then_some(*part))
    }
}

/// Copies `part`'s tree into the runtime's scratch arena and returns the
/// root's id: what a part's snapshot is built from.
pub(in crate::engine::wasm) fn record(
    runtime: &std::rc::Rc<ShellRuntime>,
    guest: &std::rc::Rc<Guest>,
    tree: &Tree,
    part: u32,
) -> Result<SpecId> {
    let root = *tree
        .roots
        .get(&part)
        .ok_or_else(|| anyhow!("the guest has drawn nothing for part {part}"))?;
    record_node(runtime, guest, tree, root)
}

fn record_node(
    runtime: &std::rc::Rc<ShellRuntime>,
    guest: &std::rc::Rc<Guest>,
    tree: &Tree,
    id: u64,
) -> Result<SpecId> {
    let node = tree.nodes.get(&id).ok_or_else(|| anyhow!("no node {id}"))?;
    let spec = match &node.kind {
        Kind::Registered { export, args } => registered_node(runtime, guest, &[], export, args)?,
        kind => runtime.arena_mut().push(match kind {
            Kind::Div => Component::Div,
            Kind::HFlex => Component::HFlex,
            Kind::VFlex => Component::VFlex,
            Kind::Text(text) => Component::Text(text.clone()),
            Kind::Button(id) => Component::Button(id.clone()),
            Kind::Checkbox(id) => Component::Checkbox(id.clone()),
            Kind::Input(handle) => Component::Input(EntityHandle::from(*handle)),
            Kind::Registered { .. } => unreachable!("matched above"),
        }),
    };
    for (_, prop) in &node.props {
        let op = prop_op(runtime, guest, spec, prop)?;
        runtime
            .arena_mut()
            .push_op(spec, op)
            .map_err(|error| anyhow!("{error}"))?;
    }
    for event in &node.listeners {
        let name = handler_name(event)?;
        let callback = runtime.register_node(guest, id, name);
        runtime
            .arena_mut()
            .push_op(spec, SpecOp::Callback(name, callback))
            .map_err(|error| anyhow!("{error}"))?;
    }
    for &child in &node.children {
        let child = record_node(runtime, guest, tree, child)?;
        runtime
            .arena_mut()
            .attach(spec, child)
            .map_err(|error| anyhow!("{error}"))?;
    }
    Ok(spec)
}

fn prop_op(
    runtime: &std::rc::Rc<ShellRuntime>,
    guest: &std::rc::Rc<Guest>,
    id: SpecId,
    prop: &Prop,
) -> Result<SpecOp> {
    Ok(match prop {
        Prop::Style(name) => SpecOp::NullaryStyle(
            crate::style::nullary_index(name).ok_or_else(|| anyhow!("unknown style `{name}`"))?,
        ),
        Prop::ParamStyle(name, args) => param_style(name, args)?,
        Prop::Method(name, args) => SpecOp::Method(intern(name), bridged(args)),
        Prop::Component(name, args) => registered_method(runtime, guest, &[], id, name, args)?,
        Prop::StateStyle(name, props) => {
            let name = state_style_name(name)?;
            // A detached node holding the styles, claimed so it feeds the
            // style and never the tree, as a description's state style is.
            let styles = runtime.arena_mut().push(Component::Div);
            for prop in props {
                let op = prop_op(runtime, guest, styles, prop)?;
                runtime
                    .arena_mut()
                    .push_op(styles, op)
                    .map_err(|error| anyhow!("{error}"))?;
            }
            runtime
                .arena_mut()
                .claim(styles)
                .map_err(|error| anyhow!("{error}"))?;
            SpecOp::StateStyle(name, styles)
        }
    })
}
