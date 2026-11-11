# WebAssembly guests: reactive views and a retained host tree

> [!NOTE]
> A design, agreed on 2026-11-11, and partly implemented; see "Where it
> stands" at the end. It changes how the `wasm-engine` build
> (`crates/shell/src/engine/wasm`) moves a view across the guest/host
> boundary. Everything else about that engine stays as it is.

## Why

Today a guest re-runs its whole `render` on every notify and sends the host
the entire element tree as one postcard blob. The host decodes it (allocating
every string), copies it into the spec arena, and materializes it. Three
costs follow, measured or read off the code on the `shell-wasm-engine`
branch:

- The whole tree crosses and is decoded on every change, however small.
- A theme color travels as a token, is resolved to a color, formatted as a
  hex string, and parsed back when materialized, per element per render.
- One guest's parts (its root, nested child views, dialogs, panels) share
  one guest state with no record of what each part read, so any notify
  rebuilds every part.

Web frameworks shipping server-rendered UI meet the same problem. Phoenix
LiveView answers it with a static/dynamic split and fingerprints, because its
render function re-runs and the client must match new output to old.
Solid's answer applies here more directly: when the declaration itself is
reactive, structure is created once and only the bindings whose inputs
changed run again. Identity is fixed at creation, so nothing has to be
matched and no fingerprint is needed. The closest existing GPUI precedent is
GPUIX (github.com/remorses/gpuix), which drives a retained GPUI tree from a
JavaScript renderer through one batch of node operations.

## What a guest writes

A view is a function that runs once to build structure. State lives in
signals; a value that changes is passed as a closure; conditionals and lists
are components of their own, because plain `if` and `for` would run only
once.

```rust
fn counter() -> Element {
    let count = create_signal(0);
    h_flex()
        .child(text(move || format!("Count: {}", count.get())))
        .child(button("inc").on_click(move |_| count.set(count.get() + 1)))
}
```

The reactive runtime is `sycamore-reactive` (MIT, 0.9.x at the time of
writing), kept inside `gpui-shell-wasm-guest` so a guest never names it. It
was chosen on evidence: built for `wasm32-unknown-unknown` a probe was 33 KB
with no imports and ran under a JavaScript-free wasmtime with effects
settling synchronously. Leptos's `reactive_graph` (0.2.16) built to 454 KB
with seven wasm-bindgen imports and needs a custom executor polled after
every event. Dioxus diffs a virtual DOM, which is the re-run model this
design moves away from.

## The boundary

### Operations

A guest sends a list of operations, each naming its node:

