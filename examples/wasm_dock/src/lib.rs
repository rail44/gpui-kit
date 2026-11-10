//! A guest laid out as a dock: a file list docked left and an editor in the
//! center, each a part of the guest shown as a panel the user can drag and
//! resize — a script's `DockArea` with `add_panel`.

use gpui_shell_wasm_guest::prelude::*;

struct Workbench {
    dock: Dock,
    open: Option<&'static str>,
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
        });
        let editor = cx.part(|this: &mut Workbench, _cx| {
            v_flex()
                .p(8.)
                .child(text("Editor"))
                .child(text(this.open.unwrap_or("nothing open")))
        });
        let dock = Dock::new("workbench");
        dock.add_panel(files, "files", "left");
        dock.add_panel(editor, "editor", "center");
        Workbench { dock, open: None }
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex().size_full().child(dock(&self.dock))
    }
}

export_view!(Workbench);
