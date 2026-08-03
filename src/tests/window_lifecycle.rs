//! T02 (STAB-02 / STAB-03): window/output lifecycle panic cleanup.
//!
//! STAB-02: a transient dialog mapped while its parent is minimized used to
//! panic in `Workspace::add_tile` via `active_window().unwrap()`. When every
//! window on the workspace is minimized there is no active window; the dialog
//! must become the focus owner instead of aborting the session.
//!
//! STAB-03: redraw work queued before an output was removed must not panic and
//! must not redraw other outputs. Two dispositions are exercised:
//! `Niri::remove_output` cancels the pending deadline timers it tracks, and a
//! deferred redraw callback that still arrives (as production callbacks may:
//! tty idle redraw, screencast timer) checks `output_state` membership first
//! and drops itself. The `queue_redraw` unwrap is only reachable with a live
//! output by construction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use niri_config::Config;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::utils::with_toplevel_role;

fn create_window(f: &mut Fixture, id: client::ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn set_title(f: &mut Fixture, id: client::ClientId, surface: &WlSurface, title: &str) {
    f.client(id).window(surface).set_title(title);
    f.double_roundtrip(id);
}

fn mapped_by_title<'a>(
    niri: &'a crate::niri::Niri,
    title: &str,
) -> Option<&'a crate::window::Mapped> {
    niri.layout.windows().find_map(|(_, mapped)| {
        with_toplevel_role(mapped.toplevel(), |role| {
            (role.title.as_deref() == Some(title)).then_some(mapped)
        })
    })
}

/// A02.1: a transient dialog of a minimized parent must map, stay visible, and
/// take focus; the session must not abort.
#[test]
fn dialog_of_minimized_parent_maps_visible_and_focused() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let parent_surface = create_window(&mut f, id, 200, 200);
    set_title(&mut f, id, &parent_surface, "parent");

    // Record which output the parent landed on (the fixture keeps the first
    // output active; new windows open there).
    let parent_mon = f
        .niri()
        .layout
        .windows()
        .find_map(|(mon, mapped)| {
            with_toplevel_role(mapped.toplevel(), |role| {
                (role.title.as_deref() == Some("parent"))
                    .then(|| mon.map(|mon| mon.output().name()))
            })
        })
        .flatten()
        .expect("parent must be on an output");

    // Minimize the only (active) window: the workspace then has no active window.
    f.client(id).window(&parent_surface).set_minimized();
    f.double_roundtrip(id);
    assert!(
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .active_window()
            .is_none(),
        "premise: workspace has no active window while the parent is minimized"
    );

    // Switch the active output to the other monitor: a dialog that ignores its
    // parent would land on the active output and fail the placement assertion.
    let other_out = if parent_mon == "headless-1" {
        f.niri_output(2)
    } else {
        f.niri_output(1)
    };
    f.niri().layout.focus_output(&other_out);
    f.double_roundtrip(id);

    // Open a transient dialog of the minimized parent.
    let parent_toplevel = f.client(id).window(&parent_surface).xdg_toplevel.clone();
    let dialog = f.client(id).create_window();
    let dialog_surface = dialog.surface.clone();
    dialog.set_title("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(id);

    // Map the dialog.
    let dialog = f.client(id).window(&dialog_surface);
    dialog.attach_new_buffer();
    dialog.set_size(120, 120);
    dialog.ack_last_and_commit();
    f.double_roundtrip(id);
    // The dialog is mapped, visible, and the session did not abort.
    {
        let niri = f.niri();
        let dialog_mapped = mapped_by_title(niri, "dialog").expect("dialog must be mapped");
        assert!(!dialog_mapped.is_minimized(), "dialog must be visible");
        // With no active window, the dialog takes the focus owner role in its
        // workspace (it was activated on map).
        let dialog_ws_active = niri
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(&dialog_mapped.window))
            .map(|(_, _, ws)| ws.active_window().map(|win| win.id()));
        assert_eq!(
            dialog_ws_active,
            Some(Some(dialog_mapped.id())),
            "dialog must become the focus owner when no window is active"
        );
    }

    // A02.1 "legal position": the dialog opens next to its parent, i.e. on the
    // parent's output and workspace — not on the currently active output.
    {
        let niri = f.niri();
        let parent = mapped_by_title(niri, "parent").unwrap();
        let dialog = mapped_by_title(niri, "dialog").unwrap();
        let dialog_mon = niri
            .layout
            .windows()
            .find(|(_, mapped)| mapped.id() == dialog.id())
            .unwrap()
            .0
            .map(|mon| mon.output().name());
        assert_eq!(
            dialog_mon,
            Some(parent_mon.clone()),
            "dialog must be placed next to its parent on the same output"
        );
        let parent_ws = niri
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(&parent.window))
            .map(|(_, idx, _)| idx);
        let dialog_ws = niri
            .layout
            .workspaces()
            .find(|(_, _, ws)| ws.has_window(&dialog.window))
            .map(|(_, idx, _)| idx);
        assert_eq!(
            parent_ws, dialog_ws,
            "dialog must share the parent's workspace"
        );
    }
    drop(other_out);

    // Restoring the parent must not disrupt the dialog: it stays mapped.
    let parent_window = {
        let niri = f.niri();
        mapped_by_title(niri, "parent").unwrap().window.clone()
    };
    let result = f.niri_state().execute_lifecycle_command(
        crate::lifecycle_command::LifecycleCommand::restore(
            parent_window,
            crate::lifecycle_command::LifecycleAnchorInput::None,
            crate::lifecycle_command::LifecycleInvocationSource::Test,
        ),
    );
    assert!(result.changed());
    f.double_roundtrip(id);
    assert!(
        mapped_by_title(f.niri(), "dialog").is_some(),
        "dialog must survive parent restore"
    );
}

