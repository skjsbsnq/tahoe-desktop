//! Typed logical coordinate spaces for lifecycle geometry.
//!
//! Bare `Rectangle<_, Logical>` does not encode whether a value is surface-local, output-local,
//! workspace-content, workspace-view, or global. Mixing those spaces is the F03 Genie defect.
//! Types in this module are not implicitly interchangeable; every conversion requires the
//! geometric context for that direction.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// Rectangle relative to a layer/toplevel surface origin (protocol `set_rectangle` input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceLocalRect {
    rect: Rectangle<i32, Logical>,
}

/// Rectangle in an output's logical coordinate system (origin at output top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLocalRect {
    rect: Rectangle<i32, Logical>,
}

/// Floating-point output-local rectangle used by animation morph endpoints.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLocalRectF {
    rect: Rectangle<f64, Logical>,
}

/// Point in output-local logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLocalPoint {
    point: Point<f64, Logical>,
}

/// Rectangle in scrolling workspace *content* coordinates (independent of current view offset).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceContentRect {
    rect: Rectangle<f64, Logical>,
}

/// Point in scrolling workspace content coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceContentPoint {
    point: Point<f64, Logical>,
}

/// Rectangle relative to the current workspace view (content minus view offset).
///
/// For a workspace that fills its output with origin `(0, 0)`, this matches [`OutputLocalRectF`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceViewRect {
    rect: Rectangle<f64, Logical>,
}

/// Point relative to the current workspace view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceViewPoint {
    point: Point<f64, Logical>,
}

/// Rectangle in global compositor logical coordinates (output position + output-local).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobalRect {
    rect: Rectangle<f64, Logical>,
}

impl SurfaceLocalRect {
    pub fn new(loc: Point<i32, Logical>, size: Size<i32, Logical>) -> Self {
        Self {
            rect: Rectangle::new(loc, size),
        }
    }

    pub fn from_rect(rect: Rectangle<i32, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<i32, Logical> {
        self.rect
    }

    /// Protocol surface-local rect plus mapped layer geometry → output-local.
    ///
    /// Wrapping arithmetic is intentional only for tests of the naive path; production
    /// protocol handling must use [`Self::try_to_output_local`].
    pub fn to_output_local(self, layer_geometry: Rectangle<i32, Logical>) -> OutputLocalRect {
        OutputLocalRect {
            rect: Rectangle::new(layer_geometry.loc + self.rect.loc, self.rect.size),
        }
    }

    /// Checked surface-local → output-local conversion.
    ///
    /// Returns `None` when coordinate addition overflows `i32`. Callers must fail closed
    /// to an unresolved last-hint rather than panic, wrap, or keep a prior value.
    pub fn try_to_output_local(
        self,
        layer_geometry: Rectangle<i32, Logical>,
    ) -> Option<OutputLocalRect> {
        let x = layer_geometry.loc.x.checked_add(self.rect.loc.x)?;
        let y = layer_geometry.loc.y.checked_add(self.rect.loc.y)?;
        Some(OutputLocalRect {
            rect: Rectangle::new(Point::from((x, y)), self.rect.size),
        })
    }
}

impl OutputLocalRect {
    pub fn new(loc: Point<i32, Logical>, size: Size<i32, Logical>) -> Self {
        Self {
            rect: Rectangle::new(loc, size),
        }
    }

    pub fn from_rect(rect: Rectangle<i32, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<i32, Logical> {
        self.rect
    }

    pub fn loc(self) -> Point<i32, Logical> {
        self.rect.loc
    }

    pub fn size(self) -> Size<i32, Logical> {
        self.rect.size
    }

    pub fn is_empty(self) -> bool {
        self.rect.size.w <= 0 || self.rect.size.h <= 0
    }

    pub fn to_f64(self) -> OutputLocalRectF {
        OutputLocalRectF {
            rect: self.rect.to_f64(),
        }
    }

    pub fn to_global(self, output_global_origin: Point<f64, Logical>) -> GlobalRect {
        self.to_f64().to_global(output_global_origin)
    }
}

impl OutputLocalRectF {
    pub fn new(loc: Point<f64, Logical>, size: Size<f64, Logical>) -> Self {
        Self {
            rect: Rectangle::new(loc, size),
        }
    }

