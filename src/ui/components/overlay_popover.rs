//! Shared presentation for Aviary-owned floating menus.
//!
//! The component deliberately owns the mechanics that are easy to get wrong:
//! absolute positioning, deferred overlay painting, native scrolling and the
//! opening motion. Callers retain ownership of open/close state and keyboard
//! interaction.

use super::super::motion::{ease_out_cubic, WheelScrollMotion};
use gpui::{
    deferred, point, prelude::*, px, Animation, AnimationExt as _, AnyElement, App, ElementId,
    IntoElement, Pixels, RenderOnce, ScrollHandle, ScrollWheelEvent, Window,
};
use gpui_component::{v_flex, ActiveTheme};
use std::{cell::RefCell, rc::Rc, time::Duration};

const OPEN_DURATION: Duration = Duration::from_millis(140);
const OPEN_OFFSET: Pixels = px(4.);

#[derive(Clone, Copy, Default)]
enum Padding {
    #[default]
    All,
    Vertical,
}

struct OverlayPopoverScrollState {
    handle: ScrollHandle,
    motion: WheelScrollMotion,
}

impl Default for OverlayPopoverScrollState {
    fn default() -> Self {
        Self {
            handle: ScrollHandle::new(),
            motion: WheelScrollMotion::default(),
        }
    }
}

/// Persistent scroll state shared between the host view and a popover frame.
#[derive(Clone, Default)]
pub(crate) struct OverlayPopoverScroll(Rc<RefCell<OverlayPopoverScrollState>>);

impl OverlayPopoverScroll {
    pub(crate) fn reset(&self) {
        let mut state = self.0.borrow_mut();
        state.motion.cancel();
        state.handle.set_offset(point(px(0.), px(0.)));
    }
}

/// A scrollable, animated popover positioned relative to its parent.
#[derive(IntoElement)]
pub(crate) struct OverlayPopover {
    id: ElementId,
    left: Pixels,
    top: Pixels,
    width: Pixels,
    max_height: Pixels,
    constrain_width: bool,
    padding: Padding,
    scroll: OverlayPopoverScroll,
    children: Vec<AnyElement>,
}

impl OverlayPopover {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        left: Pixels,
        top: Pixels,
        width: Pixels,
        max_height: Pixels,
        scroll: OverlayPopoverScroll,
    ) -> Self {
        Self {
            id: id.into(),
            left,
            top,
            width,
            max_height,
            constrain_width: false,
            padding: Padding::All,
            scroll,
            children: Vec::new(),
        }
    }

    /// Keep the requested width from exceeding the positioning parent.
    pub(crate) fn constrain_width(mut self) -> Self {
        self.constrain_width = true;
        self
    }

    /// Use only vertical outer padding, useful for edge-to-edge menu rows.
    pub(crate) fn vertical_padding(mut self) -> Self {
        self.padding = Padding::Vertical;
        self
    }

    pub(crate) fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    pub(crate) fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(IntoElement::into_any_element));
        self
    }
}

impl RenderOnce for OverlayPopover {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let resting_top = self.top;
        let motion_id = (self.id.clone(), "open");
        let scroll_id = (self.id.clone(), "scroll");
        let scroll_handle = self.scroll.0.borrow().handle.clone();
        self.scroll
            .0
            .borrow_mut()
            .motion
            .advance(&scroll_handle, window);
        let wheel_scroll = self.scroll.clone();
        let wheel_handle = scroll_handle.clone();
        let contents = v_flex()
            .id(scroll_id)
            .w_full()
            .max_h(self.max_height)
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .when(matches!(self.padding, Padding::All), |contents| {
                contents.p_1()
            })
            .when(matches!(self.padding, Padding::Vertical), |contents| {
                contents.py_1()
            })
            .children(self.children);
        let panel = v_flex()
            .id(self.id)
            .absolute()
            .left(self.left)
            .top(resting_top)
            .w(self.width)
            .max_h(self.max_height)
            .when(self.constrain_width, |panel| panel.max_w_full())
            .overflow_hidden()
            .on_scroll_wheel(move |event: &ScrollWheelEvent, window, _cx| {
                if wheel_scroll
                    .0
                    .borrow_mut()
                    .motion
                    .on_wheel(&wheel_handle, event, window)
                {
                    // Event callbacks run outside GPUI's paint/layout phases;
                    // `request_animation_frame` would query the current
                    // rendering view and panic here. Refresh once, then
                    // `WheelScrollMotion::advance` schedules subsequent frames
                    // from the next layout pass.
                    window.refresh();
                }
            })
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .child(contents)
            .with_animation(
                motion_id,
                Animation::new(OPEN_DURATION).with_easing(ease_out_cubic),
                move |panel, progress| {
                    panel
                        .top(resting_top - (1. - progress) * OPEN_OFFSET)
                        .opacity(progress)
                },
            );

        deferred(panel).with_priority(1)
    }
}
