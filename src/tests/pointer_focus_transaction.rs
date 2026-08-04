//! T04: pointer location cache coherence and the on-demand focus transaction.
//!
//! Two authorities are under test:
//!
//! 1. The `Niri::pointer_pos` cache is the single location authority for smithay callback contexts
//!    that cannot re-lock `PointerInternal` (T-31 / core 11605, core 3020). Every path that changes
//!    the effective pointer location — mouse motion, absolute motion, warp (`move_cursor`),
//!    pointer-constraint position hints — must keep the cache in sync, and the tablet cursor image
//!    callback must use the tablet's own location while a tool is in proximity (A04.1/A04.4/A04.5).
//!
//! 2. The on-demand layer-shell focus clear is deferred for presses on non-on-demand layer surfaces
//!    (T-29 first-click swallow) and must be keyed by the input identity (pointer button / touch
//!    slot / tablet tip) plus the pressed surface, so multi-button interleaves consume only their
//!    own releases and the death of the press itself (lock, VT switch, layer destroy, touch cancel,
//!    tablet proximity-out, device removal) terminates the transaction without leaving residue
//!    (A04.2). Grab swaps (pick color, screenshot, popup grab) do not terminate it: the held
//!    press's release still arrives at the input layer and completes the pair.
//!
//! All interaction tests drive the real production entry points:
//! `State::process_input_event` with a minimal fake input backend (all unused
//! event types are `UnusedEvent`), `Niri::move_cursor`, the real smithay
//! callback trait methods, and the real grab machinery.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use smithay::backend::input::{
    AbsolutePositionEvent, ButtonState, Device, DeviceCapability, Event, InputBackend, InputEvent,
    ProximityState, TabletToolAxisEvent, TabletToolCapabilities, TabletToolDescriptor,
    TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState,
    TabletToolType, TouchCancelEvent, TouchDownEvent, TouchEvent, TouchSlot, TouchUpEvent,
    UnusedEvent,
};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
    MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    Anchor, KeyboardInteractivity,
};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::{ClientId, LayerConfigureProps, LayerMargin};
use super::*;
use crate::input::backend_ext::NiriInputDevice;
use crate::niri::{KeyboardFocus, RedrawState, State};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Fractional tablet coordinates (of the global bounding rectangle) that put
/// the tool over the center of the mapped popup ((250, 150)) in the
/// single-output tablet fixtures (1920x1080 global rectangle).
const TABLET_POPUP_FX: f64 = 250. / 1920.;
const TABLET_POPUP_FY: f64 = 150. / 1080.;

/// Bring the test tool into proximity over the popup center.
fn tablet_proximity_in_popup(f: &mut Fixture) {
    tablet_proximity_in(f, TABLET_POPUP_FX, TABLET_POPUP_FY);
    tablet_axis(f, TABLET_POPUP_FX, TABLET_POPUP_FY);
}

/// Map a full-width on-demand bar (the shell's focus-holding surface).
fn map_bar(f: &mut Fixture, id: ClientId) -> WlSurface {
    // Output name events reach the client only after a roundtrip; the
    // fixture's map_layer looks up the output by its wl_output name.
    f.double_roundtrip(id);
    f.map_layer(
        id,
        Some(1),
        Layer::Top,
        "t04-bar",
        LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Right | Anchor::Top),
            size: Some((0, 50)),
            exclusive_zone: Some(40),
            kb_interactivity: Some(KeyboardInteractivity::OnDemand),
            ..Default::default()
        },
        (1920, 50),
    )
}

/// Map a small non-on-demand layer surface (a shell popup / dismiss layer).
fn map_popup(f: &mut Fixture, id: ClientId) -> WlSurface {
    f.map_layer(
        id,
        Some(1),
        Layer::Overlay,
        "t04-popup",
        LayerConfigureProps {
            anchor: Some(Anchor::Left | Anchor::Top),
            size: Some((100, 100)),
            margin: Some(LayerMargin {
                top: 100,
                left: 200,
                ..Default::default()
            }),
            kb_interactivity: Some(KeyboardInteractivity::None),
            ..Default::default()
        },
        (100, 100),
    )
}

fn bar_center() -> Point<f64, Logical> {
    Point::from((960., 25.))
}

fn popup_center() -> Point<f64, Logical> {
    Point::from((250., 150.))
}

fn output_center(f: &mut Fixture, n: u8) -> Point<f64, Logical> {
    let output = f.niri_output(n);
    let geo = f
        .niri()
        .global_space
        .output_geometry(&output)
        .unwrap()
        .to_f64();
    Point::from((geo.loc.x + geo.size.w / 2., geo.loc.y + geo.size.h / 2.))
}

fn output_queued(f: &mut Fixture, n: u8) -> bool {
    let output = f.niri_output(n);
    matches!(
        f.niri().output_state.get(&output).unwrap().redraw_state,
        RedrawState::Queued
    )
}

/// Render (drain) an output's queued redraw through the headless backend.
fn render_output(f: &mut Fixture, n: u8) {
    let output = f.niri_output(n);
    if !output_queued(f, n) {
        return;
    }
    let state = f.niri_state();
    let headless = state.backend.headless();
    headless.render(&mut state.niri, &output);
}

fn assert_bar_holds_focus(f: &mut Fixture, what: &str) {
    assert!(
        f.niri().layer_shell_on_demand_focus.is_some(),
        "{what}: bar must still hold the on-demand focus"
    );
    assert!(
        matches!(f.niri().keyboard_focus, KeyboardFocus::LayerShell { .. }),
        "{what}: keyboard focus must be on the bar, got {:?}",
        f.niri().keyboard_focus
    );
}

fn assert_focus_cleared(f: &mut Fixture, what: &str) {
    assert!(
        f.niri().layer_shell_on_demand_focus.is_none(),
        "{what}: on-demand focus must be cleared"
    );
}

/// Press/release a pointer button at the current pointer location through the
/// real input pipeline.
fn pointer_button(f: &mut Fixture, button: u32, pressed: bool) {
    let state = f.niri_state();
    let event = TestButtonEvent {
        device: pointer_device(),
        button_code: button,
        state: if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        time: 0,
    };
    state.process_input_event::<TestInputBackend>(InputEvent::PointerButton { event });
    f.dispatch();
    // The compositor loop recomputes the keyboard focus on every cycle
    // (update_keyboard_focus in State::refresh); the test fixture does not
    // drive the real loop, so run the same production refresh here.
    f.niri_state().update_keyboard_focus();
}

