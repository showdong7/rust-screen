//! Resize geometry stays in original image pixels; UI handles stay in screen points.
use eframe::egui::{CursorIcon, Pos2, Rect, Vec2};

use crate::annotation::{Annotation, Tool};

const MIN_SIZE: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    Start,
    End,
}

impl Handle {
    pub fn cursor(self) -> CursorIcon {
        match self {
            Self::TopLeft | Self::BottomRight => CursorIcon::ResizeNwSe,
            Self::TopRight | Self::BottomLeft => CursorIcon::ResizeNeSw,
            Self::Top | Self::Bottom => CursorIcon::ResizeVertical,
            Self::Left | Self::Right => CursorIcon::ResizeHorizontal,
            Self::Start | Self::End => CursorIcon::Crosshair,
        }
    }

    fn direction(self) -> Vec2 {
        match self {
            Self::TopLeft => Vec2::new(-1.0, -1.0),
            Self::Top => Vec2::new(0.0, -1.0),
            Self::TopRight => Vec2::new(1.0, -1.0),
            Self::Right => Vec2::new(1.0, 0.0),
            Self::BottomRight => Vec2::new(1.0, 1.0),
            Self::Bottom => Vec2::new(0.0, 1.0),
            Self::BottomLeft => Vec2::new(-1.0, 1.0),
            Self::Left => Vec2::new(-1.0, 0.0),
            _ => Vec2::ZERO,
        }
    }
}

pub fn handles(item: &Annotation, bounds: Rect) -> Vec<(Handle, Pos2)> {
    if matches!(item.tool, Tool::Line | Tool::Arrow) {
        return vec![(Handle::Start, item.start), (Handle::End, item.end)];
    }
    let mut result = vec![
        (Handle::TopLeft, bounds.left_top()),
        (Handle::TopRight, bounds.right_top()),
        (Handle::BottomRight, bounds.right_bottom()),
        (Handle::BottomLeft, bounds.left_bottom()),
    ];
    if item.tool != Tool::Text {
        result.extend([
            (Handle::Top, bounds.center_top()),
            (Handle::Right, bounds.right_center()),
            (Handle::Bottom, bounds.center_bottom()),
            (Handle::Left, bounds.left_center()),
        ]);
    }
    result
}

