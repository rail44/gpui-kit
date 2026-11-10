//! A terminal-like grid drawn by a WebAssembly guest, two ways:
//!
//! * **elements**: one row of colored text spans per line, laid out by GPUI
//!   like any other description;
//! * **canvas**: the same cells as paint operations, each run placed at its
//!   column by the guest, which asks the host for the font's advance.
//!
//! The lines come from a command run through the process stream (escape
//! sequences are dropped, not interpreted), or from a synthetic source a
//! measurement steps by hand: `scroll` adds one line per step, as output
//! does; `redraw` changes every visible line, as a full-screen program does;
//! `cells` changes every line with a color per character, the worst case a
//! highlighted or graphical program can produce.
//!
//! ```sh
//! examples/wasm_terminal/run.sh
//! ```

use std::collections::VecDeque;

use gpui_shell_wasm_guest::{Resource, abi::Arg, prelude::*, process_stream};

const FONT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 18.;
const SCROLLBACK: usize = 10_000;
const PALETTE: [&str; 6] = [
    "#8ec07c", "#83a598", "#d3869b", "#fabd2f", "#fb4934", "#ebdbb2",
];
const FOREGROUND: &str = "#ebdbb2";
const BACKGROUND: &str = "#1d2021";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Elements,
    Canvas,
}

#[derive(Clone, Copy, PartialEq)]
enum Pattern {
    Scroll,
    Redraw,
    Cells,
}

/// One run of text in one color.
#[derive(Clone)]
struct Span {
    text: String,
    color: &'static str,
}

type Line = Vec<Span>;

struct Terminal {
    columns: usize,
    rows: usize,
    mode: Mode,
    pattern: Pattern,
    step: u64,
    lines: VecDeque<Line>,
    command: InputState,
    running: Option<Resource>,
    font: String,
    advance: f32,
}

/// A synthetic line shaped like `ls -l` output: six colored fields.
fn synthetic(index: u64, step: u64) -> Line {
    let fields = [
        "drwxr-xr-x".to_owned(),
        format!("{:>3}", index % 97),
        "user".to_owned(),
        format!("{:>8}", (index * 7919 + step) % 1_000_000),
        format!("Nov 11 {:02}:{:02}", (index / 60) % 24, index % 60),
        format!("entry-{index}-{step}"),
    ];
    fields
        .into_iter()
        .enumerate()
        .map(|(field, text)| Span {
            text: format!("{text} "),
            color: PALETTE[field % PALETTE.len()],
        })
        .collect()
}

/// A line of command output, with escape sequences dropped.
fn plain(text: &str, color: &'static str) -> Line {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte in @..~
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\t' {
            clean.push_str("    ");
        } else if !c.is_control() {
            clean.push(c);
        }
    }
    vec![Span { text: clean, color }]
}

/// `line` cut to `columns` characters.
fn clip(line: &Line, columns: usize) -> Line {
    let mut left = columns;
    let mut clipped = Vec::new();
    for span in line {
        if left == 0 {
            break;
        }
        let text: String = span.text.chars().take(left).collect();
        left -= text.chars().count();
        clipped.push(Span {
            text,
            color: span.color,
        });
    }
    clipped
}

impl Terminal {
    fn push(&mut self, line: Line) {
        self.lines.push_back(line);
        while self.lines.len() > SCROLLBACK {
            self.lines.pop_front();
        }
    }

    /// One synthetic step.
    fn advance_step(&mut self) {
        self.step += 1;
        match self.pattern {
            Pattern::Scroll => {
                let index = self.lines.len() as u64 + self.step;
                self.push(synthetic(index, 0));
            }
            Pattern::Redraw => {
                self.lines = (0..self.rows as u64)
                    .map(|row| synthetic(row, self.step))
                    .collect();
            }
            Pattern::Cells => {
                self.lines = (0..self.rows as u64)
                    .map(|row| {
                        let text: String = synthetic(row, self.step)
                            .into_iter()
                            .map(|span| span.text)
                            .collect();
                        let text = format!("{text:<width$}", width = self.columns);
                        text.chars()
                            .enumerate()
                            .map(|(column, c)| Span {
                                text: c.to_string(),
                                color: PALETTE
                                    [(column + row as usize + self.step as usize) % PALETTE.len()],
                            })
                            .collect()
                    })
                    .collect();
            }
        }
    }

    /// The lines on screen, clipped to the grid.
    fn visible(&self) -> Vec<Line> {
        let start = self.lines.len().saturating_sub(self.rows);
        self.lines
            .iter()
            .skip(start)
            .map(|line| clip(line, self.columns))
            .collect()
    }

    fn run(&mut self, cx: &mut Cx<Self>) {
        let typed = self.command.value();
        let mut words = typed.split_whitespace();
        let Some(command) = words.next() else {
            return;
        };
        let args: Vec<&str> = words.collect();
        if let Some(running) = self.running.take() {
            running.stop();
        }
        self.push(plain(&format!("$ {typed}"), PALETTE[3]));
        let resource =
            process_stream::spawn(command, &args, cx, |this: &mut Terminal, event, cx| {
                if let Some(line) = event.0.get("stdout").and_then(|line| line.as_str()) {
                    this.push(plain(line.trim_end_matches('\n'), FOREGROUND));
                } else if let Some(line) = event.0.get("stderr").and_then(|line| line.as_str()) {
                    this.push(plain(line.trim_end_matches('\n'), PALETTE[4]));
                } else if let Some(exit) = event.0.get("exit") {
                    this.push(plain(&format!("[exit {}]", exit), PALETTE[1]));
                    this.running = None;
                }
                cx.notify();
            });
        self.running = Some(resource);
    }