    pub fn from_rect(rect: Rectangle<f64, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<f64, Logical> {
        self.rect
    }

    pub fn loc(self) -> Point<f64, Logical> {
        self.rect.loc
    }

    pub fn size(self) -> Size<f64, Logical> {
        self.rect.size
    }

    pub fn is_empty(self) -> bool {
        self.rect.size.w < 1. || self.rect.size.h < 1.
    }

    /// Output-local ↔ workspace-view when the workspace origin on the output is known.
    pub fn to_workspace_view(
        self,
        workspace_origin_on_output: Point<f64, Logical>,
    ) -> WorkspaceViewRect {
        WorkspaceViewRect {
            rect: Rectangle::new(self.rect.loc - workspace_origin_on_output, self.rect.size),
        }
    }

    pub fn to_global(self, output_global_origin: Point<f64, Logical>) -> GlobalRect {
        GlobalRect {
            rect: Rectangle::new(self.rect.loc + output_global_origin, self.rect.size),
        }
    }
}

impl OutputLocalPoint {
    pub fn new(x: f64, y: f64) -> Self {
        Self {
            point: Point::from((x, y)),
        }
    }

    pub fn from_point(point: Point<f64, Logical>) -> Self {
        Self { point }
    }

    pub fn as_point(self) -> Point<f64, Logical> {
        self.point
    }

    pub fn to_workspace_view(
        self,
        workspace_origin_on_output: Point<f64, Logical>,
    ) -> WorkspaceViewPoint {
        WorkspaceViewPoint {
            point: self.point - workspace_origin_on_output,
        }
    }
}

impl WorkspaceContentPoint {
    pub fn from_point(point: Point<f64, Logical>) -> Self {
        Self { point }
    }

    pub fn as_point(self) -> Point<f64, Logical> {
        self.point
    }

    /// Content → view: subtract the current scrolling view offset (`view_pos` on x).
    pub fn to_view(self, view_pos: f64) -> WorkspaceViewPoint {
        WorkspaceViewPoint {
            point: Point::from((self.point.x - view_pos, self.point.y)),
        }
    }
}

impl WorkspaceContentRect {
    pub fn from_rect(rect: Rectangle<f64, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<f64, Logical> {
        self.rect
    }

    pub fn to_view(self, view_pos: f64) -> WorkspaceViewRect {
        WorkspaceViewRect {
            rect: Rectangle::new(
                Point::from((self.rect.loc.x - view_pos, self.rect.loc.y)),
                self.rect.size,
            ),
        }
    }
}

impl WorkspaceViewPoint {
    pub fn from_point(point: Point<f64, Logical>) -> Self {
        Self { point }
    }

    pub fn as_point(self) -> Point<f64, Logical> {
        self.point
    }

    /// View → content: add the current scrolling view offset.
    pub fn to_content(self, view_pos: f64) -> WorkspaceContentPoint {
        WorkspaceContentPoint {
            point: Point::from((self.point.x + view_pos, self.point.y)),
        }
    }

    /// View → output-local using the workspace's origin on the output.
    ///
    /// Active workspaces that fill the output use origin `(0, 0)` (identity).
    pub fn to_output_local(
        self,
        workspace_origin_on_output: Point<f64, Logical>,
    ) -> OutputLocalPoint {
        OutputLocalPoint {
            point: self.point + workspace_origin_on_output,
        }
    }
}

impl WorkspaceViewRect {
    pub fn from_rect(rect: Rectangle<f64, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<f64, Logical> {
        self.rect
    }

    pub fn to_content(self, view_pos: f64) -> WorkspaceContentRect {
        WorkspaceContentRect {
            rect: Rectangle::new(
                Point::from((self.rect.loc.x + view_pos, self.rect.loc.y)),
                self.rect.size,
            ),
        }
    }

    pub fn to_output_local(
        self,
        workspace_origin_on_output: Point<f64, Logical>,
    ) -> OutputLocalRectF {
        OutputLocalRectF {
            rect: Rectangle::new(self.rect.loc + workspace_origin_on_output, self.rect.size),
        }
    }
}

impl GlobalRect {
    pub fn from_rect(rect: Rectangle<f64, Logical>) -> Self {
        Self { rect }
    }

    pub fn as_rect(self) -> Rectangle<f64, Logical> {
        self.rect
    }

    pub fn to_output_local(self, output_global_origin: Point<f64, Logical>) -> OutputLocalRectF {
        OutputLocalRectF {
            rect: Rectangle::new(self.rect.loc - output_global_origin, self.rect.size),
        }
    }
}

/// Resolves Genie morph endpoints into the single canonical space ([`OutputLocalPoint`] /
/// [`OutputLocalRectF`]).
///
/// Scrolling tile render positions are workspace-view; dock anchors are output-local. Both are
/// converted here so `MinimizeWindowAnimation` never mixes spaces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenieEndpointResolve {
    /// Workspace origin on the destination output (usually `(0, 0)` for a full-size workspace).
    pub workspace_origin_on_output: Point<f64, Logical>,
}

impl GenieEndpointResolve {
    pub fn identity() -> Self {
        Self {
            workspace_origin_on_output: Point::from((0., 0.)),
        }
    }

