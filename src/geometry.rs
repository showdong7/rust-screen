/// Coordinates are always physical desktop pixels, including negative monitor origins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DesktopRect {
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = i64::from(self.x).max(i64::from(other.x));
        let y = i64::from(self.y).max(i64::from(other.y));
        let right = (i64::from(self.x) + i64::from(self.width))
            .min(i64::from(other.x) + i64::from(other.width));
        let bottom = (i64::from(self.y) + i64::from(self.height))
            .min(i64::from(other.y) + i64::from(other.height));
        (right > x && bottom > y).then_some(Self {
            x: x as i32,
            y: y as i32,
            width: (right - x).max(0) as u32,
            height: (bottom - y).max(0) as u32,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    pub fn fit(&mut self, width: u32, height: u32) {
        self.width = self.width.clamp(1, width.max(1));
        self.height = self.height.clamp(1, height.max(1));
        self.x = self.x.min(width.saturating_sub(self.width));
        self.y = self.y.min(height.saturating_sub(self.height));
    }

    pub fn from_points(a: [f32; 2], b: [f32; 2], width: u32, height: u32) -> Self {
        let mut result = Self {
            x: a[0].min(b[0]).max(0.0).floor() as u32,
            y: a[1].min(b[1]).max(0.0).floor() as u32,
            width: (a[0] - b[0]).abs().round().max(1.0) as u32,
            height: (a[1] - b[1]).abs().round().max(1.0) as u32,
        };
        result.fit(width, height);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersects_negative_monitor_and_offscreen_window() {
        let desktop = DesktopRect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        let window = DesktopRect {
            x: -2100,
            y: -50,
            width: 800,
            height: 600,
        };
        assert_eq!(
            desktop.intersection(window),
            Some(DesktopRect {
                x: -1920,
                y: 0,
                width: 620,
                height: 550,
            })
        );
        assert_eq!(
            desktop.intersection(DesktopRect {
                x: 1920,
                y: 0,
                width: 100,
                height: 100,
            }),
            None
        );
    }

    #[test]
    fn crop_remains_within_image_when_resizing_at_bottom_right() {
        let mut crop = CropRect {
            x: 1800,
            y: 1000,
            width: 640,
            height: 480,
        };
        crop.fit(1920, 1080);
        assert_eq!(
            crop,
            CropRect {
                x: 1280,
                y: 600,
                width: 640,
                height: 480
            }
        );
        crop.width = 8000;
        crop.height = 0;
        crop.fit(1920, 1080);
        assert_eq!(crop.width, 1920);
        assert_eq!(crop.height, 1);
        assert_eq!(crop.x, 0);
    }

    #[test]
    fn dragging_in_reverse_produces_same_crop() {
        let a = [900.0, 700.0];
        let b = [100.0, 200.0];
        assert_eq!(
            CropRect::from_points(a, b, 1920, 1080),
            CropRect {
                x: 100,
                y: 200,
                width: 800,
                height: 500
            }
        );
        assert_eq!(
            CropRect::from_points(a, b, 1920, 1080),
            CropRect::from_points(b, a, 1920, 1080)
        );
    }
}
