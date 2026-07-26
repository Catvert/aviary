//! Small, view-owned transition primitives for interactive UI motion.
//!
//! A host view keeps a [`HoverMotionMap`] or [`WheelScrollMotion`], changes its
//! target from UI events, then samples the eased value while rendering. Active
//! samples request the next frame; settled samples are entirely idle. Keeping
//! motion on the view also gives animation state the same lifetime as the
//! components it belongs to.
//!
//! ```ignore
//! // Stored on the host view:
//! hover: HoverMotionMap<ComponentId>,
//!
//! // Once in the host render method:
//! self.hover.request_frame(window);
//!
//! // For each component:
//! let amount = self.hover.value(&id);
//! div()
//!     .bg(base.lerp(hovered, amount))
//!     .with_hover_motion(cx, id, |view| &mut view.hover)
//! ```

use gpui::{
    point, px, Context, Hsla, Pixels, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement, Window,
};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::{Duration, Instant};

const TARGET_EPSILON: f32 = 0.0001;
const SCROLL_SYNC_EPSILON_PX: f32 = 0.75;
const SCROLL_SNAP_PX: f32 = 0.5;
const DEFAULT_WHEEL_SCROLL_DURATION: Duration = Duration::from_millis(160);

#[derive(Clone, Copy, Debug)]
struct MotionSample {
    value: f32,
    running: bool,
}

struct MotionMap<K> {
    entries: HashMap<K, Motion>,
    active: HashSet<K>,
}

impl<K> Default for MotionMap<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            active: HashSet::new(),
        }
    }
}

impl<K> MotionMap<K>
where
    K: Clone + Eq + Hash,
{
    fn set_target(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: fn(f32) -> f32,
    ) -> bool {
        let target = target.clamp(0., 1.);
        if let Some(motion) = self.entries.get_mut(&key) {
            let changed = motion.set_target(target, duration, easing);
            if changed {
                self.active.insert(key);
            }
            return changed;
        }
        if target <= TARGET_EPSILON {
            return false;
        }

        let mut motion = Motion::default();
        let changed = motion.set_target(target, duration, easing);
        self.entries.insert(key.clone(), motion);
        if changed {
            self.active.insert(key);
        }
        changed
    }

    fn sample(&self, key: &K) -> MotionSample {
        self.entries
            .get(key)
            .map(Motion::sample)
            .unwrap_or(MotionSample {
                value: 0.,
                running: false,
            })
    }

    fn request_frame(&mut self, window: &Window) {
        let now = Instant::now();
        let entries = &self.entries;
        let mut settled_at_rest = Vec::new();
        self.active.retain(|key| {
            let Some(sample) = entries.get(key).map(|motion| motion.sample_at(now)) else {
                return false;
            };
            if !sample.running && sample.value <= TARGET_EPSILON {
                settled_at_rest.push(key.clone());
            }
            sample.running
        });
        for key in settled_at_rest {
            self.entries.remove(&key);
        }
        if !self.active.is_empty() {
            window.request_animation_frame();
        }
    }

    fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.entries.retain(|key, _| keep(key));
        self.active.retain(|key| self.entries.contains_key(key));
    }
}

/// View-owned hover transitions for a family of stable component keys.
///
/// Store one map on the host view, call [`Self::request_frame`] once from its
/// render method, and use [`HoverMotionExt::with_hover_motion`] on each
/// interactive element.
pub struct HoverMotionMap<K> {
    motions: MotionMap<K>,
    duration: Duration,
}

impl<K> HoverMotionMap<K>
where
    K: Clone + Eq + Hash,
{
    pub fn new(duration: Duration) -> Self {
        Self {
            motions: MotionMap::default(),
            duration,
        }
    }

    fn set_hovered(&mut self, key: K, hovered: bool) -> bool {
        self.motions.set_target(
            key,
            if hovered { 1. } else { 0. },
            self.duration,
            ease_out_cubic,
        )
    }

    /// Current eased hover amount in the inclusive `0..=1` range.
    pub fn value(&self, key: &K) -> f32 {
        self.motions.sample(key).value
    }

    /// Request at most one new frame for all currently moving components.
    pub fn request_frame(&mut self, window: &Window) {
        self.motions.request_frame(window);
    }

    /// Drop animation state for components that no longer belong to the view.
    pub fn retain(&mut self, keep: impl FnMut(&K) -> bool) {
        self.motions.retain(keep);
    }
}

