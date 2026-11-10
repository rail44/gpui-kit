//! A guest that reaches the application's capability-gated host services:
//! it counts how many times it has been started in the persistent store
//! (a script's `localStorage`), and, when the store names a notes file, reads
//! it and writes a copy beside it through the `fs` grants. Each is refused
//! the same way it is for a script when the application has no grant.

use gpui_shell_wasm_guest::{fs, prelude::*, storage};

struct Visits {
    count: u32,
    notes: Option<String>,
}

impl View for Visits {
    fn init(_cx: &mut Cx<Self>) -> Self {
        let count = storage::get("visits")
            .and_then(|count| count.parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        storage::set("visits", &count.to_string());
        let notes = storage::get("notes_path").map(|path| {
            let notes = fs::read_text(&path);
            fs::write_text(&format!("{path}.copy"), &notes.to_uppercase());
            notes
        });
        Visits { count, notes }
    }

    fn render(&mut self, _cx: &mut Cx<Self>) -> Element {
        v_flex()
            .child(text(format!("Visits: {}", self.count)))
            .child(text(format!(
                "Notes: {}",
                self.notes.as_deref().unwrap_or("(none)")
            )))
    }
}

export_view!(Visits);
