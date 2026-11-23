//! The phone viewer image page's zoom: the scale of the picture over its fit
//! size, from fit to five times, and the position of the picture's top-left
//! corner in the page area's own pixels. A picture keeps its aspect, so its
//! box is the largest one of its own shape the area holds, scaled: a side
//! wider or taller than the area is clamped to the area's edges, and a
//! shorter side stays centred however the picture is panned.

/// How far a pinch may zoom in: the fit size is 1, and this is the closest.
const MAX_SCALE: f32 = 5.0;

/// Where the image page's picture sits: `scale` times its fit size, its
/// top-left corner at `(offset_x, offset_y)` from the area's, over the
/// picture's own pixel `image` size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageZoom {
    /// The scale over the fit size, 1 at fit.
    scale: f32,
    /// The picture's left edge from the area's.
    offset_x: f32,
    /// The picture's top edge from the area's.
    offset_y: f32,
    /// The picture's own pixel size, width first.
    image: (f32, f32),
}

impl ImageZoom {
    /// A zoom at the fit size over a picture `image` pixels wide and tall.
    #[must_use]
    pub fn new(image: (f32, f32)) -> Self {
        Self {
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            image,
        }
    }

    /// A pinch of `delta` (0.5 is 50% closer) about `anchor`, a point
    /// relative to the area's top-left corner: the point under the fingers
    /// stays under them, and the scale stops at the fit size and
    /// [`MAX_SCALE`].
    pub fn pinch(&mut self, delta: f32, anchor: (f32, f32), area: (f32, f32)) {
        self.clamp(area);
        let scale = (self.scale * (1.0 + delta)).clamp(1.0, MAX_SCALE);
        let k = scale / self.scale;
        self.scale = scale;
        self.offset_x = anchor.0 - (anchor.0 - self.offset_x) * k;
        self.offset_y = anchor.1 - (anchor.1 - self.offset_y) * k;
        self.clamp(area);
    }

    /// A drag of `(dx, dy)` pixels: the picture moves with the finger until
    /// its edge reaches the area's, and it moves nowhere at the fit size.
    pub fn pan(&mut self, dx: f32, dy: f32, area: (f32, f32)) {
        self.clamp(area);
        self.offset_x += dx;
        self.offset_y += dy;
        self.clamp(area);
    }

    /// A double tap at `at`, relative to the area's top-left corner:
    /// zoomed, it returns to the fit size; at the fit size, it zooms in to
    /// twice it about `at`.
    pub fn double_tap(&mut self, at: (f32, f32), area: (f32, f32)) {
        if self.scale > 1.0 {
            *self = Self::new(self.image);
            self.clamp(area);
        } else {
            self.pinch(1.0, at, area);
        }
    }

    /// Keeps each offset within the picture's own edges for `area`: a side
    /// longer than the area's stops at the area's edges, and a shorter side
    /// stays centred.
    pub fn clamp(&mut self, area: (f32, f32)) {
        let (width, height) = self.box_size(area);
        self.offset_x = along(self.offset_x, width, area.0);
        self.offset_y = along(self.offset_y, height, area.1);
    }

    /// The picture's drawn size for `area`, at the current scale: its fit
    /// size in the area, scaled, keeping its own shape.
    #[must_use]
    pub fn box_size(self, area: (f32, f32)) -> (f32, f32) {
        let (fitted_w, fitted_h) = self.fitted(area);
        (fitted_w * self.scale, fitted_h * self.scale)
    }

    /// The picture's fit in `area`: the largest box of the picture's own
    /// shape the area holds. A picture of no size takes the area's.
    fn fitted(self, area: (f32, f32)) -> (f32, f32) {
        let (image_w, image_h) = self.image;
        if image_w <= 0.0 || image_h <= 0.0 {
            return area;
        }
        let k = (area.0 / image_w).min(area.1 / image_h);
        (image_w * k, image_h * k)
    }

    /// The picture's top-left corner from the area's, in pixels.
    #[must_use]
    pub fn offset(self) -> (f32, f32) {
        (self.offset_x, self.offset_y)
    }
}

