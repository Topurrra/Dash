//! A thin scrollbar along the right edge of a scrolling area (GPUI draws
//! none). Put the area and the bar in one `relative()` box:
//! `div().relative().child(area).child(scrollbar(id, handle))`. The thumb
//! can be dragged; clicking the track jumps there.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{div, point, prelude::*, px, AnyElement, App, Context, ElementId, MouseButton, ScrollHandle, Window};

use crate::theme;

const WIDTH: f32 = 10.;
const MIN_THUMB: f32 = 28.;

/// What's being dragged: which bar, and where on the thumb it was grabbed.
#[derive(Clone)]
struct ThumbDrag {
    id: ElementId,
    grab: Rc<Cell<f32>>,
}

/// Shown while dragging (nothing: the thumb itself moves).
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Thumb height and top for a view `view` tall over `max` more content,
/// scrolled down by `scrolled`.
fn thumb(view: f32, max: f32, scrolled: f32) -> (f32, f32) {
    let height = (view * view / (view + max)).clamp(MIN_THUMB.min(view), view);
    let top = if max > 0. { (scrolled / max).clamp(0., 1.) * (view - height) } else { 0. };
    (height, top)
}

/// Scrolls so the thumb's top lands at `thumb_top` (px from the track's top).
fn scroll_to(handle: &ScrollHandle, thumb_top: f32, view: f32, height: f32, max: f32) {
    let room = (view - height).max(1.);
    let scrolled = (thumb_top / room).clamp(0., 1.) * max;
    handle.set_offset(point(handle.offset().x, px(-scrolled)));
}

pub fn scrollbar(id: impl Into<ElementId>, handle: &ScrollHandle) -> AnyElement {
    let id = id.into();
    let view = f32::from(handle.bounds().size.height);
    let max = f32::from(handle.max_offset().height);
    if max <= 0.5 || view <= 0. {
        return div().into_any_element();
    }
    let (height, top) = thumb(view, max, -f32::from(handle.offset().y));
    let grab = Rc::new(Cell::new(height / 2.));
    let drag = ThumbDrag { id: id.clone(), grab: grab.clone() };
    let (on_track, on_move, on_thumb) = (handle.clone(), handle.clone(), handle.clone());
    let thumb_grab = grab.clone();
    // The press was on the thumb (it runs first): the track mustn't jump.
    // It isn't stopped there, or the track would never start the drag.
    let pressed_thumb = Rc::new(Cell::new(false));
    let thumb_pressed = pressed_thumb.clone();
    div()
        .id(id.clone())
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .w(px(WIDTH))
        .group("scrollbar")
        // Clicking the track: the thumb's middle goes there.
        .on_mouse_down(MouseButton::Left, move |event, window, _: &mut App| {
            if pressed_thumb.replace(false) {
                return;
            }
            let y = f32::from(event.position.y - on_track.bounds().origin.y);
            grab.set(height / 2.);
            scroll_to(&on_track, y - height / 2., view, height, max);
            window.refresh();
        })
        .on_drag(drag, |_, _, _, cx| cx.new(|_| NoPreview))
        .on_drag_move::<ThumbDrag>(move |event, window, cx| {
            let drag = event.drag(cx);
            if drag.id != id {
                return;
            }
            let y = f32::from(event.event.position.y - event.bounds.origin.y);
            scroll_to(&on_move, y - drag.grab.get(), view, height, max);
            window.refresh();
        })
        .child(
            div()
                .absolute()
                .top(px(top))
                .right(px(2.))
                .w(px(5.))
                .h(px(height))
                .rounded_full()
                .bg(theme::scrollbar())
                .group_hover("scrollbar", |s| s.w(px(7.)).bg(theme::muted()))
                // Grabbed on the thumb: keep that point under the pointer.
                .on_mouse_down(MouseButton::Left, move |event: &gpui::MouseDownEvent, _, _: &mut App| {
                    thumb_grab.set(f32::from(event.position.y - on_thumb.bounds().origin.y) - top);
                    thumb_pressed.set(true);
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::thumb;

    #[test]
    fn thumb_tracks_the_scroll() {
        // Half the content visible: half-height thumb.
        assert_eq!(thumb(400., 400., 0.), (200., 0.));
        assert_eq!(thumb(400., 400., 400.), (200., 200.));
        // A huge list: the thumb keeps a usable size.
        let (h, top) = thumb(400., 10_000_000., 5_000_000.);
        assert_eq!(h, 28.);
        assert!((top - 186.).abs() < 0.01);
    }
}