/// View-owned motion for a notched mouse wheel.
///
/// GPUI's pixel deltas from touchpads are already progressive, so they pass
/// through untouched. Only line deltas are replayed as a tween. Call
/// [`Self::advance`] once while rendering the scrollable view and attach
/// [`Self::on_wheel`] to an ancestor of GPUI's scrollable element, after its
/// internal handler has applied the wheel jump.
/// A scroll position and the wheel motion that animates it.
///
/// The two are inseparable: a [`WheelScrollMotion`] smooths exactly one handle,
/// and every pane that scrolls owns both. Keeping them in one value is what
/// stops a view from advancing one pane's motion against another's offset.
pub struct ScrollPane<H> {
    pub handle: H,
    pub motion: WheelScrollMotion,
}

impl<H> ScrollPane<H> {
    pub fn new(handle: H) -> Self {
        Self {
            handle,
            motion: WheelScrollMotion::default(),
        }
    }
}

pub struct WheelScrollMotion {
    motion: Option<Motion>,
    duration: Duration,
    /// Last offset written or observed during render. It lets the next wheel
    /// event recover the pre-jump position even when GPUI clamps at an edge.
    last_offset: Option<f32>,
}

impl Default for WheelScrollMotion {
    fn default() -> Self {
        Self::new(DEFAULT_WHEEL_SCROLL_DURATION)
    }
}

impl WheelScrollMotion {
    pub fn new(duration: Duration) -> Self {
        Self {
            motion: None,
            duration,
            last_offset: None,
        }
    }

    /// Advance the tween from the current frame time and update the handle.
    ///
    /// A programmatic jump made while a tween is active wins immediately:
    /// observing an unexpected handle offset cancels the pending motion rather
    /// than overwriting the caller's new position.
    pub fn advance(&mut self, handle: &ScrollHandle, window: &Window) {
        let actual = f32::from(handle.offset().y);
        let Some(mut motion) = self.motion.take() else {
            self.last_offset = Some(actual);
            return;
        };

        if self
            .last_offset
            .is_some_and(|expected| (actual - expected).abs() > SCROLL_SYNC_EPSILON_PX)
        {
            self.last_offset = Some(actual);
            return;
        }

        let now = Instant::now();
        let max_offset = f32::from(handle.max_offset().height);
        let mut sample = motion.sample_at(now);
        let target = motion.target.clamp(-max_offset, 0.);

        // The list can grow or shrink while moving. Retarget from the current
        // visual position so clamping to the new bounds remains continuous.
        if (target - motion.target).abs() > TARGET_EPSILON {
            let current = sample.value.clamp(-max_offset, 0.);
            motion = Motion::between_at(current, target, self.duration, ease_out_cubic, now);
            sample = motion.sample_at(now);
        }

        let x = handle.offset().x;
        if !sample.running || (target - sample.value).abs() <= SCROLL_SNAP_PX {
            handle.set_offset(point(x, px(target)));
            self.last_offset = Some(target);
            return;
        }

        let current = sample.value.clamp(-max_offset, 0.);
        handle.set_offset(point(x, px(current)));
        self.last_offset = Some(current);
        self.motion = Some(motion);
        window.request_animation_frame();
    }