/// Move the pointer (warp path) and settle the focus update.
fn move_pointer(f: &mut Fixture, pos: Point<f64, Logical>) {
    f.niri_state().move_cursor(pos);
    f.dispatch();
}

/// Give the bar the on-demand keyboard focus by clicking it.
fn focus_bar(f: &mut Fixture) {
    move_pointer(f, bar_center());
    pointer_button(f, BTN_LEFT, true);
    pointer_button(f, BTN_LEFT, false);
    assert_bar_holds_focus(f, "after clicking the bar");
}

/// Focus the bar, then press-and-hold `button` on the popup (deferral).
fn defer_press_on_popup(f: &mut Fixture, button: u32) {
    assert_bar_holds_focus(f, "before popup press");
    move_pointer(f, popup_center());
    pointer_button(f, button, true);
    // The deferred clear must not have fired at press time.
    assert_bar_holds_focus(f, "while the popup press is held");
}

fn touch_device() -> TestDevice {
    TestDevice {
        id: 2,
        cap: Cap::Touch,
    }
}

fn tablet_device() -> TestDevice {
    TestDevice {
        id: 3,
        cap: Cap::Tablet,
    }
}

fn pointer_device() -> TestDevice {
    TestDevice {
        id: 1,
        cap: Cap::Pointer,
    }
}

/// A fixed tablet tool descriptor used by all tablet test events.
fn test_tool() -> TabletToolDescriptor {
    TabletToolDescriptor {
        tool_type: TabletToolType::Pen,
        hardware_serial: 0xdead_beef,
        hardware_id_wacom: 0x1234,
        capabilities: TabletToolCapabilities::empty(),
    }
}

fn touch_down(f: &mut Fixture, slot: u32, pos: Point<f64, Logical>) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TouchDown {
        event: TestTouchDownEvent {
            device: touch_device(),
            slot,
            x: pos.x / 1920.,
            y: pos.y / 1080.,
            time: 0,
        },
    });
    f.dispatch();
    // The compositor loop recomputes the keyboard focus on every cycle
    // (update_keyboard_focus in State::refresh); see `pointer_button`.
    f.niri_state().update_keyboard_focus();
}

fn touch_up(f: &mut Fixture, slot: u32) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TouchUp {
        event: TestTouchUpEvent {
            device: touch_device(),
            slot,
            time: 0,
        },
    });
    f.dispatch();
    // The compositor loop recomputes the keyboard focus on every cycle
    // (update_keyboard_focus in State::refresh); see `pointer_button`.
    f.niri_state().update_keyboard_focus();
}

fn touch_cancel(f: &mut Fixture) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TouchCancel {
        event: TestTouchCancelEvent {
            device: touch_device(),
            time: 0,
        },
    });
    f.dispatch();
    // The compositor loop recomputes the keyboard focus on every cycle
    // (update_keyboard_focus in State::refresh); see `pointer_button`.
    f.niri_state().update_keyboard_focus();
}

/// Bring a tablet tool into proximity at a fractional position of the global
/// bounding rectangle, then optionally move it.
fn tablet_proximity_in(f: &mut Fixture, fx: f64, fy: f64) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TabletToolProximity {
        event: TestTabletProximityEvent {
            device: tablet_device(),
            tool: test_tool(),
            state: ProximityState::In,
            x: fx,
            y: fy,
            time: 0,
        },
    });
    f.dispatch();
}

fn tablet_axis(f: &mut Fixture, fx: f64, fy: f64) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TabletToolAxis {
        event: TestTabletAxisEvent {
            device: tablet_device(),
            tool: test_tool(),
            x: fx,
            y: fy,
            time: 0,
        },
    });
    f.dispatch();
}

fn tablet_tip(f: &mut Fixture, down: bool) {
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TabletToolTip {
        event: TestTabletTipEvent {
            device: tablet_device(),
            tool: test_tool(),
            tip_state: if down {
                TabletToolTipState::Down
            } else {
                TabletToolTipState::Up
            },
            x: 0.5,
            y: 0.5,
            time: 0,
        },
    });
    f.dispatch();
    // The compositor loop recomputes the keyboard focus on every cycle
    // (update_keyboard_focus in State::refresh); see `pointer_button`.
    f.niri_state().update_keyboard_focus();
}

// ---------------------------------------------------------------------------
// A04.1 / A04.4: pointer location cache coherence
// ---------------------------------------------------------------------------

#[test]
fn warp_updates_cached_location_and_cursor_image_targets_new_output() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    // The cache starts at (0, 0) — output 1.
    assert_eq!(f.niri().pointer_pos, Point::from((0., 0.)));

    // Warp to output 2 (the `move_cursor` path used by warp-to-focus, IPC
    // focus actions, confirm-mru and tablet proximity-out).
    let target = output_center(&mut f, 2);
    move_pointer(&mut f, target);
    assert_eq!(
        f.niri().pointer_pos,
        target,
        "warp must keep the smithay-callback cache in sync"
    );

    // The warp itself redraws both the old and the new output; drain them so
    // the assertion below is about the cursor_image callback only.
    render_output(&mut f, 1);
    render_output(&mut f, 2);

    // The cursor_image callback runs while smithay holds PointerInternal's
    // mutex and can only use the cache; it must redraw the output the cursor
    // actually moved to.
    let seat = f.niri().seat.clone();
    SeatHandler::cursor_image(f.niri_state(), &seat, CursorImageStatus::default_named());
    assert!(
        output_queued(&mut f, 2),
        "cursor_image must redraw the output under the warped cursor"
    );
    assert!(
        !output_queued(&mut f, 1),
        "cursor_image must not redraw the stale output"
    );
}

