//! Opt-in drag-to-resize for the connection sidebar (issue #3).
//!
//! GNOME HIG treats a navigation sidebar as adaptive, fixed-logic navigation,
//! not a desktop splitter — Nautilus 43 even dropped its resizable sidebar for
//! exactly this reason. So drag-resize here is strictly **opt-in**
//! (`UiSettings::sidebar_drag_resize`, off by default) and leaves the default
//! Adwaita experience untouched.
//!
//! ## Approach
//!
//! The sidebar lives inside an [`adw::OverlaySplitView`], whose adaptive
//! collapse, overlay, pinning and swipe gestures we must not disturb — so we do
//! **not** swap it for a `GtkPaned`. Instead the sidebar's own widget is wrapped
//! in a horizontal box with a thin grab handle at its trailing edge, and a
//! [`gtk4::GestureDrag`] on that handle drives the split view's
//! `max-sidebar-width`. The container, and every piece of adaptive behaviour it
//! owns, stays exactly as it was.
//!
//! ## Width model (two fields)
//!
//! - `UiSettings::sidebar_min_width` is the **floor** — the Settings spin row
//!   drives it, and a drag can never go below it (it snaps back up).
//! - `UiSettings::sidebar_width` is the **current** width — a drag rewrites it,
//!   persisted on drag-*end* only (not live, to avoid thrashing the config).
//!
//! The ceiling is half the window width, re-evaluated on every drag so a
//! narrowed window re-clamps the sidebar down. All clamping goes through
//! [`rustconn_core::config::UiSettings::clamp_sidebar_width`], the single source
//! of truth shared with the startup and settings-apply paths.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::Orientation;
use libadwaita as adw;

use crate::state::{with_state, with_state_mut, SharedAppState};

/// Width of the grab handle strip in pixels. Thin enough not to steal visible
/// space, wide enough to be an easy pointer target (GNOME HIG comfortable hit
/// target is larger, but the handle only needs to catch the edge).
const HANDLE_WIDTH: i32 = 4;

/// Snap-back / re-clamp animation duration. Short enough to feel immediate,
/// long enough not to jump. Skipped entirely under reduced-motion.
const SNAP_MS: u32 = 150;

/// Wraps the sidebar widget in a drag-handle container and wires the gesture.
///
/// Returns the widget to hand to [`adw::OverlaySplitView::set_sidebar`]. The
/// handle is only *active* when the opt-in setting is on and the split view is
/// not collapsed (narrow window); when the setting is off it is inert and
/// invisible, so the default experience is unchanged.
pub fn wrap_with_handle(
    sidebar_widget: &gtk4::Box,
    split_view: &adw::OverlaySplitView,
    state: &SharedAppState,
) -> gtk4::Box {
    let container = gtk4::Box::new(Orientation::Horizontal, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);

    // The sidebar fills the row; the handle hugs its trailing edge.
    sidebar_widget.set_hexpand(true);
    container.append(sidebar_widget);

    let handle = gtk4::DrawingArea::new();
    handle.set_width_request(HANDLE_WIDTH);
    handle.set_vexpand(true);
    handle.add_css_class("sidebar-resize-handle");
    // A column-resize cursor advertises the affordance only on hover.
    handle.set_cursor_from_name(Some("col-resize"));
    container.append(&handle);

    let drag = gtk4::GestureDrag::new();
    // The width the sidebar had when this drag began; the gesture reports an
    // offset, so we add it to this baseline rather than reading a live width
    // mid-drag (which the animation could be moving).
    let start_width: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));

    {
        let split_view = split_view.clone();
        let state = state.clone();
        let start_width = start_width.clone();
        drag.connect_drag_begin(move |_gesture, _x, _y| {
            // Inert unless opted in and shown wide.
            if !is_active(&split_view, &state) {
                return;
            }
            start_width.set(split_view.max_sidebar_width());
        });
    }

    {
        let split_view = split_view.clone();
        let state = state.clone();
        let start_width = start_width.clone();
        drag.connect_drag_update(move |_gesture, offset_x, _offset_y| {
            if !is_active(&split_view, &state) {
                return;
            }
            let candidate = (start_width.get() + offset_x).round() as i32;
            let window_width = split_view.width();
            let clamped =
                with_state(&state, |s| s.settings().ui.clamp_sidebar_width(candidate, window_width));
            // Live visual feedback follows the pointer within the allowed band.
            split_view.set_max_sidebar_width(f64::from(clamped));
        });
    }

    {
        let split_view = split_view.clone();
        let state = state.clone();
        drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
            if !is_active(&split_view, &state) {
                return;
            }
            let window_width = split_view.width();
            let current = split_view.max_sidebar_width().round() as i32;
            let final_width =
                with_state(&state, |s| s.settings().ui.clamp_sidebar_width(current, window_width));

            // Snap back up to the floor if the pointer was dragged below it.
            animate_to(&split_view, f64::from(final_width));

            // Persist the current width only (the floor is owned by Settings).
            with_state_mut(&state, |s| {
                let mut settings = s.settings().clone();
                settings.ui.sidebar_width = Some(final_width);
                if let Err(e) = s.update_settings(settings) {
                    tracing::warn!(error = %e, "failed to persist sidebar width after drag");
                }
            });
        });
    }

    handle.add_controller(drag);
    container
}

/// Whether the handle should respond right now: opted in, and the split view is
/// showing the sidebar in its wide (non-collapsed) layout. On a narrow window
/// the sidebar is an overlay driven by swipe gestures, where a drag handle
/// would fight the show/hide gesture — so it stays inert there.
fn is_active(split_view: &adw::OverlaySplitView, state: &SharedAppState) -> bool {
    with_state(state, |s| s.settings().ui.sidebar_drag_resize) && !split_view.is_collapsed()
}

/// Animates `max-sidebar-width` to `target`, or sets it instantly under
/// reduced-motion. Uses an [`adw::TimedAnimation`] on a throwaway callback
/// target so the property is stepped each frame.
fn animate_to(split_view: &adw::OverlaySplitView, target: f64) {
    let from = split_view.max_sidebar_width();
    if (from - target).abs() < 1.0 {
        split_view.set_max_sidebar_width(target);
        return;
    }

    // Respect the user's reduced-motion preference: no animation, just set it.
    if !gtk4::Settings::default()
        .map(|s| s.is_gtk_enable_animations())
        .unwrap_or(true)
    {
        split_view.set_max_sidebar_width(target);
        return;
    }

    let sv = split_view.clone();
    let callback = adw::CallbackAnimationTarget::new(move |value| {
        sv.set_max_sidebar_width(value);
    });
    let animation =
        adw::TimedAnimation::new(split_view, from, target, SNAP_MS, callback);
    animation.play();
    // Keep the animation alive until it finishes by moving it into its own
    // done handler's capture; `play()` holds a ref for the duration.
    animation.connect_done(move |_| {});
}