    pub fn window_from_view_pos(self, tile_view_pos: Point<f64, Logical>) -> OutputLocalPoint {
        WorkspaceViewPoint::from_point(tile_view_pos)
            .to_output_local(self.workspace_origin_on_output)
    }

    pub fn anchor_from_output_local(self, anchor: OutputLocalRect) -> OutputLocalRectF {
        // Anchor is already output-local; workspace origin does not apply to Dock geometry.
        let _ = self;
        anchor.to_f64()
    }
}

/// Pure numerical case from the F03 research report: `view_pos=1000`, window screen x=`100`,
/// dock output-local x=`900` must not produce dock x=`-100` after a single view subtraction.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f03_view_pos_1000_window_and_anchor_share_output_local() {
        let view_pos = 1000.0;
        // Window appears at screen x=100 while content x = view_pos + 100.
        let window_content = WorkspaceContentPoint::from_point(Point::from((1100.0, 50.0)));
        let window_view = window_content.to_view(view_pos);
        assert_eq!(window_view.as_point(), Point::from((100.0, 50.0)));

        let resolve = GenieEndpointResolve::identity();
        let window_ol = resolve.window_from_view_pos(window_view.as_point());
        let anchor = OutputLocalRect::new(Point::from((900, 700)), Size::from((48, 48)));
        let anchor_ol = resolve.anchor_from_output_local(anchor);

        // Both endpoints live in output-local; no second view_pos subtraction.
        assert_eq!(window_ol.as_point().x, 100.0);
        assert_eq!(anchor_ol.loc().x, 900.0);

        // The old bug path: store content for window, bare output-local for anchor, then subtract
        // view_pos from both → dock at -100.
        let buggy_dock_x = anchor_ol.loc().x - view_pos;
        assert_eq!(buggy_dock_x, -100.0);
        assert_ne!(
            window_ol.as_point().x - view_pos,
            anchor_ol.loc().x - view_pos,
            "pre-fix mixed spaces only look consistent when view_pos is 0"
        );
    }

    #[test]
    fn surface_local_plus_layer_geo_is_output_local() {
        let surface = SurfaceLocalRect::new(Point::from((10, 20)), Size::from((30, 40)));
        let layer = Rectangle::new(Point::from((0, 640)), Size::from((200, 80)));
        let out = surface.to_output_local(layer);
        assert_eq!(
            out.as_rect(),
            Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
        );
        assert_eq!(
            surface.try_to_output_local(layer).unwrap().as_rect(),
            out.as_rect()
        );
    }

    #[test]
    fn try_to_output_local_fails_closed_on_i32_overflow() {
        let surface = SurfaceLocalRect::new(Point::from((i32::MAX, 0)), Size::from((1, 1)));
        let layer = Rectangle::new(Point::from((1, 0)), Size::from((10, 10)));
        assert!(surface.try_to_output_local(layer).is_none());
    }

    #[test]
    fn content_view_roundtrip() {
        let content = WorkspaceContentPoint::from_point(Point::from((1500.0, 12.0)));
        let view = content.to_view(400.0);
        assert_eq!(view.as_point(), Point::from((1100.0, 12.0)));
        assert_eq!(view.to_content(400.0).as_point(), content.as_point());
    }

    #[test]
    fn workspace_view_to_output_local_uses_origin() {
        let view = WorkspaceViewPoint::from_point(Point::from((40.0, 50.0)));
        let ol = view.to_output_local(Point::from((0.0, 100.0)));
        assert_eq!(ol.as_point(), Point::from((40.0, 150.0)));
    }

    #[test]
    fn global_output_local_roundtrip() {
        let ol = OutputLocalRectF::new(Point::from((10.0, 20.0)), Size::from((30.0, 40.0)));
        let origin = Point::from((1920.0, 0.0));
        let global = ol.to_global(origin);
        assert_eq!(
            global.as_rect(),
            Rectangle::new(Point::from((1930.0, 20.0)), Size::from((30.0, 40.0)))
        );
        assert_eq!(global.to_output_local(origin).as_rect(), ol.as_rect());
    }
}