#[test]
fn warp_cache_stays_correct_across_scale_and_transform_outputs() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output_with_scale_transform(1, (1280, 720), 1.0, smithay::utils::Transform::Normal);
    f.add_output_with_scale_transform(2, (2560, 1440), 2.0, smithay::utils::Transform::Flipped180);
    f.add_output_with_scale_transform(3, (1920, 1080), 1.5, smithay::utils::Transform::_90);

    // The cache is global logical coordinates regardless of the output's
    // scale/transform; each warp must land exactly on the requested global
    // position and the cursor_image callback must target the right output.
    for n in [1u8, 2, 3, 2, 1] {
        let target = output_center(&mut f, n);
        move_pointer(&mut f, target);
        assert_eq!(f.niri().pointer_pos, target, "cache drift on output {n}");

        // Drain previous redraws so the assertion is about this callback only.
        render_output(&mut f, 1);
        render_output(&mut f, 2);
        render_output(&mut f, 3);

        let seat = f.niri().seat.clone();
        SeatHandler::cursor_image(f.niri_state(), &seat, CursorImageStatus::default_named());
        for m in [1u8, 2, 3] {
            assert_eq!(
                output_queued(&mut f, m),
                m == n,
                "cursor_image must redraw exactly the output under the cursor (n={n}, m={m})"
            );
        }
    }
}

#[test]
fn color_pick_after_warp_reads_pixel_at_warped_position() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    // Fractional scale output: the pick conversion (to_physical_precise_floor)
    // must consume the global-logical cache coordinate correctly.
    f.add_output_with_scale_transform(1, (1920, 1080), 1.5, smithay::utils::Transform::Normal);

    let id = f.add_client();
    let _ = create_pick_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);

    let (_, mapped) = f.niri().layout.windows().next().unwrap();
    let geo = mapped.window.geometry();
    let window_center = Point::from((
        geo.loc.x as f64 + geo.size.w as f64 / 2.,
        geo.loc.y as f64 + geo.size.h as f64 / 2.,
    ));
    assert_eq!(window_center, Point::from((100., 100.)));

    // The window is focused, so its area renders as the focus ring color
    // (the ring is drawn with a background for client-side-decorated
    // windows); the desktop around it is gray. Warp to the window center —
    // the pick grab reads the *cache*, so a stale cache would pick the (0,0)
    // corner of the desktop instead.
    move_pointer(&mut f, window_center);

    let (tx, rx) = async_channel::unbounded();
    f.niri_state().handle_pick_color(tx);

    let state = f.niri_state();
    let pointer = state.niri.seat.get_pointer().unwrap();
    pointer.button(
        state,
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Pressed,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );

    let mut color = None;
    for _ in 0..100 {
        if let Ok(c) = rx.try_recv() {
            color = Some(c);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let color = color
        .expect("pick color must reply")
        .expect("pick must succeed");
    // The default active focus ring color is (0.498, 0.784, 1.0); the
    // desktop gradient is gray (r ≈ g ≈ b < 0.3), so the blue-ish assertion
    // discriminates the window area from the desktop.
    assert!(
        color.rgb[0] < 0.6 && color.rgb[1] > 0.7 && color.rgb[2] > 0.9,
        "pick must read the focused window area at the warped position, got {:?}",
        color.rgb
    );
}

/// Map a window filled with a single-pixel buffer (any color; the focused
/// window's area renders as the focus ring, see the pick test).
fn create_pick_window(f: &mut Fixture, id: ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    {
        let client = f.client(id);
        let buffer = client.state.spbm.as_ref().unwrap().create_u32_rgba_buffer(
            255,
            0,
            0,
            255,
            &client.qh,
            (),
        );
        surface.attach(Some(&buffer), 0, 0);
    }
    let window = f.client(id).window(&surface);
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    // The fixture does not drive the compositor loop, so the window would
    // stay frozen at the start of its open animation (alpha ~0); complete
    // animations like the production loop does.
    f.niri_complete_animations();
    surface
}

#[test]
fn tablet_tool_image_redraws_output_under_tablet_cursor() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    // Register the tablet and bring the tool into proximity on output 2
    // (fraction 0.75 of the 3840-wide bounding rectangle → x = 2880).
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: tablet_device(),
    });
    tablet_proximity_in(&mut f, 0.75, 0.5);
    assert_eq!(
        f.niri().tablet_cursor_location,
        Some(Point::from((2880., 540.))),
        "tablet cursor must be on output 2"
    );

    // The proximity-in handler redraws the output under the last mouse
    // position; drain both outputs so the assertion is about the callback.
    render_output(&mut f, 1);
    render_output(&mut f, 2);

    // tablet_tool_image is a smithay callback with the same re-entrancy
    // constraint as cursor_image; while the tablet is in proximity it must
    // redraw the output under the tablet cursor, not the last mouse position.
    TabletSeatHandler::tablet_tool_image(
        f.niri_state(),
        &test_tool(),
        CursorImageStatus::default_named(),
    );
    assert!(
        output_queued(&mut f, 2),
        "tablet_tool_image must redraw the output under the tablet cursor"
    );
    assert!(
        !output_queued(&mut f, 1),
        "tablet_tool_image must not redraw the stale mouse output"
    );
}

#[test]
fn cursor_position_hint_syncs_cache_and_redraws_target_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = create_pick_window(&mut f, id, 200, 200);

    // Lock the pointer on the window through the real protocol, then focus
    // the window (the constraint activates when the pointer focus lands on
    // its surface).
    let locked = f.client(id).state.lock_pointer(&surface);
    move_pointer(&mut f, Point::from((100., 100.)));

    // The hint target is the window's surface origin plus the hinted offset
    // (constrained to the output); the handler resolves the origin the same
    // way, so read the cached surface origin the test fixture already holds.
    let origin = f
        .niri()
        .pointer_contents
        .surface
        .as_ref()
        .map(|(_, loc)| *loc)
        .expect("pointer must be over the window");

    // The client moves the constrained pointer to (50, 50) within the window
    // and commits; the compositor delivers the hint through the real
    // PointerConstraintsHandler path.
    locked.set_cursor_position_hint(50., 50.);
    surface.commit();
    f.roundtrip(id);

    // The cache must reflect the hinted target (A04.1: every location-change
    // path keeps the smithay-callback cache in sync).
    assert_eq!(
        f.niri().pointer_pos,
        origin + Point::from((50., 50.)),
        "cursor_position_hint must sync the location cache to the hinted target"
    );

    // A second hint must sync the cache again.
    render_output(&mut f, 1);
    locked.set_cursor_position_hint(150., 150.);
    surface.commit();
    f.roundtrip(id);
    assert_eq!(
        f.niri().pointer_pos,
        origin + Point::from((150., 150.)),
        "second hint must sync the cache again"
    );
    // NB: the hint handler also queues a redraw for the hinted position, but
    // it is deliberately not asserted separately: the hint is delivered on
    // the carrying surface commit, which itself already queues one redraw
    // for the mapped toplevel on the same output, and the hint target is
    // always constrained to that same output — the hint's own redraw is
    // subsumed by the commit's redraw in every reachable scenario. Only the
    // cache sync is behaviorally pinned here.
}