/// The opposite edge/corner is fixed. Crossing it clamps rather than flipping.
/// Objects can remain partially outside a crop; resizing never translates the anchor.
pub fn resize(
    original: &Annotation,
    bounds: Rect,
    handle: Handle,
    target: Pos2,
    keep_ratio: bool,
) -> Annotation {
    let mut item = original.clone();
    if matches!(handle, Handle::Start | Handle::End) {
        let anchor = if handle == Handle::Start {
            original.end
        } else {
            original.start
        };
        let old = if handle == Handle::Start {
            original.start
        } else {
            original.end
        };
        let delta = target - anchor;
        let target = if delta.length() < MIN_SIZE {
            let direction = (old - anchor).normalized();
            anchor
                + if direction == Vec2::ZERO {
                    Vec2::new(MIN_SIZE, 0.0)
                } else {
                    direction * MIN_SIZE
                }
        } else {
            target
        };
        if handle == Handle::Start {
            item.start = target;
        } else {
            item.end = target;
        }
        return item;
    }
    let direction = handle.direction();
    let anchor = Pos2::new(
        if direction.x < 0.0 {
            bounds.right()
        } else if direction.x > 0.0 {
            bounds.left()
        } else {
            bounds.center().x
        },
        if direction.y < 0.0 {
            bounds.bottom()
        } else if direction.y > 0.0 {
            bounds.top()
        } else {
            bounds.center().y
        },
    );
    if original.tool == Tool::Text {
        let diagonal = bounds.size() * direction;
        let factor = ((target - anchor).dot(diagonal) / diagonal.length_sq().max(0.0001)).clamp(
            8.0 / original.style.font_size,
            200.0 / original.style.font_size,
        );
        item.style.font_size = original.style.font_size * factor;
        let corner = anchor + diagonal * factor;
        item.start = anchor.min(corner);
        item.end = item.start;
        return item;
    }
    let mut size = bounds.size();
    if direction.x != 0.0 {
        size.x = ((target.x - anchor.x) * direction.x).max(MIN_SIZE);
    }
    if direction.y != 0.0 {
        size.y = ((target.y - anchor.y) * direction.y).max(MIN_SIZE);
    }
    if keep_ratio && direction.x != 0.0 && direction.y != 0.0 {
        let diagonal = bounds.size().max(Vec2::splat(MIN_SIZE));
        let desired = (target - anchor) * direction;
        let factor = (desired.dot(diagonal) / diagonal.length_sq())
            .max(MIN_SIZE / diagonal.x)
            .max(MIN_SIZE / diagonal.y);
        size = diagonal * factor;
    }
    let corner = anchor + size * direction;
    item.start = Pos2::new(
        if direction.x == 0.0 {
            bounds.left()
        } else {
            anchor.x.min(corner.x)
        },
        if direction.y == 0.0 {
            bounds.top()
        } else {
            anchor.y.min(corner.y)
        },
    );
    item.end = Pos2::new(
        if direction.x == 0.0 {
            bounds.right()
        } else {
            anchor.x.max(corner.x)
        },
        if direction.y == 0.0 {
            bounds.bottom()
        } else {
            anchor.y.max(corner.y)
        },
    );
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{annotation::Style, annotation_render};
    use eframe::egui::Color32;

    fn item(tool: Tool) -> Annotation {
        let mut item = Annotation::new(
            tool,
            Pos2::new(60.0, 80.0),
            Style {
                color: Color32::RED,
                width: 5.0,
                font_size: 32.0,
                filled: false,
            },
        );
        item.end = Pos2::new(260.0, 180.0);
        item.text = "大小測試\nSecond line".into();
        item
    }

    #[test]
    fn all_eight_handles_preserve_the_opposite_anchor_and_stroke() {
        for tool in [Tool::Rectangle, Tool::Ellipse] {
            let original = item(tool);
            let bounds = Rect::from_two_pos(original.start, original.end);
            for (handle, position) in handles(&original, bounds) {
                let direction = handle.direction();
                let resized = resize(
                    &original,
                    bounds,
                    handle,
                    position + direction * 20.0,
                    false,
                );
                let result = Rect::from_two_pos(resized.start, resized.end);
                assert_eq!(result.width(), bounds.width() + direction.x.abs() * 20.0);
                assert_eq!(result.height(), bounds.height() + direction.y.abs() * 20.0);
                assert_eq!(resized.style, original.style);
                if direction.x < 0.0 {
                    assert_eq!(result.right(), bounds.right());
                } else {
                    assert_eq!(result.left(), bounds.left());
                }
                if direction.y < 0.0 {
                    assert_eq!(result.bottom(), bounds.bottom());
                } else {
                    assert_eq!(result.top(), bounds.top());
                }
            }
        }
    }

    #[test]
    fn crossing_the_anchor_clamps_size_without_flipping() {
        let original = item(Tool::Rectangle);
        let bounds = Rect::from_two_pos(original.start, original.end);
        for (handle, position) in handles(&original, bounds) {
            let resized = resize(
                &original,
                bounds,
                handle,
                position - handle.direction() * 1000.0,
                false,
            );
            assert!(resized.end.x - resized.start.x >= MIN_SIZE);
            assert!(resized.end.y - resized.start.y >= MIN_SIZE);
            if handle.direction().x > 0.0 {
                assert_eq!(resized.start.x, original.start.x);
            }
            if handle.direction().y > 0.0 {
                assert_eq!(resized.start.y, original.start.y);
            }
        }
    }

    #[test]
    fn shift_preserves_original_ratio_for_all_corners_including_reverse_draws() {
        let mut original = item(Tool::Ellipse);
        std::mem::swap(&mut original.start, &mut original.end);
        let bounds = Rect::from_two_pos(original.start, original.end);
        for (handle, position) in handles(&original, bounds).into_iter().take(4) {
            let resized = resize(
                &original,
                bounds,
                handle,
                position + handle.direction() * Vec2::new(170.0, 35.0),
                true,
            );
            let size = resized.end - resized.start;
            assert!((size.x / size.y - 2.0).abs() < 0.0001);
        }
    }

    #[test]
    fn line_and_arrow_endpoints_keep_the_other_end_fixed_and_never_collapse() {
        for tool in [Tool::Line, Tool::Arrow] {
            let original = item(tool);
            let bounds = Rect::from_two_pos(original.start, original.end);
            let resized = resize(&original, bounds, Handle::End, original.start, false);
            assert_eq!(resized.start, original.start);
            assert!((resized.start.distance(resized.end) - MIN_SIZE).abs() < 0.001);
            let resized = resize(
                &original,
                bounds,
                Handle::Start,
                Pos2::new(350.0, 280.0),
                false,
            );
            assert_eq!(resized.end, original.end);
            assert_eq!(resized.start, Pos2::new(350.0, 280.0));
            assert_eq!(resized.style.width, original.style.width);
        }
    }

    #[test]
    fn text_corners_scale_font_and_hold_opposite_corner_with_font_limits() {
        let font = annotation_render::load_font();
        let original = item(Tool::Text);
        let bounds = annotation_render::bounds(&original, &font);
        for (handle, position) in handles(&original, bounds) {
            let diagonal = bounds.size() * handle.direction();
            let anchor = position - diagonal;
            let resized = resize(&original, bounds, handle, anchor + diagonal * 1.5, false);
            assert!((resized.style.font_size - 48.0).abs() < 0.001);
            let result = annotation_render::bounds(&resized, &font);
            let opposite = annotation_resize_opposite(&resized, result, handle);
            assert!(
                anchor.distance(opposite) < 0.001,
                "{handle:?}: {anchor:?} vs {opposite:?}"
            );
            assert_eq!(resized.text, original.text);
            for factor in [-5.0, 100.0] {
                let limited = resize(&original, bounds, handle, anchor + diagonal * factor, false);
                assert!((8.0..=200.0).contains(&limited.style.font_size));
            }
        }
    }

    fn annotation_resize_opposite(item: &Annotation, bounds: Rect, handle: Handle) -> Pos2 {
        let opposite = match handle {
            Handle::TopLeft => Handle::BottomRight,
            Handle::TopRight => Handle::BottomLeft,
            Handle::BottomRight => Handle::TopLeft,
            _ => Handle::TopRight,
        };
        handles(item, bounds)
            .into_iter()
            .find(|(h, _)| *h == opposite)
            .unwrap()
            .1
    }
}