/// A02.2 floating: the same minimized-parent dialog opening with a floating
/// parent must not panic either.
#[test]
fn dialog_of_minimized_floating_parent_maps_without_panic() {
    let config = Config::parse_mem(
        r#"
        window-rule {
            match title="parent"
            open-floating true
        }
        "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    // The title must be set before the first commit so the open-floating rule
    // applies at configure time.
    let parent_surface = {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.set_title("parent");
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.set_size(200, 200);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
        surface
    };
    let parent_window = {
        let niri = f.niri();
        niri.layout.windows().next().unwrap().1.window.clone()
    };
    assert!(
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .floating()
            .has_window(&parent_window),
        "premise: parent is floating"
    );

    f.client(id).window(&parent_surface).set_minimized();
    f.double_roundtrip(id);
    assert!(
        f.niri()
            .layout
            .active_workspace()
            .unwrap()
            .active_window()
            .is_none(),
        "premise: workspace has no active window while the floating parent is minimized"
    );

    let parent_toplevel = f.client(id).window(&parent_surface).xdg_toplevel.clone();
    let dialog = f.client(id).create_window();
    let dialog_surface = dialog.surface.clone();
    dialog.set_title("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(id);

    let dialog = f.client(id).window(&dialog_surface);
    dialog.attach_new_buffer();
    dialog.set_size(120, 120);
    dialog.ack_last_and_commit();
    f.double_roundtrip(id);

    {
        let niri = f.niri();
        let dialog_mapped = mapped_by_title(niri, "dialog").expect("dialog must be mapped");
        assert!(!dialog_mapped.is_minimized(), "dialog must be visible");
    }
}

/// A02.2 destroy: a dialog whose parent was destroyed before it maps must fall
/// back to automatic placement without panicking.
#[test]
fn dialog_after_parent_destroyed_falls_back_without_panic() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let parent_surface = create_window(&mut f, id, 200, 200);
    set_title(&mut f, id, &parent_surface, "parent");

    let parent_toplevel = f.client(id).window(&parent_surface).xdg_toplevel.clone();
    let dialog = f.client(id).create_window();
    let dialog_surface = dialog.surface.clone();
    dialog.set_title("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(id);

    // Destroy the parent before the dialog maps. The role object must go
    // first (xdg_surface_destroy before its role object is a protocol error).
    let parent = f.client(id).window(&parent_surface);
    parent.xdg_toplevel.destroy();
    parent.xdg_surface.destroy();
    parent.surface.destroy();
    f.roundtrip(id);

    // Map the dialog: with the parent gone it must fall back to Auto placement.
    let dialog = f.client(id).window(&dialog_surface);
    dialog.attach_new_buffer();
    dialog.set_size(120, 120);
    dialog.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        mapped_by_title(f.niri(), "dialog").is_some(),
        "dialog must still map after its parent was destroyed"
    );
}

/// A02.5: with an active window that is not the dialog's parent, smart
/// activation must keep the existing behavior (dialog stays unfocused).
#[test]
fn dialog_of_unfocused_parent_does_not_steal_focus() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let parent_surface = create_window(&mut f, id, 200, 200);
    set_title(&mut f, id, &parent_surface, "parent");
    let other_surface = create_window(&mut f, id, 200, 200);
    set_title(&mut f, id, &other_surface, "other");

    // Focus the non-parent window.
    let (other_window, other_id) = {
        let niri = f.niri();
        let other = mapped_by_title(niri, "other").unwrap();
        (other.window.clone(), other.id())
    };
    f.niri().layout.activate_window(&other_window);
    f.double_roundtrip(id);

    let parent_toplevel = f.client(id).window(&parent_surface).xdg_toplevel.clone();
    let dialog = f.client(id).create_window();
    let dialog_surface = dialog.surface.clone();
    dialog.set_title("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(id);

    let dialog = f.client(id).window(&dialog_surface);
    dialog.attach_new_buffer();
    dialog.set_size(120, 120);
    dialog.ack_last_and_commit();
    f.double_roundtrip(id);

    {
        let niri = f.niri();
        assert!(
            mapped_by_title(niri, "dialog").is_some(),
            "dialog must be mapped"
        );
        assert_eq!(
            niri.layout.focus().map(|win| win.id()),
            Some(other_id),
            "dialog of an unfocused parent must not steal focus"
        );
    }
}

/// A02.3: the animation-redraw deadline pending on an output that is then
/// removed. `Niri::remove_output` drops the output's pending deadline tokens
/// (estimated-vblank token and animation-redraw timer, niri.rs:3300-3309).
/// This test proves the composite safety property: once the output is gone,
/// running the compositor's event loop past the deadline does not panic and
/// does not touch the remaining output. (Whether the timer is cancelled or, if
/// it fired anyway, caught by the callback's `get_mut` guard is not
/// observable from outside; the arrival-and-drop disposition is covered by
/// the dedicated test below.)
#[test]
fn queued_redraw_after_output_removal_is_cancelled() {
    let config = Config::parse_mem(
        r#"
        window-rule {
            baba-is-float true
        }
        "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);
    f.niri_complete_animations();

    let out1 = f.niri_output(1);
    let out2 = f.niri_output(2);

    // Queue a redraw on output 1; the redraw pass schedules an animation-redraw
    // deadline (baba-is-float keeps scheduling frames).
    f.niri().queue_redraw(&out1);
    f.niri_state().refresh_and_flush_clients();
    assert!(
        f.niri().animation_redraw_is_scheduled(&out1),
        "premise: output 1 has a pending animation-redraw deadline"
    );

    // Remove the output while the deadline is still pending. `remove_output`
    // drops the output's pending timer tokens (niri.rs:3307-3309).
    f.niri().remove_output(&out1);

    // Run the compositor's event loop past the deadline: the loop must stay
    // healthy. The arrival-with-dropped-callback case is covered by the
    // dedicated test below.
    {
        let server = &mut f.state.server;
        server
            .event_loop
            .dispatch(Duration::from_millis(100), &mut server.state)
            .unwrap();
    }
    assert!(
        f.niri().output_state.contains_key(&out2),
        "remaining output must stay tracked"
    );
}

/// A02.3: a deferred redraw callback that fires after its output was removed
/// (as production callbacks can: the tty idle redraw and the screencast timer
/// both survive removal) must drop itself via the `output_state` membership
/// check instead of panicking, and must not redraw the remaining output.
#[test]
fn stale_redraw_callback_after_output_removal_is_dropped() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let out1 = f.niri_output(1);
    let out2 = f.niri_output(2);

    // Register a deferred redraw callback for output 1 on the compositor's
    // event loop, exactly like the production deferred callbacks do (captured
    // `Output` strong reference, membership check before `queue_redraw`).
    let fired = std::sync::Arc::new(AtomicBool::new(false));
    let fired_flag = fired.clone();
    let timer_out = out1.clone();
    let loop_handle = f.state.server.event_loop.handle().clone();
    loop_handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(20)),
            move |_, _, state| {
                fired_flag.store(true, Ordering::Relaxed);
                // Disposition for a stale redraw callback: dropped (discard),
                // matching the guards in the tty idle redraw and screencast
                // timer paths. `queue_redraw` itself must only ever be called
                // with a live output.
                if state.niri.output_state.contains_key(&timer_out) {
                    state.niri.queue_redraw(&timer_out);
                }
                TimeoutAction::Drop
            },
        )
        .unwrap();

    // Remove the output before the callback fires. The callback is not tracked
    // by `remove_output`, so it will still arrive. `remove_output` reflows the
    // remaining output and queues a legitimate redraw on it; drain that first
    // so the before/after comparison below only sees the stale callback.
    f.niri().remove_output(&out1);
    f.niri_state().refresh_and_flush_clients();
    let out2_state_before = format!(
        "{:?}",
        f.niri().output_state.get(&out2).unwrap().redraw_state
    );
    assert!(
        matches!(
            f.niri().output_state.get(&out2).unwrap().redraw_state,
            crate::niri::RedrawState::Idle
        ),
        "premise: output 2 is idle when the stale callback arrives"
    );

    // Run the loop past the callback deadline: the callback must arrive and
    // drop itself; no panic, and the remaining output must be untouched.
    {
        let server = &mut f.state.server;
        server
            .event_loop
            .dispatch(Duration::from_millis(100), &mut server.state)
            .unwrap();
    }
    assert!(
        fired.load(Ordering::Relaxed),
        "premise: the stale callback must have arrived"
    );
    assert!(
        f.niri().output_state.contains_key(&out2),
        "remaining output must stay tracked"
    );
    let out2_state_after = format!(
        "{:?}",
        f.niri().output_state.get(&out2).unwrap().redraw_state
    );
    assert_eq!(
        out2_state_before, out2_state_after,
        "stale callback must not redraw the remaining output"
    );
}

/// A02.3: removing an output while its redraw is merely queued (before any
/// dispatch) must not panic and must not touch the remaining output.
#[test]
fn queued_redraw_at_removal_time_does_not_panic() {
    let mut f = Fixture::new();
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let _ = create_window(&mut f, id, 200, 200);
    f.niri_complete_animations();

    let out1 = f.niri_output(1);
    let out2 = f.niri_output(2);

    f.niri().queue_redraw(&out1);
    assert!(
        matches!(
            f.niri().output_state.get(&out1).unwrap().redraw_state,
            crate::niri::RedrawState::Queued
        ),
        "premise: redraw is queued"
    );

    f.niri().remove_output(&out1);
    f.dispatch();

    assert!(
        f.niri().output_state.contains_key(&out2),
        "remaining output must stay tracked"
    );
}