// ---------------------------------------------------------------------------
// A04.2: on-demand focus transaction
// ---------------------------------------------------------------------------

#[test]
fn deferred_focus_clear_fires_only_at_last_matching_release() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);

    // Multi-button interleave: press left and right on the popup, then
    // release only the right button. The deferred clear belongs to the press
    // *pair*; releasing one button while the other is still held must not
    // drop the keyboard leave between the still-held press and its release.
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    pointer_button(&mut f, BTN_RIGHT, true);
    assert_bar_holds_focus(&mut f, "both popup buttons held");

    pointer_button(&mut f, BTN_RIGHT, false);
    assert_bar_holds_focus(&mut f, "after releasing the non-matching button");

    // Releasing the last deferred press completes the click; the clear fires.
    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "after releasing the last popup press");
}

#[test]
fn deferred_focus_clear_consumed_by_matching_button_only() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);

    // Press on the popup (deferred), then press (and release) a different
    // button on the desktop while the popup press is still held. The desktop
    // press must not clear the on-demand focus now: that would put the
    // keyboard leave between the popup press and its release and reintroduce
    // the T-29 first-click swallow. The clear is deferred to the popup
    // press's own release instead (A04.2 multi-button interleave).
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "popup press held");

    move_pointer(&mut f, Point::from((1000., 600.)));
    pointer_button(&mut f, BTN_RIGHT, true);
    assert_bar_holds_focus(
        &mut f,
        "desktop press must defer while the popup press is held",
    );
    pointer_button(&mut f, BTN_RIGHT, false);
    assert_bar_holds_focus(&mut f, "desktop release with no entry is a no-op");

    // The last pending release completes the popup click and fires the
    // deferred clear.
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "last pending release fires the deferred clear");

    // Re-focus the bar; a stale release must not clear it.
    focus_bar(&mut f);
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "stale release must not clear a new holder");
}

#[test]
fn window_press_with_held_popup_press_clears_immediately() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);
    let _ = create_pick_window(&mut f, id, 200, 200);

    focus_bar(&mut f);

    // Press on the popup (deferred), then press a button on the window while
    // the popup press is still held. Window presses keep the original T-29
    // trade-off and clear immediately: a deferred clear would leave a stale
    // on-demand holder that kills the window's first xdg popup grab (context
    // menus open on press).
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "popup press held");

    move_pointer(&mut f, Point::from((100., 100.)));
    pointer_button(&mut f, BTN_RIGHT, true);
    assert_focus_cleared(&mut f, "window press clears immediately");
    pointer_button(&mut f, BTN_RIGHT, false);

    // The popup press's stale release must not clear a future holder.
    focus_bar(&mut f);
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "stale release after window press");
}

#[test]
fn pick_color_grab_does_not_resolve_held_press_early() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);
    defer_press_on_popup(&mut f, BTN_LEFT);

    // A pick-color grab replaces the in-flight click grab, but the held
    // press's release still arrives at the input layer (the release hook
    // runs before pointer routing), so the click completes normally and its
    // deferred clear must fire at the release — resolving it at grab time
    // would put the keyboard leave between the press and its release (T-29
    // first-click swallow).
    let (tx, _rx) = async_channel::unbounded();
    f.niri_state().handle_pick_color(tx);
    assert_bar_holds_focus(
        &mut f,
        "pick color must not resolve the still-held press early",
    );

    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "the held press's release fires the deferred clear");

    // No residue: re-focusing the bar and a stale release must not clear it.
    focus_bar(&mut f);
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "stale release after pick color");
}

#[test]
fn pick_color_press_release_pair_completes_through_suppressed_release() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);

    // Start a pick-color grab (no pending press yet, so nothing resolves).
    let (tx, _rx) = async_channel::unbounded();
    f.niri_state().handle_pick_color(tx);
    assert_bar_holds_focus(&mut f, "pick color must not clear without a pending press");

    // Press on the popup while the pick-color grab is active: the press hook
    // records a deferred clear, then the pick grab consumes the press and
    // suppresses its release.
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "pick-color press on popup deferred");

    // The suppressed release must still complete the press pair (the release
    // hook runs before the suppressed-buttons early return): the deferred
    // clear fires at release, and no entry lingers for a stale release to
    // consume later.
    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(
        &mut f,
        "suppressed release must complete the pick-color press pair",
    );

    // No residue: re-focusing the bar and re-releasing must be a no-op.
    focus_bar(&mut f);
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "no residue after the completed pick-color pair");
}

#[test]
fn screenshot_open_does_not_resolve_held_press_early() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);
    defer_press_on_popup(&mut f, BTN_LEFT);

    // Opening the screenshot UI unsets pointer/touch grabs, but the held
    // press's release still arrives at the input layer and completes the
    // click; resolving it at screenshot time would put the keyboard leave
    // between the press and its release (T-29 first-click swallow).
    f.niri_state().open_screenshot_ui(false, None);
    assert_bar_holds_focus(
        &mut f,
        "screenshot open must not resolve the still-held press early",
    );

    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "the held press's release fires the deferred clear");
}

#[test]
fn deferred_focus_clear_resolves_on_lock_and_vt_pause() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);
    defer_press_on_popup(&mut f, BTN_LEFT);

    // The session locking / VT pause hooks call the same resolver; headless
    // tests cannot drive a real SessionLocker or session pause, so exercise
    // the resolver directly (the call sites are pinned by the static
    // contract tests below).
    f.niri().resolve_pending_on_demand_focus_clear();
    assert_focus_cleared(&mut f, "lock/VT pause must resolve the deferred clear");
    // The stale release must not clear a future holder.
    focus_bar(&mut f);
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "stale release after lock/VT pause");
}