| Operation | Meaning |
|---|---|
| `Create { id, kind, props }` | A node of `kind` (div, text, button, a registered component, ...) with its props that never change |
| `SetProp { id, name, value }` | Set one prop or style; an absent value removes it |
| `SetText { id, text }` | Replace a text node's contents |
| `Insert { parent, child, before }` | Insert before `before`, or at the end; inserting an attached node moves it |
| `Remove { id }` | Detach from its parent; the node remains |
| `Dispose { id }` | Destroy the node and its descendants, and drop their handlers |
| `Listen { id, event }` / `Unlisten` | Mark or unmark that the node has a handler for `event` |
| `SetRoot { part, id }` | Make `id` the root of a part (the guest's view, a dialog, a panel, ...) |

The guest knows the tree it built, so it computes every anchor itself and
never queries the host. State styles (`hover`, ...) and named slots are
props. A registered component's constructor arguments are part of `kind`;
when they change the node is recreated, while its methods are props.
Operations are postcard-encoded.

Templates (a static structure registered once and cloned, as Dioxus 0.7
does) are left out until operation counts show they are needed.

### Node identity

The guest numbers nodes with a `u64` counter and never reuses a number. The
host looks a node up by guest and number, so one guest's numbers cannot
collide with or reach another's. A number that is never reused means an
event arriving after a node was disposed finds nothing, on either side,
rather than a newer node that took its number (the hazard of reuse, which a
generation counter would otherwise have to close).

### Handlers and events

A handler's body stays in the guest; the host keeps only the mark `Listen`
set. GPUI attaches handlers per element, so there is no bubbling to emulate.

The guest's dispatch export takes a target, an event name and a payload,
and returns the operations the handler produced: one crossing per event.
Targets are of three kinds, all delivered the same way:

- a node (a click, a change);
- a retained state handle (an input's Enter);
- a resource (a process's output, an HTTP response, a socket message).

Targets and event names cross as typed values, outside any JSON: handles
are 64-bit and JSON numbers lose precision above 2^53. A small payload is
JSON; measured, a click's whole dispatch including its JSON was 0.002 to
0.003 ms. Large data and binary (a response body, a file, a socket frame,
process output that may not be UTF-8) cross as raw bytes beside the
payload, never inside it. A stream that produces many small events delivers
what has arrived as one batch rather than one crossing per item.

Handlers that must return a value (a table cell's data) are out of scope
for now.

### Batching

The guest runs only from `init`, an event, or an asynchronous result, and
effects settle synchronously, so every return from a guest export carries
all the operations queued since the last one. There is no timer and no
explicit flush.

The host applies a batch at once, in order. If an operation is invalid (an
unknown id, say), the guest is recorded as broken and its later batches are
refused, so a half-applied tree is never treated as correct. The views to
refresh are the parts whose subtrees the batch touched; GPUI coalesces them
into one frame however many batches arrive in it.

A runaway chain of effects never returns. Stopping it belongs to execution
limits (wasmtime epoch interruption), not to batching.

## The host

### The retained tree

The host keeps one retained tree per guest and applies operations to it. A
part's view, when it needs to draw, copies its subtree into the spec arena
and builds a snapshot as today: the snapshot is now built from the retained
tree instead of from a guest render and a decode. Materialization, overlays,
docks, hot reload, policy and callback generations, everything above the
engine seam, are used unchanged. Each node's number becomes its element key,
so GPUI element state (hover, scroll, focus) follows the node rather than its
position.

Copying a subtree on every refresh still walks it, inside the host, with no
guest call and no decode. Whether that walk matters is to be measured; if it
does, materializing straight from the retained tree, or making the spec
arena itself retained, are the next options. The latter touches code shared
with the QuickJS engine.

### Lazy lists

A list asks the guest for a range of rows synchronously during layout, as
today, under the same restricted layout scope (no notify, no retained state).
The guest creates only the rows that do not exist yet and returns their
operations, which the host applies before drawing. Created rows are kept, so
later frames need no call; the guest disposes rows that have scrolled far
away. Rows are identified by keys the guest gives, so the list keeps the
reader's place when rows are added or removed.

The guest can also use the measured, variable-height list that `gpui-shell`
already has (`ListKind` other than `Uniform`). A row whose nodes a batch
touched is re-measured. Answering the range from a guest signal instead was
considered and rejected: the rows would arrive a frame late, leaving blank
rows while scrolling fast.

### Retained state

Inputs and registered components' state stay host entities, referenced from
nodes by handle. A guest creates one through a host call that returns the
handle at once, because it may read the state back (`value()`) before its
operations are applied, and reads and calls methods the same way. The guest
SDK registers a cleanup with the reactive owner that created the state, so
disposing that scope (a list row scrolled away) releases the state through
a host call; release with the application generation remains as a backstop.
State events arrive through the single event path above; the SDK offers an
optional wrapper that feeds them into a signal.

## Open questions, deferred

- Whether copying subtrees into the arena needs replacing (measure first).
- Templates for repeated structure (measure operation counts first).
- Handlers that return values.
- Builder methods only, or also a `view!` macro.

## Where it stands

`examples/wasm_todolist` runs on this protocol. The host tells the two
protocols apart by the exports a module has, so every other example still
describes its view per render, unchanged.

Implemented:

- The ABI's `retained` module, and the guest SDK's `reactive` module on
  `sycamore-reactive`: nodes for div, flex, text, button, checkbox, input and
  registered components, fixed or reactive props, `hover`, keyed lists
  (`Node::each`), node handlers, an input's submit, and `export_reactive!`.
- The host's tree (`engine/wasm/guest/retained.rs`), applied batch by batch
  with a broken guest's later batches refused, copied into the spec arena
  when a part draws, and refreshing only the parts a batch touched.

Measured with `scripts/compare-engines.sh` (release, three alternating
runs, load average about 7): the todo list with 100 rows describes in
0.18 to 0.25 ms against 0.68 to 0.89 ms when its guest re-rendered, and its
tree is identical to the script twin's. A filter click costs 0.011 to
0.016 ms instead of 0.002 to 0.003 ms, since the guest's effects, the
batch's encoding and its application now happen in the event rather than
in the next render.

### The same view written as a GPUI view is

`examples/wasm_todolist_render` is the todo list written as a GPUI view
(a struct whose `render` runs again after every notify), with the same view
code it had before the reactive rewrite. `export_view_retained!` has the
guest compare each render with the last, by position, and send only the
difference over the same protocol (`gpui-shell-wasm-guest`'s `diffed`). It
passes the same todo test, and its tree is identical to the other two.

Timed side by side (`scripts/compare-engines.sh`, five alternating release
runs, load average about 4.7; best / median, ms, 100 rows):

| | QuickJS | wasm, reactive | wasm, GPUI style |
|---|---|---|---|
| describe | 0.896 / 0.928 | 0.163 / 0.241 | 0.162 / 0.179 |
| filter click | 0.005 / 0.005 | 0.011 / 0.016 | 0.276 / 0.293 |
| one row toggled | 0.005 / 0.005 | 0.059 / 0.087 | 0.276 / 0.293 |
| materialize | 0.746 / 0.759 | 0.763 / 1.054 | 0.756 / 0.762 |

An event's own cost is not the whole of it: QuickJS defers its render to
the next describe and does no comparison at all, while both retained
guests do their work in the event and leave the host only a copy. Per
toggled row, then, QuickJS spends about 0.93 ms (0.005 + 0.928), the
reactive guest about 0.33 ms, and the GPUI-style guest about 0.47 ms: it
renders the whole view again on every notify, as a GPUI view does on the
host, and then compares it with the last.

The GPUI-style figures are after two changes to the comparison, which
first cost 0.56 to 0.79 ms per event (measured natively, render 151 µs,
flattening 51 µs, comparison 320 µs, encoding 0.3 µs, for a 25-byte batch).
The guest now compares the render's element tree directly, moving its
values instead of flattening and copying them, compares props in order
first, and touches the handler table only where a callback moved; and a
style, method or event name, or a string argument, is an
`abi::Name` (`Cow<'static, str>`), so a literal in the view's code is never
copied.

### With rows as child views

`examples/wasm_todolist_children` splits the same view the way a GPUI view
is split into entities: each row is a child view (`cx.new`, kept in the
list's state and placed from its `render`) that holds its own todo. A row
renders again only when it notified; the list, when it renders again,
keeps every row it still places without rendering it. A row tells the list
with `cx.emit`, heard through `cx.subscribe`. The script engine has the
same thing (`cx.new(Class)`, `cx.notify(entity)`); its todo twin does not
use it. It passes the same todo test, with the same tree.

Five alternating release runs, load average about 3.9 (best / median, ms):

| | QuickJS | reactive | GPUI style | GPUI style, child rows |
|---|---|---|---|---|
| describe | 0.895 / 0.977 | 0.157 / 0.240 | 0.166 / 0.175 | 0.159 / 0.216 |
| filter click | 0.005 / 0.006 | 0.011 / 0.016 | 0.285 / 0.294 | 0.019 / 0.023 |
| one row toggled | 0.005 / 0.005 | 0.058 / 0.086 | 0.276 / 0.293 | 0.022 / 0.029 |

Ticking a row now renders that row and the list's own frame (its header,
and a hundred places that are kept), instead of a hundred rows: 0.02 to
0.03 ms in the guest, under the reactive guest's 0.06 to 0.09 ms, whose
every row re-reads the list to find its own todo. Per toggled row to a
description, about 0.25 ms against QuickJS's 0.98 ms. What is left is the
host's copy of the whole part into the arena (the describe row), which a
child view does not yet narrow: for that, a child view has to be a view of
its own on the host too.

Simplified or not yet done:

- Events from retained state and resources arrive as `HANDLER` events
  addressed by the callback id the guest passed to the import, rather than
  as separate state and resource targets.
- Node numbers are not yet element keys; elements are still keyed by their
  arena position.
- A style removed and set again goes after the node's other props, so its
  position can differ from what the same calls would record in a script.
- Lazy lists, parts other than the root (dialogs, sheets, panels, nested
  views), releasing retained state with its reactive owner, and large
  payloads as raw bytes are not implemented for retained guests yet.