    /// Replace GPUI's line-sized jump with an eased transition.
    ///
    /// Returns `true` when the host view should be notified to render the first
    /// frame. Pixel deltas cancel a pending tween and remain one-to-one with
    /// the touchpad gesture.
    pub fn on_wheel(
        &mut self,
        handle: &ScrollHandle,
        event: &ScrollWheelEvent,
        window: &Window,
    ) -> bool {
        let delta = match event.delta {
            ScrollDelta::Pixels(_) => {
                self.cancel();
                return false;
            }
            ScrollDelta::Lines(_) => event.delta.pixel_delta(window.line_height()),
        };
        // Match GPUI's vertical fallback for horizontal-only wheel deltas.
        let dy = if delta.y == px(0.) { delta.x } else { delta.y };
        if dy == px(0.) {
            return false;
        }

        let dy = f32::from(dy);
        let jumped = f32::from(handle.offset().y);
        let max_offset = f32::from(handle.max_offset().height);
        let clamp = |value: f32| value.clamp(-max_offset, 0.);
        let now = Instant::now();

        let (current, target) = match self.motion.take() {
            Some(motion) => (motion.sample_at(now).value, motion.target + dy),
            None => {
                let inferred = jumped - dy;
                let observed = self.last_offset.filter(|previous| {
                    (jumped - clamp(*previous + dy)).abs() <= SCROLL_SYNC_EPSILON_PX
                });
                (observed.unwrap_or(inferred), jumped)
            }
        };
        let current = clamp(current);
        let target = clamp(target);
        let x = handle.offset().x;

        if (target - current).abs() <= SCROLL_SNAP_PX {
            handle.set_offset(point(x, px(target)));
            self.last_offset = Some(target);
            return true;
        }

        // Undo the internal jump and let rendering advance from here.
        handle.set_offset(point(x, px(current)));
        self.last_offset = Some(current);
        self.motion = Some(Motion::between_at(
            current,
            target,
            self.duration,
            ease_out_cubic,
            now,
        ));
        true
    }

    /// Abandon the current tween before direct gesture or programmatic motion.
    pub fn cancel(&mut self) {
        self.motion = None;
        self.last_offset = None;
    }

    /// Destination used for near-bottom calculations while motion is active.
    pub fn target_y(&self, handle: &ScrollHandle) -> Pixels {
        self.motion
            .as_ref()
            .map(|motion| {
                px(motion
                    .target
                    .clamp(-f32::from(handle.max_offset().height), 0.))
            })
            .unwrap_or(handle.offset().y)
    }
}

/// Fluent binding between a GPUI hover event and a view-owned motion map.
pub trait HoverMotionExt: StatefulInteractiveElement {
    fn with_hover_motion<V, K>(
        self,
        cx: &Context<V>,
        key: K,
        motions: impl for<'a> Fn(&'a mut V) -> &'a mut HoverMotionMap<K> + 'static,
    ) -> Self
    where
        Self: Sized,
        V: 'static,
        K: Clone + Eq + Hash + 'static,
    {
        self.on_hover(cx.listener(move |view, hovered, _, cx| {
            if motions(view).set_hovered(key.clone(), *hovered) {
                cx.notify();
            }
        }))
    }
}

impl<E> HoverMotionExt for E where E: StatefulInteractiveElement {}

struct Motion {
    from: f32,
    target: f32,
    started_at: Instant,
    duration: Duration,
    easing: fn(f32) -> f32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            from: 0.,
            target: 0.,
            started_at: Instant::now(),
            duration: Duration::ZERO,
            easing: ease_out_cubic,
        }
    }
}

impl Motion {
    fn between_at(
        from: f32,
        target: f32,
        duration: Duration,
        easing: fn(f32) -> f32,
        now: Instant,
    ) -> Self {
        Self {
            from,
            target,
            started_at: now,
            duration,
            easing,
        }
    }

    fn set_target(&mut self, target: f32, full_duration: Duration, easing: fn(f32) -> f32) -> bool {
        self.set_target_at(target, full_duration, easing, Instant::now())
    }