#[test]
fn deferred_focus_clear_resolves_on_pressed_surface_destroy() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    let popup = map_popup(&mut f, id);

    focus_bar(&mut f);
    defer_press_on_popup(&mut f, BTN_LEFT);

    // Destroy the pressed layer surface mid-press: the click is dead, so the
    // deferred clear resolves through the layer destroy path.
    f.client(id).layer(&popup).layer_surface.destroy();
    f.roundtrip(id);
    assert_focus_cleared(&mut f, "pressed surface destroy must resolve the clear");
    // The stale release must not clear a future holder.
    focus_bar(&mut f);
    pointer_button(&mut f, BTN_LEFT, false);
    assert_bar_holds_focus(&mut f, "stale release after surface destroy");
}

#[test]
fn touch_down_up_mirrors_deferred_focus_clear_with_slot_interleave() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: touch_device(),
    });

    focus_bar(&mut f);

    // Touch down on the popup must defer (same first-click-swallow class as
    // the pointer path), not clear at down time.
    touch_down(&mut f, 0, popup_center());
    assert_bar_holds_focus(&mut f, "touch down on popup deferred");

    // Multi-slot interleave: a second finger down and up must not consume
    // the first slot's deferred clear.
    touch_down(&mut f, 1, popup_center());
    touch_up(&mut f, 1);
    assert_bar_holds_focus(&mut f, "unrelated slot up must not consume");

    touch_up(&mut f, 0);
    assert_focus_cleared(&mut f, "last slot up completes the touch click");
}

#[test]
fn touch_cancel_resolves_pending_slot() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: touch_device(),
    });

    focus_bar(&mut f);
    touch_down(&mut f, 0, popup_center());
    assert_bar_holds_focus(&mut f, "touch down deferred");

    touch_cancel(&mut f);
    assert_focus_cleared(&mut f, "touch cancel must resolve the deferred clear");
}

#[test]
fn touch_cancel_does_not_resolve_unrelated_pointer_press() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: touch_device(),
    });

    focus_bar(&mut f);

    // A pointer press on the popup defers its clear; then an unrelated touch
    // cancel (palm rejection) arrives while the pointer button is still held.
    // The touch cancel only ends touch clicks — resolving the still-alive
    // pointer press here would clear the on-demand focus between the pointer
    // press and release and reintroduce the T-29 first-click swallow.
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "pointer press deferred");

    touch_cancel(&mut f);
    assert_bar_holds_focus(
        &mut f,
        "touch cancel must not resolve an unrelated held pointer press",
    );

    // The pointer click completes normally and its own release fires the
    // deferred clear.
    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "pointer release completes the click");
}

#[test]
fn device_removal_resolves_pending_press_of_that_kind() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: touch_device(),
    });

    focus_bar(&mut f);
    touch_down(&mut f, 0, popup_center());
    assert_bar_holds_focus(&mut f, "touch down deferred");

    // The touch device is hot-unplugged mid-press: no release will ever
    // arrive, so the deferred clear must resolve now (A04.2 device
    // removal), otherwise the stale entry would poison the transaction and
    // stop outside-dismiss for later clicks.
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceRemoved {
        device: touch_device(),
    });
    f.dispatch();
    assert_focus_cleared(&mut f, "device removal must resolve its pending press");

    // No residue: re-focusing the bar and a stale slot-up must not clear it.
    focus_bar(&mut f);
    touch_up(&mut f, 0);
    assert_bar_holds_focus(&mut f, "stale slot-up after device removal");
}

#[test]
fn device_removal_does_not_resolve_another_devices_press() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    // A second pointer device (e.g. a second mouse).
    let other = TestDevice {
        id: 4,
        cap: Cap::Pointer,
    };
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: other.clone(),
    });

    focus_bar(&mut f);
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "press deferred");

    // Removing an *unrelated* pointer device must not resolve the still-held
    // press of the other device: that would put the keyboard leave between
    // its press and release (T-29 first-click swallow).
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceRemoved { device: other });
    f.dispatch();
    assert_bar_holds_focus(
        &mut f,
        "device removal must not resolve another device's held press",
    );

    pointer_button(&mut f, BTN_LEFT, false);
    assert_focus_cleared(&mut f, "the held press's own release fires the clear");
}

#[test]
fn touch_cancel_does_not_resolve_same_devices_tablet_press() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    // The touch seat is created by the touch device; the pen events use the
    // *same* device id as the touch cancel, modelling a mixed touchscreen/pen
    // device whose touch and tablet capabilities share one device node.
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: touch_device(),
    });
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: tablet_device(),
    });

    focus_bar(&mut f);
    tablet_proximity_in_popup(&mut f);
    tablet_tip(&mut f, true);
    assert_bar_holds_focus(&mut f, "tablet tip down deferred");

    // A palm-rejection touch cancel from the same device must only end its
    // touch clicks: the still-held pen tip press of the same device is a
    // different input kind and keeps its deferral (T-29 first-click
    // swallow).
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TouchCancel {
        event: TestTouchCancelEvent {
            device: tablet_device(),
            time: 0,
        },
    });
    f.dispatch();
    assert_bar_holds_focus(
        &mut f,
        "touch cancel must not resolve the same device's held pen tip press",
    );

    tablet_tip(&mut f, false);
    assert_focus_cleared(&mut f, "the tip-up completes the pen click");
}

#[test]
fn pointer_and_tablet_device_removal_resolve_their_pending_presses() {
    // Pointer device removal (hot-unplugging the mouse mid-press).
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    focus_bar(&mut f);
    move_pointer(&mut f, popup_center());
    pointer_button(&mut f, BTN_LEFT, true);
    assert_bar_holds_focus(&mut f, "pointer press deferred");

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceRemoved {
        device: pointer_device(),
    });
    f.dispatch();
    assert_focus_cleared(
        &mut f,
        "pointer device removal must resolve its pending press",
    );

    // Tablet device removal (hot-unplugging the tablet mid-tip-press).
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: tablet_device(),
    });

    focus_bar(&mut f);
    tablet_proximity_in_popup(&mut f);
    tablet_tip(&mut f, true);
    assert_bar_holds_focus(&mut f, "tablet tip down deferred");

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceRemoved {
        device: tablet_device(),
    });
    f.dispatch();
    assert_focus_cleared(
        &mut f,
        "tablet device removal must resolve its pending tip press",
    );
}

