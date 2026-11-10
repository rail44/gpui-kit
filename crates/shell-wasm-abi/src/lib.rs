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
    /// postcard-encoded [`super::Description`]. (Handler payloads stay JSON:
    /// they are small, and a click's shape is the host's to choose.)
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
    /// `fn(export_ptr: u32, export_len: u32, args_ptr: u32, args_len: u32)
    /// -> u64`: creates a registered component's retained state (a
    /// `gpui-component` `InputState`, ...) from postcard-encoded
    /// `Vec<ComponentArg>` and returns its handle.
    pub const STATE_NEW: &str = "state_new";
    /// `fn(handle: u64, export_ptr: u32, export_len: u32, method_ptr: u32,
    /// method_len: u32, args_ptr: u32, args_len: u32) -> u64`: calls a state
    /// method with postcard-encoded `Vec<Data>`; returns a packed buffer
    /// holding the postcard-encoded `Data` result.
    pub const STATE_CALL: &str = "state_call";
    /// `fn(handle: u64, export_ptr: u32, export_len: u32, event_ptr: u32,
    /// event_len: u32, callback: u32)`: runs `callback` on a state's input
    /// event (`change`, `submit`, `focus`, `blur`) for as long as it lives.
    pub const STATE_ON: &str = "state_on";
    /// `fn(key_ptr: u32, key_len: u32) -> u64`: the application's persistent
    /// store entry (a script's `localStorage.getItem`), as a packed buffer, or
    /// [`super::ABSENT`] when there is none. Needs the `storage` capability.
    pub const STORAGE_GET: &str = "storage_get";
    /// `fn(key_ptr: u32, key_len: u32, value_ptr: u32, value_len: u32)`:
    /// `localStorage.setItem`.
    pub const STORAGE_SET: &str = "storage_set";
    /// `fn(key_ptr: u32, key_len: u32)`: `localStorage.removeItem`.
    pub const STORAGE_REMOVE: &str = "storage_remove";
    /// `fn(path_ptr: u32, path_len: u32) -> u64`: a UTF-8 file inside a
    /// `capabilities.fs.read` root, as a packed buffer. Traps with the
    /// capability's message when the path is not granted.
    pub const FS_READ_TEXT: &str = "fs_read_text";
    /// `fn(path_ptr: u32, path_len: u32, text_ptr: u32, text_len: u32)`:
    /// writes a file inside a `capabilities.fs.write` root.
    pub const FS_WRITE_TEXT: &str = "fs_write_text";
    /// `fn(url_ptr: u32, url_len: u32, callback: u32)`: starts a GET the
    /// application's network grants allow (traps when they do not) and
    /// returns at once; `callback` later receives `{"status", "body"}` or
    /// `{"error"}` as its JSON payload.
    pub const HTTP_GET: &str = "http_get";
    /// `fn(request_ptr: u32, request_len: u32, callback: u32)`: as
    /// [`HTTP_GET`] for any method, with a JSON request `{"method", "url",
    /// "headers": [[name, value], ...], "body"}`.
    pub const HTTP_REQUEST: &str = "http_request";
    /// `fn(request_ptr: u32, request_len: u32, callback: u32)`: runs a
    /// command the execute grant allows, from a JSON request `{"command",
    /// "args"}`; `callback` receives `{"code", "stdout", "stderr"}` or
    /// `{"error"}`.
    pub const PROCESS_RUN: &str = "process_run";
    /// `fn(path_ptr: u32, path_len: u32, callback: u32)`: [`FS_READ_TEXT`]
    /// off the main thread; `callback` receives `{"text"}` or `{"error"}`.
    pub const FS_READ_TEXT_ASYNC: &str = "fs_read_text_async";
    /// `fn(path_ptr: u32, path_len: u32, text_ptr: u32, text_len: u32,
    /// callback: u32)`: [`FS_WRITE_TEXT`] off the main thread; `callback`
    /// receives `{"ok": true}` or `{"error"}`.
    pub const FS_WRITE_TEXT_ASYNC: &str = "fs_write_text_async";
    /// `fn(key_ptr: u32, key_len: u32) -> u64`: `sessionStorage.getItem`
    /// ([`super::ABSENT`] when unset). No capability, no disk.
    pub const SESSION_GET: &str = "session_get";
    /// `fn(key_ptr: u32, key_len: u32, value_ptr: u32, value_len: u32)`:
    /// `sessionStorage.setItem`.
    pub const SESSION_SET: &str = "session_set";
    /// `fn() -> u64`: the clipboard's text ([`super::ABSENT`] when it holds
    /// none), under the `clipboard.read` grant.
    pub const CLIPBOARD_READ: &str = "clipboard_read";
    /// `fn(text_ptr: u32, text_len: u32)`: replaces the clipboard's text,
    /// under the `clipboard.write` grant.
    pub const CLIPBOARD_WRITE: &str = "clipboard_write";
}

/// What a lookup import returns for "no value", distinct from any packed
/// buffer (a guest has no allocation at the top of a 32-bit address space).
pub const ABSENT: u64 = u64::MAX;

/// A plain data value: what a registered state method takes and returns,
/// mirroring gpui-shell's `ComponentDataValue`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Data {
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
    Array(Vec<Data>),
    Object(Vec<(String, Data)>),
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
    /// A component the host's catalog registered (`gpui-component`'s
    /// `Button`, `Tag`, `Progress`, ...), by its constructor's export name.
    Registered {
        export: String,
        args: Vec<ComponentArg>,
    },
}

/// An argument to a registered component's constructor or method, mirroring
/// gpui-shell's `ComponentArgument`. Elements and handlers are passed by
/// number: an element is a node index that must come before the node using
/// it, a handler is the guest's callback id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComponentArg {
    String(String),
    Number(f64),
    Boolean(bool),
    Enum(String),
    Element(u32),
    Callback(u32),
    /// A registered retained state, by the handle `state_new` returned.
    State(u64),
    Array(Vec<ComponentArg>),
    Optional(Option<Box<ComponentArg>>),
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
    /// A method a registered component declares (`label`, `primary`,
    /// `value`, ...).
    Component(String, Vec<ComponentArg>),
    /// A state style (`hover`, `active`, `focus`): the styles recorded on a
    /// detached node, by its index, applied while the element is in that
    /// state.
    StateStyle(String, u32),
    /// A named element slot (`content`, `trigger`, `header`, ...): the node
    /// at the index is rendered by the component in a place of its own.
    Slot(String, u32),
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