    fn set_target_at(
        &mut self,
        target: f32,
        full_duration: Duration,
        easing: fn(f32) -> f32,
        now: Instant,
    ) -> bool {
        if (self.target - target).abs() <= TARGET_EPSILON {
            return false;
        }

        let current = self.sample_at(now).value;
        self.from = current;
        self.target = target;
        self.started_at = now;
        self.duration = full_duration.mul_f32((target - current).abs());
        self.easing = easing;
        true
    }

    fn sample(&self) -> MotionSample {
        self.sample_at(Instant::now())
    }

    fn sample_at(&self, now: Instant) -> MotionSample {
        if self.duration.is_zero() {
            return MotionSample {
                value: self.target,
                running: false,
            };
        }

        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed >= self.duration {
            return MotionSample {
                value: self.target,
                running: false,
            };
        }

        let progress = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = (self.easing)(progress.clamp(0., 1.));
        MotionSample {
            value: self.from + (self.target - self.from) * eased,
            running: true,
        }
    }
}

/// Cubic ease-out: immediate response followed by a soft landing.
pub fn ease_out_cubic(progress: f32) -> f32 {
    1. - (1. - progress).powi(3)
}

/// Safe interpolation for style values composed by the caller.
pub trait Lerp: Sized {
    fn lerp(self, target: Self, amount: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, target: Self, amount: f32) -> Self {
        self + (target - self) * amount.clamp(0., 1.)
    }
}

impl Lerp for Pixels {
    fn lerp(self, target: Self, amount: f32) -> Self {
        self + (target - self) * amount.clamp(0., 1.)
    }
}

impl Lerp for Hsla {
    fn lerp(self, target: Self, amount: f32) -> Self {
        let amount = amount.clamp(0., 1.);
        let mut hue_delta = target.h - self.h;
        if hue_delta > 0.5 {
            hue_delta -= 1.;
        } else if hue_delta < -0.5 {
            hue_delta += 1.;
        }

        Self {
            h: (self.h + hue_delta * amount).rem_euclid(1.),
            s: self.s + (target.s - self.s) * amount,
            l: self.l + (target.l - self.l) * amount,
            a: self.a + (target.a - self.a) * amount,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversing_keeps_the_current_value_and_scales_the_duration() {
        let start = Instant::now();
        let mut motion = Motion::default();
        assert!(motion.set_target_at(1., Duration::from_millis(200), |t| t, start));
        let halfway = start + Duration::from_millis(100);
        assert!(motion.set_target_at(0., Duration::from_millis(200), |t| t, halfway));

        assert!((motion.sample_at(halfway).value - 0.5).abs() < TARGET_EPSILON);
        assert_eq!(motion.duration, Duration::from_millis(100));
        assert!(
            !motion
                .sample_at(halfway + Duration::from_millis(100))
                .running
        );
    }

    #[test]
    fn colors_take_the_shortest_path_around_the_hue_wheel() {
        let from = Hsla {
            h: 0.95,
            s: 1.,
            l: 0.5,
            a: 1.,
        };
        let to = Hsla { h: 0.05, ..from };

        let midpoint = from.lerp(to, 0.5);
        assert!(midpoint.h < TARGET_EPSILON || midpoint.h > 1. - TARGET_EPSILON);
    }

    #[test]
    fn hover_map_does_not_restart_an_unchanged_target() {
        let mut hover = HoverMotionMap::new(Duration::from_millis(140));
        let key = "component-a";

        assert!(hover.set_hovered(key, true));
        assert!(!hover.set_hovered(key, true));
        assert!((0. ..=1.).contains(&hover.value(&key)));

        hover.retain(|candidate| *candidate != key);
        assert_eq!(hover.value(&key), 0.);
    }

    #[test]
    fn scalar_motion_uses_elapsed_frame_time() {
        let start = Instant::now();
        let motion = Motion::between_at(-40., -120., Duration::from_millis(160), |t| t, start);

        let halfway = motion.sample_at(start + Duration::from_millis(80));
        assert!(halfway.running);
        assert!((halfway.value + 80.).abs() < TARGET_EPSILON);
        assert!(!motion.sample_at(start + Duration::from_millis(160)).running);
    }
}