#[test]
fn tablet_tip_down_up_mirrors_deferred_focus_clear() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: tablet_device(),
    });

    focus_bar(&mut f);

    // Move the tool over the popup and touch down: must defer, not clear.
    tablet_proximity_in_popup(&mut f);
    tablet_tip(&mut f, true);
    assert_bar_holds_focus(&mut f, "tablet tip down deferred");

    tablet_tip(&mut f, false);
    assert_focus_cleared(&mut f, "tablet tip up completes the click");
}

#[test]
fn tablet_proximity_out_resolves_pending_tip() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_bar(&mut f, id);
    map_popup(&mut f, id);

    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::DeviceAdded {
        device: tablet_device(),
    });

    focus_bar(&mut f);
    tablet_proximity_in_popup(&mut f);
    tablet_tip(&mut f, true);
    assert_bar_holds_focus(&mut f, "tip down deferred");

    // The tool leaves proximity with the tip still pending: the click is
    // dead and the transaction must resolve.
    let state = f.niri_state();
    state.process_input_event::<TestInputBackend>(InputEvent::TabletToolProximity {
        event: TestTabletProximityEvent {
            device: tablet_device(),
            tool: test_tool(),
            state: ProximityState::Out,
            x: TABLET_POPUP_FX,
            y: TABLET_POPUP_FY,
            time: 0,
        },
    });
    f.dispatch();

    assert_focus_cleared(&mut f, "proximity out must resolve the deferred clear");
}

// ---------------------------------------------------------------------------
// A04.3: historical deadlock harnesses
// ---------------------------------------------------------------------------

/// A grab whose button callback runs while smithay holds `PointerInternal`'s
/// mutex — the exact context of the historical cursor_image deadlock
/// (T-31 / core 11605). It invokes the real `SeatHandler::cursor_image`.
struct CursorImageProbeGrab {
    start_data: PointerGrabStartData<State>,
}

impl PointerGrab<State> for CursorImageProbeGrab {
    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        // NB: this runs under the PointerInternal lock; re-locking via
        // current_location() would deadlock (the regression this harness
        // guards against).
        let seat = data.niri.seat.clone();
        SeatHandler::cursor_image(data, &seat, CursorImageStatus::default_named());
        handle.button(data, event);
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}

/// Watchdog: if a regression reintroduces the PointerInternal re-entrancy
/// deadlock, the main thread blocks forever in futex_wait; abort the suite
/// deterministically instead of hanging it.
///
/// An RAII guard sets the done flag on drop, which also covers panics, so a
/// failed (non-hung) test does not leave an orphan 60s abort pending over the
/// rest of the test binary.
struct DeadlockWatchdog {
    done: Arc<AtomicBool>,
}

impl DeadlockWatchdog {
    fn spawn() -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        thread::spawn(move || {
            for _ in 0..600 {
                thread::sleep(Duration::from_millis(100));
                if done_clone.load(Ordering::Relaxed) {
                    return;
                }
            }
            eprintln!(
                "DEADLOCK WATCHDOG FIRED: the test hung inside a smithay callback \
                 under the PointerInternal lock (re-entrancy regression)"
            );
            std::process::abort();
        });
        Self { done }
    }
}

impl Drop for DeadlockWatchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

#[test]
fn cursor_image_under_pointer_internal_lock_does_not_deadlock() {
    let _watchdog = DeadlockWatchdog::spawn();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let start_data = PointerGrabStartData {
        focus: None,
        button: BTN_LEFT,
        location: Point::from((0., 0.)),
    };
    let grab = CursorImageProbeGrab { start_data };

    let state = f.niri_state();
    let pointer = state.niri.seat.get_pointer().unwrap();
    pointer.set_grab(state, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    pointer.button(
        state,
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Pressed,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
    // Clean up the probe grab (the press above leaves it active).
    pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), 0);
}

#[test]
fn on_ungrab_under_pointer_internal_lock_does_not_deadlock() {
    let _watchdog = DeadlockWatchdog::spawn();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // The historical core-3020 context: a PointerGrab callback (on_ungrab)
    // invoked while smithay holds PointerInternal's mutex. Drive the real
    // PickColorGrab::unset through PointerHandle::unset_grab.
    let (tx, _rx) = async_channel::unbounded();
    f.niri_state().handle_pick_color(tx);

    let state = f.niri_state();
    let pointer = state.niri.seat.get_pointer().unwrap();
    pointer.unset_grab(state, SERIAL_COUNTER.next_serial(), 0);
}

// ---------------------------------------------------------------------------
// A04.5 / A04.2: static contract guardrails
// ---------------------------------------------------------------------------

/// Find the name of the function containing `line_idx` (0-based) in `src`.
fn enclosing_fn(src: &str, line_idx: usize) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut depth = 0i32;
    for i in (0..=line_idx).rev() {
        let line = lines[i];
        // Track brace depth of the line itself (open before close so a `fn`
        // on the same line as `{` counts).
        let opens = line.chars().filter(|c| *c == '{').count() as i32;
        let closes = line.chars().filter(|c| *c == '}').count() as i32;
        let prev_depth = depth;
        depth += opens - closes;
        if prev_depth == 0 && depth > 0 {
            // This line opens a block at depth 0 → the enclosing function.
            // Multi-line signatures put the `{` on a later line than the
            // `fn`, so scan further back for the `fn` line.
            let name = line
                .split("fn ")
                .nth(1)
                .and_then(|rest| rest.split(['(', '{', '<']).next())
                .or_else(|| {
                    lines[..=line_idx].iter().rev().find_map(|l| {
                        l.split("fn ")
                            .nth(1)
                            .and_then(|rest| rest.split(['(', '{', '<']).next())
                    })
                })
                .unwrap_or("<unknown>")
                .trim();
            return name.to_string();
        }
    }
    "<top-level>".to_string()
}

