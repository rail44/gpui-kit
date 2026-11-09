//! The data a WebAssembly guest view hands gpui-shell, and the names of the
//! functions on both sides of the module boundary.
//!
//! A guest describes one render as a flat list of [`Node`]s. Each node mirrors
//! one recorded element of gpui-shell's `SpecArena`: what constructed it, the
//! style and behavior calls made on it, and its children by index. The host
//! records the list into the arena in one pass, so a render crosses the module
//! boundary once rather than once per builder call.
//!
//! The guest is a core WebAssembly module (no component model, no WASI). Every
//! byte buffer crosses as a `(pointer, length)` pair in the guest's linear
//! memory; a buffer returned to the host is packed into one `u64` as
//! `pointer << 32 | length`.

use serde::{Deserialize, Serialize};

/// The module the host's imports are linked under.
pub const IMPORT_MODULE: &str = "gpui_shell";

/// Guest exports.
pub mod export {
    /// `fn(len: u32) -> u32`: allocates `len` bytes in guest memory for the
    /// host to write into, and returns the pointer.
    pub const ALLOC: &str = "gpui_shell_alloc";
    /// `fn()`: constructs the view. Runs where gpui-shell runs a script view's
    /// `init`, so retained state may be created here.
    pub const INIT: &str = "gpui_shell_init";
    /// `fn() -> u64`: describes one render; returns a packed buffer holding a
    /// JSON-encoded [`super::Description`].
    pub const RENDER: &str = "gpui_shell_render";
    /// `fn(callback: u32, event_ptr: u32, event_len: u32) -> u32`: runs one
    /// handler with a JSON-encoded event payload. Returns 1 when the handler
    /// asked for a re-render, 0 otherwise.
    pub const DISPATCH: &str = "gpui_shell_dispatch";
}

/// Host imports, all under [`IMPORT_MODULE`].
pub mod import {
    /// `fn(placeholder_ptr: u32, placeholder_len: u32) -> u32`: creates a
    /// retained text input and returns its handle.
    pub const INPUT_NEW: &str = "input_new";
    /// `fn(handle: u32) -> u64`: the input's text, as a packed buffer the host
    /// allocated with the guest's `alloc` export.
    pub const INPUT_VALUE: &str = "input_value";
    /// `fn(handle: u32, ptr: u32, len: u32)`: replaces the input's text.
    pub const INPUT_SET_VALUE: &str = "input_set_value";
    /// `fn(handle: u32, callback: u32)`: runs `callback` when Enter is pressed
    /// in the input, for as long as the input lives.
    pub const INPUT_ON_SUBMIT: &str = "input_on_submit";
    /// `fn(ptr: u32, len: u32)`: writes a line to the host log.
    pub const LOG: &str = "log";
}

/// Packs a guest buffer into the single `u64` an export or import returns.
pub fn pack(ptr: u32, len: u32) -> u64 {
    (u64::from(ptr) << 32) | u64::from(len)
}

/// Splits a packed buffer back into `(pointer, length)`.
pub fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// One render: every node, and which of them is the root.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Description {
    pub root: u32,
    pub nodes: Vec<Node>,
}

/// One element, as gpui-shell records it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub component: Component,
    pub ops: Vec<Op>,
    pub children: Vec<u32>,
}

/// What constructed an element. The `gpui-base` element set a script view
/// reaches through `"gpui-kit"`/`"gpui-base"`; registered `gpui-component`
/// components are out of this spike's scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Component {
    Div,
    HFlex,
    VFlex,
    Text(String),
    /// The element id.
    Button(String),
    /// The element id.
    Checkbox(String),
    /// A retained input, by the handle `input_new` returned.
    Input(u32),
}

/// One call recorded on an element.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    /// A no-argument style method (`size_full`, `items_center`, ...).
    Style(String),
    /// A style method with arguments (`gap(8)`, `bg("primary")`, ...).
    ParamStyle(String, Vec<Arg>),
    /// A behavior method (`checked(true)`, `disabled(false)`, ...).
    Method(String, Vec<Arg>),
    /// An event handler (`on_click`, `on_change`) and the guest's callback id.
    Callback(String, u32),
}

/// An argument value, matching gpui-shell's neutral `Bridged` value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Arg {
    Nil,
    Bool(bool),
    Number(f64),
    Str(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips() {
        assert_eq!(unpack(pack(0x1234, 0xffff_ffff)), (0x1234, 0xffff_ffff));
    }
}
