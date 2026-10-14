//! Opt-in drag-to-resize for the connection sidebar (issue #3).
//!
//! libadwaita's adaptive sidebar has no draggable edge — GNOME Files lost its
//! draggable sidebar in GNOME 43 when it moved to libadwaita's adaptive
//! sidebar. So drag-resize here is strictly **opt-in**
//! (`UiSettings::sidebar_drag_resize`, off by default) and leaves the default
//! Adwaita experience untouched.
//!
//! ## Approach
//!
//! The sidebar lives inside an [`adw::OverlaySplitView`], whose adaptive
//! collapse, overlay, pinning and swipe gestures we must not disturb — so we do
//! **not** swap it for a `GtkPaned`. Instead the sidebar's own widget is wrapped
//! in a horizontal box with a thin grab handle at its inner edge, and a
//! [`gtk4::GestureDrag`] on that handle drives the split view's
//! `max-sidebar-width`. The container, and every piece of adaptive behaviour it
//! owns, stays exactly as it was.
//!
//! The gesture sits on the handle only for hit-testing and the cursor. The
//! handle moves with the edge, so its own offsets are measured in a moving
//! frame: adding them to a start width feeds every width change back into the
//! next update, and the edge flips between two positions on alternate frames.
//! So the width is never derived from the gesture's offset. Each update
//! translates the pointer into the split view's coordinates
//! (`compute_point`) — a frame that does not move when the sidebar resizes —
//! and [`width_from_pointer`] maps that position alone to a width. `GtkPaned`
//! gets the same effect by attaching its gesture to the stationary parent;
//! doing that here would make the split view see every drag in the content
//! area (terminal selection, tab drags).
//!
//! ## Width model (one value)
//!
//! `UiSettings::sidebar_width` is the only width, in the split view's
//! `sidebar-width-unit` (sp). The Settings spin row and
//! [`rustconn_core::config::clamp_sidebar_width`] use the plain
//! [`rustconn_core::config::SIDEBAR_WIDTH_MIN`]..=
//! [`rustconn_core::config::SIDEBAR_WIDTH_MAX`] range. A drag uses that range
//! narrowed to what libadwaita actually shows ([`drag_bounds`]): never below
//! the sidebar's measured minimum width (its buttons), never above the room the
//! window leaves (`min(fraction·W, W − content_min)`). So the edge stays under
//! the pointer and a dragged width is always the visible width. A drag
//! persists the width on drag-*end* only (not live, to avoid thrashing the
//! config), and only when it changed.
//!
//! ## Units and direction
//!
//! Every geometry input — pointer x, split view width, handle width — is
//! converted from px with [`adw::LengthUnit::from_px`] into the split view's
//! unit before the width is computed; otherwise the edge would outrun the
//! pointer under text scaling (Large Text). In right-to-left layouts the
//! sidebar sits on the right with the handle on its left edge, so the width is
//! measured from the split view's right edge (`W − x`).
//!
//! ## Accessibility
//!
//! The handle has the separator role and the accessible label "Resize
//! sidebar". It is not focusable: the keyboard way to change the width is the
//! Settings ▸ Interface "Sidebar width" spin row.

use adw::prelude::*;
use gtk4::Orientation;
use libadwaita as adw;
use rustconn_core::config::{SIDEBAR_WIDTH_MAX, SIDEBAR_WIDTH_MIN, clamp_sidebar_width};

use crate::i18n::i18n;
use crate::state::{SharedAppState, with_state, with_state_mut};

/// Width of the grab handle strip in pixels. Thin enough not to steal visible
/// space, wide enough to be an easy pointer target (GNOME HIG comfortable hit
/// target is larger, but the handle only needs to catch the edge).
const HANDLE_WIDTH: i32 = 4;

/// CSS class identifying the handle; also how [`apply_drag_resize_setting`]
/// finds it inside the split view's sidebar.
const HANDLE_CSS_CLASS: &str = "sidebar-resize-handle";

/// Cursor shown over the handle while it is active.
const HANDLE_CURSOR: &str = "col-resize";