/// The `pointer_pos` cache is the single location authority (A04.5): it must
/// be written only by the four location-change paths, never by a second
/// cache.
#[test]
fn pointer_pos_writers_are_the_single_cache_authority() {
    let expected_writers = [
        "on_pointer_motion",
        "on_pointer_motion_absolute",
        "move_cursor",
        "cursor_position_hint",
    ];

    let mut found = Vec::new();
    for file in ["src/niri.rs", "src/input/mod.rs", "src/handlers/mod.rs"] {
        let src = std::fs::read_to_string(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), file))
            .unwrap_or_else(|e| panic!("reading {file}: {e}"));
        for (idx, line) in src.lines().enumerate() {
            if line.contains(".pointer_pos = ") {
                found.push(format!(
                    "{}:{} ({})",
                    file,
                    idx + 1,
                    enclosing_fn(&src, idx)
                ));
            }
        }
    }

    let mut writers: Vec<&str> = found
        .iter()
        .filter_map(|f| f.split(" (").nth(1).and_then(|s| s.strip_suffix(')')))
        .collect();
    writers.sort_unstable();
    let mut expected: Vec<&str> = expected_writers.to_vec();
    expected.sort_unstable();

    assert_eq!(
        writers, expected,
        "pointer_pos must have exactly the four known writers; found: {found:?}"
    );
}