/// `offset` kept to the picture's own edges on one axis: between the area's
/// start and the picture's end when the picture is longer than the area, and
/// centred when it is shorter.
fn along(offset: f32, picture: f32, area: f32) -> f32 {
    if picture > area {
        offset.min(0.0).max(area - picture)
    } else {
        (area - picture) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page area, and the picture that fills it exactly: 400 by 800
    /// pixels.
    const AREA: (f32, f32) = (400.0, 800.0);

    /// The phone's image page area: 390 by 650 pixels.
    const PAGE: (f32, f32) = (390.0, 650.0);

    /// A landscape picture, 1920 by 1080 pixels.
    const PHOTO: (f32, f32) = (1920.0, 1080.0);

    /// A zoom at `scale` times the fit size, its top-left corner `offset`
    /// from the area's, over a picture filling `AREA`.
    fn at(scale: f32, offset: (f32, f32)) -> ImageZoom {
        ImageZoom {
            scale,
            offset_x: offset.0,
            offset_y: offset.1,
            image: AREA,
        }
    }

    /// [`PHOTO`] on [`PAGE`], zoomed `scale` times about the area's centre.
    fn centred(scale: f32) -> ImageZoom {
        let mut zoom = ImageZoom::new(PHOTO);
        zoom.pinch(scale - 1.0, (PAGE.0 / 2.0, PAGE.1 / 2.0), PAGE);
        zoom
    }

    #[test]
    fn a_fresh_zoom_is_the_fit_size() {
        let zoom = ImageZoom::new(AREA);
        assert_eq!(zoom.box_size(AREA), AREA);
        assert_eq!(zoom.offset(), (0.0, 0.0));
    }

    #[test]
    fn a_pinch_zooms_about_the_point_under_the_fingers() {
        let mut zoom = ImageZoom::new(AREA);
        zoom.pinch(1.0, (200.0, 400.0), AREA);
        assert_eq!(zoom.box_size(AREA), (800.0, 1600.0));
        assert_eq!(zoom.offset(), (-200.0, -400.0));
    }

    #[test]
    fn pinches_compound_multiplicatively() {
        let mut zoom = ImageZoom::new(AREA);
        zoom.pinch(0.5, (0.0, 0.0), AREA);
        zoom.pinch(0.5, (0.0, 0.0), AREA);
        assert_eq!(zoom.box_size(AREA), (900.0, 1800.0), "2.25 times the fit");
        assert_eq!(zoom.offset(), (0.0, 0.0));
    }

    #[test]
    fn a_pinch_stops_at_five_times_the_fit() {
        let mut zoom = ImageZoom::new(AREA);
        zoom.pinch(10.0, (200.0, 400.0), AREA);
        assert_eq!(zoom.box_size(AREA), (2000.0, 4000.0), "five times the fit");
        let (x, y) = zoom.offset();
        assert!((-1600.0..=0.0).contains(&x), "x {x} inside the picture");
        assert!((-3200.0..=0.0).contains(&y), "y {y} inside the picture");
    }

    #[test]
    fn a_pinch_below_the_fit_stays_at_the_fit() {
        let mut zoom = ImageZoom::new(AREA);
        zoom.pinch(-0.9, (200.0, 400.0), AREA);
        assert_eq!(zoom.box_size(AREA), AREA);
        assert_eq!(zoom.offset(), (0.0, 0.0));
    }

    #[test]
    fn a_pan_at_the_fit_moves_nothing() {
        let mut zoom = ImageZoom::new(AREA);
        zoom.pan(50.0, 50.0, AREA);
        assert_eq!(zoom.offset(), (0.0, 0.0));
    }

    #[test]
    fn a_pan_stops_at_the_picture_edges() {
        let mut zoom = at(2.0, (-200.0, -400.0));
        zoom.pan(300.0, 0.0, AREA);
        assert_eq!(zoom.offset().0, 0.0, "the left edge stops at the area's");

        let mut zoom = at(2.0, (-200.0, -400.0));
        zoom.pan(-1000.0, 0.0, AREA);
        assert_eq!(
            zoom.offset().0,
            -400.0,
            "the right edge stops at the area's"
        );
    }

    #[test]
    fn a_double_tap_returns_to_the_fit_and_zooms_in_again() {
        let mut zoom = at(2.0, (-200.0, -400.0));
        zoom.double_tap((0.0, 0.0), AREA);
        assert_eq!(zoom.box_size(AREA), AREA);
        assert_eq!(zoom.offset(), (0.0, 0.0));

        let mut zoom = ImageZoom::new(AREA);
        zoom.double_tap((100.0, 100.0), AREA);
        assert_eq!(zoom.box_size(AREA), (800.0, 1600.0));
        assert_eq!(zoom.offset(), (-100.0, -100.0));
    }

    #[test]
    fn a_smaller_area_clamps_the_offset() {
        let mut zoom = at(2.0, (-400.0, -800.0));
        zoom.clamp((200.0, 400.0));
        assert_eq!(zoom.offset(), (-200.0, -400.0));
    }

    #[test]
    fn a_pinch_keeps_the_point_under_the_fingers_on_a_letterboxed_picture() {
        let zoom = centred(5.0);
        let (width, height) = zoom.box_size(PAGE);
        assert_eq!(
            (width, height),
            (1950.0, 1096.875),
            "the picture's own shape, five times its fit"
        );
        let (x, y) = zoom.offset();
        assert_eq!(
            (x + width / 2.0, y + height / 2.0),
            (195.0, 325.0),
            "the picture's centre stays under the area's"
        );
    }

    #[test]
    fn a_tall_picture_stops_at_the_area_edges() {
        let mut down = centred(5.0);
        assert_eq!(down.box_size(PAGE).1, 1096.875, "taller than the area");
        down.pan(0.0, 5000.0, PAGE);
        assert_eq!(down.offset().1, 0.0, "its top stops at the area's top");

        let mut up = centred(5.0);
        let (_, height) = up.box_size(PAGE);
        up.pan(0.0, -5000.0, PAGE);
        assert_eq!(
            up.offset().1 + height,
            650.0,
            "its bottom stops at the area's bottom"
        );
    }

    #[test]
    fn a_short_picture_stays_centred_however_it_is_panned() {
        let mut zoom = centred(2.0);
        let (width, height) = zoom.box_size(PAGE);
        assert_eq!((width, height), (780.0, 438.75), "shorter than the area");
        let centred_y = zoom.offset().1;
        assert_eq!(centred_y, (650.0 - height) / 2.0, "centred at first");
        zoom.pan(500.0, 5000.0, PAGE);
        assert_eq!(zoom.offset().1, centred_y, "a short side stays centred");
    }

    #[test]
    fn a_square_picture_on_a_square_area_zooms_as_before() {
        let area = (400.0, 400.0);
        let mut zoom = ImageZoom::new(area);
        zoom.pinch(1.0, (200.0, 200.0), area);
        assert_eq!(zoom.box_size(area), (800.0, 800.0));
        assert_eq!(zoom.offset(), (-200.0, -200.0));
    }
}
