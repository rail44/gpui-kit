//! A guest laid out as a dock: a file list docked left and, in the center, an
//! editor and a notes panel sharing one tab strip. Each is a part of the guest
//! shown as a panel the user can drag and resize, a script's `DockArea` with
//! `add_panel`. The file list also saves the layout and restores it, as a
//! script's `dock.dump()` and `dock.load(state)`.

use gpui_shell_wasm_guest::prelude::*;

struct Workbench {
    dock: Dock,
    open: Option<&'static str>,
    saved: Option<String>,
}

const FILES: [&str; 3] = ["main.rs", "lib.rs", "Cargo.toml"];

impl View for Workbench {
    fn init(cx: &mut Cx<Self>) -> Self {
        let files = cx.part(|this: &mut Workbench, cx| {
            v_flex()
                .gap(4.)
                .p(8.)
                .child(text("Files"))
                .children(FILES.map(|file| {
                    button(format!("open-{file}"))
                        .child(text(file))
                        .on_click(cx.listener(move |this: &mut Workbench, _, cx| {
                            this.open = Some(file);
                            cx.notify();
                        }))
                }))
                .child(text(format!("({} open)", this.open.map_or(0, |_| 1))))
                .child(
                    button("save-layout")
                        .child(text("Save layout"))
                        .on_click(cx.listener(|this: &mut Workbench, _, cx| {
                            this.saved = Some(this.dock.dump());
                            cx.notify();
                        })),
                )
                .child(
                    button("restore-layout")
                        .child(text("Restore layout"))
                        .on_click(cx.listener(|this: &mut Workbench, _, _| {
                            if let Some(layout) = &this.saved {
                                this.dock.load(layout);
                            }
                        })),
                )
                .child(text(format!(
                    "Saved: {}",
                    this.saved.as_ref().map_or(0, String::len)
                )))
        });
        let editor = cx.part(|this: &mut Workbench, _cx| {
            v_flex()
                .p(8.)
                .child(text("Editor"))
                .child(text(this.open.unwrap_or("nothing open")))
        });
        let notes = cx.part(|_: &mut Workbench, _cx| v_flex().p(8.).child(text("Notes")));
        let dock = Dock::new("workbench");
        dock.add_panel(files, "files", "left");
        dock.add_panel(editor, "editor", "center");
        dock.add_panel(notes, "notes", "center");
        Workbench {
            dock,
            open: None,
            saved: None,
        }
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex().size_full().child(dock(&self.dock))
    }
}

export_view!(Workbench);