    fn grid_width(&self) -> f64 {
        (self.columns as f32 * self.advance) as f64
    }

    fn grid_height(&self) -> f64 {
        (self.rows as f32 * LINE_HEIGHT) as f64
    }

    fn as_elements(&self) -> Element {
        let rows = self.visible().into_iter().map(|line| {
            h_flex()
                .h(LINE_HEIGHT as f64)
                .style("flex_none")
                .children(line.into_iter().map(|span| {
                    div()
                        .style("whitespace_nowrap")
                        .text_color(span.color)
                        .child(text(span.text))
                }))
        });
        v_flex()
            .style_with("w", [Arg::Number(self.grid_width())])
            .h(self.grid_height())
            .style("overflow_hidden")
            .style_with("font_family", [Arg::Str(self.font.clone())])
            .text_size(FONT_SIZE as f64)
            .children(rows)
    }

    fn as_canvas(&self) -> Element {
        let mut ops = Vec::new();
        for (row, line) in self.visible().into_iter().enumerate() {
            let mut column = 0;
            for span in line {
                let width = span.text.chars().count();
                ops.push(PaintOp::Text {
                    x: column as f32 * self.advance,
                    y: row as f32 * LINE_HEIGHT,
                    text: span.text,
                    color: span.color.to_owned(),
                    size: FONT_SIZE,
                    line_height: LINE_HEIGHT,
                });
                column += width;
            }
        }
        canvas(&self.font, ops)
            .style_with("w", [Arg::Number(self.grid_width())])
            .h(self.grid_height())
    }

    fn toggle(&self, id: &str, caption: &str, on: bool) -> Element {
        button(id.to_owned())
            .px(8.)
            .py(4.)
            .rounded(4.)
            .bg(if on { "primary" } else { "muted" })
            .text_color(if on {
                "primary_foreground"
            } else {
                "foreground"
            })
            .cursor_pointer()
            .child(text(caption))
    }
}

impl View for Terminal {
    fn init(cx: &mut Cx<Self>) -> Self {
        let font = mono_font();
        let advance = text_advance(&font, FONT_SIZE);
        let command = InputState::new("A command to run, such as: find /usr/share");
        command.on_submit(cx, |this, _, cx| {
            this.run(cx);
            cx.notify();
        });
        Terminal {
            columns: 80,
            rows: 24,
            mode: Mode::Elements,
            pattern: Pattern::Scroll,
            step: 0,
            lines: VecDeque::new(),
            command,
            running: None,
            font,
            advance,
        }
    }

    fn render(&mut self, cx: &mut Cx<Self>) -> Element {
        let controls = h_flex()
            .gap(6.)
            .items_center()
            .child(
                self.toggle("mode-elements", "Elements", self.mode == Mode::Elements)
                    .on_click(cx.listener(|this, _, cx| {
                        this.mode = Mode::Elements;
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("mode-canvas", "Canvas", self.mode == Mode::Canvas)
                    .on_click(cx.listener(|this, _, cx| {
                        this.mode = Mode::Canvas;
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("size-80x24", "80x24", self.columns == 80)
                    .on_click(cx.listener(|this, _, cx| {
                        (this.columns, this.rows) = (80, 24);
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("size-200x60", "200x60", self.columns == 200)
                    .on_click(cx.listener(|this, _, cx| {
                        (this.columns, this.rows) = (200, 60);
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("pattern-scroll", "Scroll", self.pattern == Pattern::Scroll)
                    .on_click(cx.listener(|this, _, cx| {
                        this.pattern = Pattern::Scroll;
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("pattern-redraw", "Redraw", self.pattern == Pattern::Redraw)
                    .on_click(cx.listener(|this, _, cx| {
                        this.pattern = Pattern::Redraw;
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("pattern-cells", "Cells", self.pattern == Pattern::Cells)
                    .on_click(cx.listener(|this, _, cx| {
                        this.pattern = Pattern::Cells;
                        cx.notify();
                    })),
            )
            .child(
                self.toggle("step", "Step", false)
                    .on_click(cx.listener(|this, _, cx| {
                        this.advance_step();
                        cx.notify();
                    })),
            )
            .child(text(format!(
                "{} lines, step {}",
                self.lines.len(),
                self.step
            )));

        let grid = match self.mode {
            Mode::Elements => self.as_elements(),
            Mode::Canvas => self.as_canvas(),
        };
        v_flex()
            .size_full()
            .gap(8.)
            .p(12.)
            .bg("background")
            .text_color("foreground")
            .child(controls)
            .child(input(&self.command))
            .child(div().p(8.).rounded(6.).bg(BACKGROUND).child(grid))
    }
}

export_view!(Terminal);
