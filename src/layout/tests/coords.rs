//! R02 / F03: typed coordinate conversion and Genie endpoint consistency.

use smithay::utils::{Logical, Point, Rectangle, Size};

use super::*;
use crate::layout::coords::{
    GenieEndpointResolve, OutputLocalRect, SurfaceLocalRect, WorkspaceContentPoint,
};
use crate::layout::minimize_window_animation::tests_export::genie_area_for_test;

fn resolve_endpoints(
    view_pos: f64,
    window_content_x: f64,
    window_y: f64,
    anchor: OutputLocalRect,
) -> (f64, f64, Rectangle<f64, Logical>) {
    let content = WorkspaceContentPoint::from_point(Point::from((window_content_x, window_y)));
    let view = content.to_view(view_pos);
    let resolve = GenieEndpointResolve::identity();
    let window = resolve.window_from_view_pos(view.as_point());
    let anchor_f = resolve.anchor_from_output_local(anchor);
    let window_rect = Rectangle::new(window.as_point(), Size::from((200.0, 150.0)));
    let area = genie_area_for_test(window_rect, anchor_f.as_rect());
    (window.as_point().x, anchor_f.loc().x, area)
}

#[test]
fn f03_case_view_pos_1000_window_100_anchor_900() {
    let anchor = OutputLocalRect::new(Point::from((900, 700)), Size::from((48, 48)));
    // content x = view_pos + screen x
    let (win_x, dock_x, area) = resolve_endpoints(1000.0, 1100.0, 80.0, anchor);
    assert_eq!(win_x, 100.0);
    assert_eq!(dock_x, 900.0);
    // Genie area must cover both on-screen endpoints (not dock at -100).
    assert!(area.loc.x <= 100.0);
    assert!(area.loc.x + area.size.w >= 900.0 + 48.0);
    assert!(
        area.loc.x > -50.0,
        "dock must not be off the left by view_pos: {area:?}"
    );
}

#[test]
fn f03_view_pos_zero_still_consistent() {
    let anchor = OutputLocalRect::new(Point::from((40, 700)), Size::from((48, 48)));
    let (win_x, dock_x, _) = resolve_endpoints(0.0, 120.0, 50.0, anchor);
    assert_eq!(win_x, 120.0);
    assert_eq!(dock_x, 40.0);
}

#[test]
fn f03_negative_view_pos() {
    let anchor = OutputLocalRect::new(Point::from((500, 600)), Size::from((32, 32)));
    // view_pos negative (overscroll left): content 50 → view 66 when view_pos = -16
    let (win_x, dock_x, area) = resolve_endpoints(-16.0, 50.0, 40.0, anchor);
    assert_eq!(win_x, 66.0);
    assert_eq!(dock_x, 500.0);
    assert!(area.loc.x <= win_x.min(dock_x));
    assert!(area.loc.x + area.size.w >= win_x.max(dock_x) + 32.0);
}

#[test]
fn f03_large_positive_view_pos() {
    let anchor = OutputLocalRect::new(Point::from((1800, 900)), Size::from((40, 40)));
    let (win_x, dock_x, area) = resolve_endpoints(5000.0, 5120.0, 10.0, anchor);
    assert_eq!(win_x, 120.0);
    assert_eq!(dock_x, 1800.0);
    assert!(area.loc.x + area.size.w >= 1840.0);
}

#[test]
fn surface_local_to_output_local_matches_handler() {
    let surface = SurfaceLocalRect::new(Point::from((10, 20)), Size::from((30, 40)));
    // layer at bottom of 720p output: y = 720 - 80 = 640
    let layer = Rectangle::new(Point::from((0, 640)), Size::from((200, 80)));
    let out = surface.to_output_local(layer);
    assert_eq!(
        out.as_rect(),
        Rectangle::new(Point::from((10, 660)), Size::from((30, 40)))
    );
}

#[test]
fn minimize_rect_encodes_output_local_space() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    let output = layout.outputs().next().unwrap().clone();
    let rect = MinimizeRect {
        output,
        rect: OutputLocalRect::new(Point::from((24, 680)), Size::from((48, 48))),
    };
    assert_eq!(rect.rect.loc(), Point::from((24, 680)));
    assert!(layout.minimize_window_with_target(&1, Some(rect.clone())));
    assert!(layout.restore_window_with_source(&1, Some(rect)));
}