/// Wraps the sidebar widget in a drag-handle container and wires the gesture.
///
/// Returns the widget to hand to [`adw::OverlaySplitView::set_sidebar`]. The
/// handle is only shown when the opt-in setting is on and the split view is
/// not collapsed (narrow window). Otherwise it is hidden entirely — no width,
/// no drawing, no cursor — so the default experience is unchanged. It is
/// re-synced on every `collapsed` change and, after Settings is saved, by
/// [`apply_drag_resize_setting`].
pub fn wrap_with_handle(
    sidebar_widget: &gtk4::Box,
    split_view: &adw::OverlaySplitView,
    state: &SharedAppState,
) -> gtk4::Box {
    let container = gtk4::Box::new(Orientation::Horizontal, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);

    // The sidebar fills the row; the handle hugs its inner edge (the box
    // mirrors in RTL, so the handle stays next to the content).
    sidebar_widget.set_hexpand(true);
    container.append(sidebar_widget);

    // `accessible-role` is construct-only, hence the builder.
    let handle = gtk4::DrawingArea::builder()
        .width_request(HANDLE_WIDTH)
        .vexpand(true)
        .css_classes([HANDLE_CSS_CLASS])
        .accessible_role(gtk4::AccessibleRole::Separator)
        .build();
    // A separator defaults to horizontal; this one is vertical.
    handle.update_property(&[
        gtk4::accessible::Property::Label(&i18n("Resize sidebar")),
        gtk4::accessible::Property::Orientation(Orientation::Vertical),
    ]);
    container.append(&handle);

    // Visibility and cursor follow the setting and the collapsed state.
    let drag_resize = with_state(state, |s| s.settings().ui.sidebar_drag_resize);
    sync_handle(handle.upcast_ref(), drag_resize, split_view.is_collapsed());
    {
        // Weak: the split view owns the container that owns the handle, so a
        // strong capture here would form a cycle.
        let handle = handle.downgrade();
        let state = state.clone();
        split_view.connect_collapsed_notify(move |sv| {
            if let Some(handle) = handle.upgrade() {
                let drag_resize = with_state(&state, |s| s.settings().ui.sidebar_drag_resize);
                sync_handle(handle.upcast_ref(), drag_resize, sv.is_collapsed());
            }
        });
    }

    let drag = gtk4::GestureDrag::new();

    {
        let split_view = split_view.clone();
        let state = state.clone();
        drag.connect_drag_update(move |gesture, offset_x, offset_y| {
            // Inert unless opted in and shown wide.
            if !is_active(&split_view, &state) {
                return;
            }
            // The handle comes from the gesture rather than a capture, so no
            // new reference to it is held by its own controller.
            let Some(handle) = gesture.widget() else {
                return;
            };
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            // start + offset is the pointer in the handle's *current*
            // coordinates — the same transform GTK used for this event. The
            // handle moves with the edge, so translate into the split view,
            // which does not.
            let local = gtk4::graphene::Point::new(
                (start_x + offset_x) as f32,
                (start_y + offset_y) as f32,
            );
            let Some(pointer) = handle.compute_point(&split_view, &local) else {
                tracing::debug!(
                    "sidebar resize: pointer outside the split view's coordinate space"
                );
                return;
            };

            // All geometry in the split view's unit (sp by default), or the
            // edge outruns the pointer under Large Text.
            let unit = split_view.sidebar_width_unit();
            let gtk_settings = split_view.settings();
            let to_sp = |px: f64| unit.from_px(px, Some(&gtk_settings));
            let pointer_sp = to_sp(f64::from(pointer.x()));
            let rtl = split_view.direction() == gtk4::TextDirection::Rtl;
            let (min_sp, max_sp) = visible_bounds(&split_view);
            let width = width_from_pointer(
                pointer_sp,
                to_sp(f64::from(split_view.width())),
                to_sp(f64::from(HANDLE_WIDTH)),
                rtl,
                min_sp,
                max_sp,
            );
            tracing::trace!(pointer_sp, width, min_sp, max_sp, "sidebar drag");
            set_width_if_changed(&split_view, width);
        });
    }

    {
        let split_view = split_view.clone();
        let state = state.clone();
        drag.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
            if !is_active(&split_view, &state) {
                return;
            }
            let final_width = clamp_sidebar_width(split_view.max_sidebar_width().round() as i32);
            set_width_if_changed(&split_view, final_width);

            // Persist the one width value (issue #3) — only if it changed, so a
            // click without motion does not rewrite the config.
            with_state_mut(&state, |s| {
                if s.settings().ui.effective_sidebar_width() == final_width {
                    return;
                }
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

/// Sets `max-sidebar-width` only when `width` differs from the current value,
/// so an unchanged width never re-queues a resize.
fn set_width_if_changed(split_view: &adw::OverlaySplitView, width: i32) {
    let width = f64::from(width);
    if (width - split_view.max_sidebar_width()).abs() >= 0.5 {
        split_view.set_max_sidebar_width(width);
    }
}

/// The drag range in whole sp, from the same terms libadwaita's
/// `get_sidebar_width()` uses to size the shown sidebar: the minimum width of
/// the sidebar's bin, and `min(fraction·W, W − content_min)` for the window.
fn visible_bounds(split_view: &adw::OverlaySplitView) -> (i32, i32) {
    let unit = split_view.sidebar_width_unit();
    let gtk_settings = split_view.settings();
    let to_sp = |px: i32| unit.from_px(f64::from(px), Some(&gtk_settings));

    // libadwaita measures its internal bin around our wrapper (the bin's CSS
    // box included); fall back to the wrapper if the tree ever differs.
    let sidebar_min_px = split_view
        .sidebar()
        .map(|wrapper| wrapper.parent().unwrap_or(wrapper))
        .map_or(0, |bin| bin.measure(Orientation::Horizontal, -1).0);

    let window_cap_px = split_view.content().map(|content| {
        let bin = content.parent().unwrap_or(content);
        let content_min = bin.measure(Orientation::Horizontal, -1).0;
        let w = split_view.width();
        ((f64::from(w) * split_view.sidebar_width_fraction()) as i32).min(w - content_min)
    });

    drag_bounds(to_sp(sidebar_min_px), window_cap_px.map(to_sp))
}

/// Sidebar width (whole sp) for a pointer at `pointer_x`, in the split view's
/// stationary frame. Every argument is in the split view's unit.
///
/// There is no "current width" parameter: the result depends only on where the
/// pointer is, so a width change cannot feed back into the next update. The
/// handle is treated as grabbed at its centre. LTR: the sidebar starts at
/// x = 0 and the handle is its last `handle_width`, so width =
/// x + handle_width / 2. RTL: the sidebar ends at the split view's right edge
/// and the handle is its first strip, so width = W − x + handle_width / 2.
/// The lower bound wins if the bounds are ever inverted (no panic).
fn width_from_pointer(
    pointer_x: f64,
    split_view_width: f64,
    handle_width: f64,
    rtl: bool,
    min_sp: i32,
    max_sp: i32,
) -> i32 {
    let edge = if rtl {
        split_view_width - pointer_x
    } else {
        pointer_x
    } + handle_width / 2.0;
    let capped = (edge.round() as i32).min(max_sp);
    if capped < min_sp { min_sp } else { capped }
}

/// Drag bounds in whole sp: the settings range narrowed to what libadwaita can
/// show — never below the sidebar's own minimum width, never above the room the
/// window leaves (`window_cap_sp`, `None` when there is no content), never
/// outside 180..=500. The lower bound wins if the window leaves less room than
/// the sidebar needs.
fn drag_bounds(sidebar_min_sp: f64, window_cap_sp: Option<f64>) -> (i32, i32) {
    // Constant bounds, 180 ≤ 500: `clamp` cannot panic.
    let lower = (sidebar_min_sp.ceil() as i32).clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
    let upper = window_cap_sp
        .map_or(SIDEBAR_WIDTH_MAX, |cap| cap.floor() as i32)
        .min(SIDEBAR_WIDTH_MAX);
    (lower, if upper < lower { lower } else { upper })
}

/// Whether the handle should respond right now: opted in, and the split view is
/// showing the sidebar in its wide (non-collapsed) layout. On a narrow window
/// the sidebar is an overlay driven by swipe gestures, where a drag handle
/// would fight the show/hide gesture — so it stays inert there.
fn is_active(split_view: &adw::OverlaySplitView, state: &SharedAppState) -> bool {
    handle_should_be_active(
        with_state(state, |s| s.settings().ui.sidebar_drag_resize),
        split_view.is_collapsed(),
    )
}

/// Whether the handle should be shown and respond: opted in and the split view
/// is in its wide (non-collapsed) layout. Single source of truth for both the
/// gesture guards and the handle's visibility.
const fn handle_should_be_active(drag_resize: bool, collapsed: bool) -> bool {
    drag_resize && !collapsed
}

/// Shows the handle (with its col-resize cursor) only while active; otherwise
/// hides it entirely so it takes no space, draws nothing and sets no cursor.
fn sync_handle(handle: &gtk4::Widget, drag_resize: bool, collapsed: bool) {
    let active = handle_should_be_active(drag_resize, collapsed);
    handle.set_visible(active);
    handle.set_cursor_from_name(active.then_some(HANDLE_CURSOR));
}

/// Re-applies the opt-in setting to the handle after Settings is saved.
///
/// `drag_resize` is the *new* value: the caller runs before it lands in
/// `AppState`, so reading state here would apply the old one.
pub fn apply_drag_resize_setting(split_view: &adw::OverlaySplitView, drag_resize: bool) {
    let handle = split_view
        .sidebar()
        .and_then(|container| container.last_child())
        .filter(|w| w.has_css_class(HANDLE_CSS_CLASS));
    if let Some(handle) = handle {
        sync_handle(&handle, drag_resize, split_view.is_collapsed());
    } else {
        tracing::warn!("sidebar resize handle not found; setting not applied");
    }
}

#[cfg(test)]
mod tests {
    use super::{drag_bounds, handle_should_be_active, width_from_pointer};

    const SPLIT: f64 = 1400.0;
    const HANDLE: f64 = 4.0;

    #[test]
    fn handle_is_active_only_when_opted_in_and_wide() {
        // (drag_resize, collapsed) -> shown
        assert!(!handle_should_be_active(false, false));
        assert!(!handle_should_be_active(false, true));
        assert!(!handle_should_be_active(true, true));
        assert!(handle_should_be_active(true, false));
    }

    #[test]
    fn width_follows_pointer_in_both_directions() {
        // LTR: edge = pointer + half the handle.
        assert_eq!(
            width_from_pointer(300.0, SPLIT, HANDLE, false, 180, 500),
            302
        );
        // RTL: measured from the right edge, 1400 − 1100 + 2.
        assert_eq!(
            width_from_pointer(1100.0, SPLIT, HANDLE, true, 180, 500),
            302
        );
        // RTL: moving the pointer left widens the sidebar.
        let narrow = width_from_pointer(1100.0, SPLIT, HANDLE, true, 180, 500);
        let wide = width_from_pointer(1050.0, SPLIT, HANDLE, true, 180, 500);
        assert_eq!(wide - narrow, 50);
        // LTR: moving the pointer right widens it.
        assert_eq!(
            width_from_pointer(350.0, SPLIT, HANDLE, false, 180, 500),
            352
        );
    }

    #[test]
    fn width_is_clamped_to_bounds() {
        // Below the minimum: the edge stops at the bound, it is not stored lower.
        assert_eq!(
            width_from_pointer(50.0, SPLIT, HANDLE, false, 244, 500),
            244
        );
        assert_eq!(
            width_from_pointer(1350.0, SPLIT, HANDLE, true, 244, 500),
            244
        );
        // Above the maximum.
        assert_eq!(
            width_from_pointer(900.0, SPLIT, HANDLE, false, 180, 500),
            500
        );
        assert_eq!(
            width_from_pointer(100.0, SPLIT, HANDLE, true, 180, 450),
            450
        );
        // Inverted bounds: the lower one wins, no panic.
        assert_eq!(
            width_from_pointer(270.0, SPLIT, HANDLE, false, 300, 250),
            300
        );
        assert_eq!(
            width_from_pointer(900.0, SPLIT, HANDLE, false, 300, 250),
            300
        );
    }

    /// Regression for the vibrating separator. The old code added the handle's
    /// own drag offset to the start width; the handle moves with the edge, so
    /// the offset was `D − (w_prev − start)` and `w_next = 2·start + D − w_prev`
    /// — a period-2 cycle for a still pointer. Here the pointer goes through
    /// the handle-local frame for every possible current width and back out
    /// (what `compute_point` does), and the width must not change.
    #[test]
    fn width_does_not_depend_on_current_width() {
        for rtl in [false, true] {
            // Absolute pointer x in the split view, same for every frame.
            let pointer = if rtl { 1100.0 } else { 300.0 };
            // Handle origin in the split view for a sidebar of width `w`.
            let origin = |w: f64| if rtl { SPLIT - w } else { w - HANDLE };
            let width_for_current = |w: i32| {
                let w = f64::from(w);
                let local = pointer - origin(w);
                width_from_pointer(local + origin(w), SPLIT, HANDLE, rtl, 180, 500)
            };

            let expected = width_for_current(320);
            for current in [180, 240, 285, 320, 325, 500] {
                assert_eq!(
                    width_for_current(current),
                    expected,
                    "rtl={rtl} current={current}"
                );
            }
            // Feeding the result back as the next current width is a fixed
            // point: no alternation between two positions.
            let mut w = 285;
            for _ in 0..4 {
                w = width_for_current(w);
                assert_eq!(w, expected, "rtl={rtl}");
            }
        }

        // The same harness does catch the old formula: it alternates.
        let (start, d) = (285, 40);
        let old = |w_prev: i32| 2 * start + d - w_prev;
        assert_eq!(old(start), start + d);
        assert_eq!(old(start + d), start);
    }

    #[test]
    fn drag_bounds_follow_libadwaita() {
        // Small child minimum: the settings range.
        assert_eq!(drag_bounds(120.0, None), (180, 500));
        // The sidebar's own minimum raises the lower bound (rounded up).
        assert_eq!(drag_bounds(243.2, None), (244, 500));
        // The window caps the upper bound (rounded down).
        assert_eq!(drag_bounds(243.2, Some(400.7)), (244, 400));
        // Window leaves less room than the sidebar needs: lower wins.
        assert_eq!(drag_bounds(243.2, Some(200.0)), (244, 244));
        // A child minimum above the settings range still stores at most 500.
        assert_eq!(drag_bounds(620.0, None), (500, 500));
        // A wide window never lifts the upper bound above 500.
        assert_eq!(drag_bounds(120.0, Some(980.0)), (180, 500));
    }
}