/// The lifecycle resolve call sites for the focus transaction must exist
/// (A04.2: lock, VT switch, screenshot grab teardown). Headless tests cannot
/// drive a real SessionLocker or session pause, so the call sites are pinned
/// here as a static contract.
#[test]
fn focus_transaction_lifecycle_resolve_call_sites_exist() {
    // lock() must resolve the deferred clear.
    let niri_src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/niri.rs"))
        .expect("reading src/niri.rs");
    let lock_fn = niri_src
        .lines()
        .position(|l| l.starts_with("    pub fn lock("))
        .expect("Niri::lock");
    let lock_body = niri_src
        .lines()
        .skip(lock_fn + 1)
        .take_while(|l| !l.starts_with("    pub fn "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lock_body.contains("resolve_pending_on_demand_focus_clear"),
        "Niri::lock must resolve the pending focus transaction"
    );

    // open_screenshot_ui() must *not* resolve the held press early (the
    // release arrives at the input layer and completes the click); the
    // screenshot grabs do not kill deferred presses — pinned by the
    // behavioral test screenshot_open_does_not_resolve_held_press_early.

    // The TTY backend must resolve on session pause (VT switch).
    let tty_src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/backend/tty.rs"))
            .expect("reading src/backend/tty.rs");
    let pause = tty_src
        .lines()
        .position(|l| l.contains("SessionEvent::PauseSession"))
        .expect("PauseSession arm");
    let pause_body = tty_src
        .lines()
        .skip(pause)
        .take_while(|l| !l.trim_start().starts_with("SessionEvent::ActivateSession"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        pause_body.contains("resolve_pending_on_demand_focus_clear"),
        "PauseSession must resolve the pending focus transaction"
    );

    // The input backend must resolve a removed device's presses (A04.2
    // hot-plug).
    let input_src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/input/mod.rs"))
            .expect("reading src/input/mod.rs");
    let removed = input_src
        .lines()
        .position(|l| l.starts_with("    fn on_device_removed("))
        .expect("on_device_removed");
    let removed_body = input_src
        .lines()
        .skip(removed + 1)
        .take_while(|l| !l.starts_with("    fn "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        removed_body.contains("resolve_pending_on_demand_focus_clear_for_device"),
        "on_device_removed must resolve the removed device's deferred clears"
    );
}

// ---------------------------------------------------------------------------
// Minimal fake input backend
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Cap {
    Pointer,
    Touch,
    Tablet,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TestDevice {
    id: u32,
    cap: Cap,
}

impl Device for TestDevice {
    fn id(&self) -> String {
        format!("test-device-{}", self.id)
    }

    fn name(&self) -> String {
        format!("test device {}", self.id)
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(
            (self.cap, capability),
            (Cap::Pointer, DeviceCapability::Pointer)
                | (Cap::Touch, DeviceCapability::Touch)
                | (Cap::Tablet, DeviceCapability::TabletTool)
        )
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

impl NiriInputDevice for TestDevice {
    fn output(&self, _state: &State) -> Option<smithay::output::Output> {
        None
    }
}

struct TestInputBackend;

impl InputBackend for TestInputBackend {
    type Device = TestDevice;
    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerButtonEvent = TestButtonEvent;
    type PointerMotionEvent = TestMotionEvent;
    type PointerMotionAbsoluteEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = TestTouchDownEvent;
    type TouchUpEvent = TestTouchUpEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = TestTouchCancelEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = TestTabletAxisEvent;
    type TabletToolProximityEvent = TestTabletProximityEvent;
    type TabletToolTipEvent = TestTabletTipEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = ();
}

struct TestButtonEvent {
    device: TestDevice,
    button_code: u32,
    state: ButtonState,
    time: u64,
}

impl Event<TestInputBackend> for TestButtonEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl smithay::backend::input::PointerButtonEvent<TestInputBackend> for TestButtonEvent {
    fn button_code(&self) -> u32 {
        self.button_code
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

struct TestMotionEvent {
    device: TestDevice,
    dx: f64,
    dy: f64,
    time: u64,
}

impl Event<TestInputBackend> for TestMotionEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl smithay::backend::input::PointerMotionEvent<TestInputBackend> for TestMotionEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }

    fn delta_x_unaccel(&self) -> f64 {
        self.dx
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.dy
    }
}

struct TestTouchDownEvent {
    device: TestDevice,
    slot: u32,
    x: f64,
    y: f64,
    time: u64,
}

impl Event<TestInputBackend> for TestTouchDownEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TouchEvent<TestInputBackend> for TestTouchDownEvent {
    fn slot(&self) -> TouchSlot {
        Some(self.slot).into()
    }
}

impl TouchDownEvent<TestInputBackend> for TestTouchDownEvent {}

impl AbsolutePositionEvent<TestInputBackend> for TestTouchDownEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}

struct TestTouchUpEvent {
    device: TestDevice,
    slot: u32,
    time: u64,
}

impl Event<TestInputBackend> for TestTouchUpEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TouchEvent<TestInputBackend> for TestTouchUpEvent {
    fn slot(&self) -> TouchSlot {
        Some(self.slot).into()
    }
}

impl TouchUpEvent<TestInputBackend> for TestTouchUpEvent {}

struct TestTouchCancelEvent {
    device: TestDevice,
    time: u64,
}

impl Event<TestInputBackend> for TestTouchCancelEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TouchEvent<TestInputBackend> for TestTouchCancelEvent {
    fn slot(&self) -> TouchSlot {
        Some(0).into()
    }
}

impl TouchCancelEvent<TestInputBackend> for TestTouchCancelEvent {}

/// Fractional position (0..1) of the target coordinate space.
struct TestTabletAxisEvent {
    device: TestDevice,
    tool: TabletToolDescriptor,
    x: f64,
    y: f64,
    time: u64,
}

impl Event<TestInputBackend> for TestTabletAxisEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TabletToolEvent<TestInputBackend> for TestTabletAxisEvent {
    fn tool(&self) -> TabletToolDescriptor {
        self.tool.clone()
    }

    fn delta_x(&self) -> f64 {
        0.
    }

    fn delta_y(&self) -> f64 {
        0.
    }

    fn distance(&self) -> f64 {
        0.
    }

    fn distance_has_changed(&self) -> bool {
        false
    }

    fn pressure(&self) -> f64 {
        0.5
    }

    fn pressure_has_changed(&self) -> bool {
        false
    }

    fn slider_position(&self) -> f64 {
        0.
    }

    fn slider_has_changed(&self) -> bool {
        false
    }

    fn tilt_x(&self) -> f64 {
        0.
    }

    fn tilt_x_has_changed(&self) -> bool {
        false
    }

    fn tilt_y(&self) -> f64 {
        0.
    }

    fn tilt_y_has_changed(&self) -> bool {
        false
    }

    fn rotation(&self) -> f64 {
        0.
    }

    fn rotation_has_changed(&self) -> bool {
        false
    }

    fn wheel_delta(&self) -> f64 {
        0.
    }

    fn wheel_delta_discrete(&self) -> i32 {
        0
    }

    fn wheel_has_changed(&self) -> bool {
        false
    }
}

impl TabletToolAxisEvent<TestInputBackend> for TestTabletAxisEvent {}

impl AbsolutePositionEvent<TestInputBackend> for TestTabletAxisEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}

struct TestTabletProximityEvent {
    device: TestDevice,
    tool: TabletToolDescriptor,
    state: ProximityState,
    x: f64,
    y: f64,
    time: u64,
}

impl Event<TestInputBackend> for TestTabletProximityEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TabletToolEvent<TestInputBackend> for TestTabletProximityEvent {
    fn tool(&self) -> TabletToolDescriptor {
        self.tool.clone()
    }

    fn delta_x(&self) -> f64 {
        0.
    }

    fn delta_y(&self) -> f64 {
        0.
    }

    fn distance(&self) -> f64 {
        0.
    }

    fn distance_has_changed(&self) -> bool {
        false
    }

    fn pressure(&self) -> f64 {
        0.5
    }

    fn pressure_has_changed(&self) -> bool {
        false
    }

    fn slider_position(&self) -> f64 {
        0.
    }

    fn slider_has_changed(&self) -> bool {
        false
    }

    fn tilt_x(&self) -> f64 {
        0.
    }

    fn tilt_x_has_changed(&self) -> bool {
        false
    }

    fn tilt_y(&self) -> f64 {
        0.
    }

    fn tilt_y_has_changed(&self) -> bool {
        false
    }

    fn rotation(&self) -> f64 {
        0.
    }

    fn rotation_has_changed(&self) -> bool {
        false
    }

    fn wheel_delta(&self) -> f64 {
        0.
    }

    fn wheel_delta_discrete(&self) -> i32 {
        0
    }

    fn wheel_has_changed(&self) -> bool {
        false
    }
}

impl TabletToolProximityEvent<TestInputBackend> for TestTabletProximityEvent {
    fn state(&self) -> ProximityState {
        self.state
    }
}

impl AbsolutePositionEvent<TestInputBackend> for TestTabletProximityEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}

struct TestTabletTipEvent {
    device: TestDevice,
    tool: TabletToolDescriptor,
    tip_state: TabletToolTipState,
    x: f64,
    y: f64,
    time: u64,
}

impl Event<TestInputBackend> for TestTabletTipEvent {
    fn time(&self) -> u64 {
        self.time
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl TabletToolEvent<TestInputBackend> for TestTabletTipEvent {
    fn tool(&self) -> TabletToolDescriptor {
        self.tool.clone()
    }

    fn delta_x(&self) -> f64 {
        0.
    }

    fn delta_y(&self) -> f64 {
        0.
    }

    fn distance(&self) -> f64 {
        0.
    }

    fn distance_has_changed(&self) -> bool {
        false
    }

    fn pressure(&self) -> f64 {
        0.5
    }

    fn pressure_has_changed(&self) -> bool {
        false
    }

    fn slider_position(&self) -> f64 {
        0.
    }

    fn slider_has_changed(&self) -> bool {
        false
    }

    fn tilt_x(&self) -> f64 {
        0.
    }

    fn tilt_x_has_changed(&self) -> bool {
        false
    }

    fn tilt_y(&self) -> f64 {
        0.
    }

    fn tilt_y_has_changed(&self) -> bool {
        false
    }

    fn rotation(&self) -> f64 {
        0.
    }

    fn rotation_has_changed(&self) -> bool {
        false
    }

    fn wheel_delta(&self) -> f64 {
        0.
    }

    fn wheel_delta_discrete(&self) -> i32 {
        0
    }

    fn wheel_has_changed(&self) -> bool {
        false
    }
}

impl TabletToolTipEvent<TestInputBackend> for TestTabletTipEvent {
    fn tip_state(&self) -> TabletToolTipState {
        self.tip_state
    }
}

impl AbsolutePositionEvent<TestInputBackend> for TestTabletTipEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}
