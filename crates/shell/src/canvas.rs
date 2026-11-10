//! A box its application paints itself: rectangles and runs of text at
//! positions the application chose, in order.
//!
//! The rest of the description is laid out by GPUI; this is for a surface
//! whose content is positioned rather than laid out (a terminal grid, a
//! chart). Text is shaped through the window's text system, whose line
//! layout cache keeps a run that is painted again from being shaped again.

use std::rc::Rc;

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Point, Refineable as _, SharedString, Style, StyleRefinement, TextAlign, TextRun,
    Window,
};

/// What one canvas paints.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CanvasSpec {
    pub(crate) font: SharedString,
    pub(crate) ops: Vec<CanvasOp>,
}

/// One drawing operation, in pixels from the canvas's top-left. Only the
/// WebAssembly engine records them.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(feature = "wasm-engine"), allow(dead_code))]
pub(crate) enum CanvasOp {
    Rect {
        bounds: Bounds<Pixels>,
        color: gpui::Hsla,
    },
    Text {
        origin: Point<Pixels>,
        text: SharedString,
        color: gpui::Hsla,
        size: Pixels,
        line_height: Pixels,
    },
}

pub(crate) struct CanvasElement {
    spec: Rc<CanvasSpec>,
    style: StyleRefinement,
}

impl CanvasElement {
    pub(crate) fn new(spec: Rc<CanvasSpec>, style: StyleRefinement) -> Self {
        Self { spec, style }
    }
}

impl IntoElement for CanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CanvasElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let font = gpui::font(self.spec.font.clone());
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for op in &self.spec.ops {
                match op {
                    CanvasOp::Rect {
                        bounds: rect,
                        color,
                    } => window.paint_quad(gpui::fill(
                        Bounds::new(bounds.origin + rect.origin, rect.size),
                        *color,
                    )),
                    CanvasOp::Text {
                        origin,
                        text,
                        color,
                        size,
                        line_height,
                    } => {
                        let run = TextRun {
                            len: text.len(),
                            font: font.clone(),
                            color: *color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let line =
                            window
                                .text_system()
                                .shape_line(text.clone(), *size, &[run], None);
                        if let Err(error) = line.paint(
                            bounds.origin + *origin,
                            *line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        ) {
                            tracing::debug!("a canvas text run could not be painted: {error}");
                        }
                    }
                }
            }
        });
    }
}
