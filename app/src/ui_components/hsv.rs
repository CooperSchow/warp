//! HSV ↔ RGB conversions for the custom tab color picker.
//!
//! Hue is in degrees `[0, 360)`, saturation and value in `[0, 1]`. All
//! conversions are the standard piecewise-linear HSV math, so a hue strip can
//! be rendered exactly with six 2-stop linear gradients.

/// Converts HSV to 8-bit RGB. `h` is wrapped into `[0, 360)`; `s`/`v` are
/// clamped to `[0, 1]`.
pub(crate) fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.);
    let s = s.clamp(0., 1.);
    let v = v.clamp(0., 1.);

    let c = v * s;
    let x = c * (1. - ((h / 60.) % 2. - 1.).abs());
    let m = v - c;

    let (r1, g1, b1) = match h {
        h if h < 60. => (c, x, 0.),
        h if h < 120. => (x, c, 0.),
        h if h < 180. => (0., c, x),
        h if h < 240. => (0., x, c),
        h if h < 300. => (x, 0., c),
        _ => (c, 0., x),
    };

    (
        ((r1 + m) * 255.).round() as u8,
        ((g1 + m) * 255.).round() as u8,
        ((b1 + m) * 255.).round() as u8,
    )
}

/// Converts 8-bit RGB to HSV `(h in [0, 360), s in [0, 1], v in [0, 1])`.
/// Achromatic inputs report hue 0 and saturation 0.
pub(crate) fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.;
    let g = g as f32 / 255.;
    let b = b as f32 / 255.;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let h = if delta == 0. {
        0.
    } else if max == r {
        60. * (((g - b) / delta).rem_euclid(6.))
    } else if max == g {
        60. * ((b - r) / delta + 2.)
    } else {
        60. * ((r - g) / delta + 4.)
    };

    let s = if max == 0. { 0. } else { delta / max };

    (h.rem_euclid(360.), s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_colors_round_trip() {
        let cases: [(f32, f32, f32, (u8, u8, u8)); 8] = [
            (0., 1., 1., (255, 0, 0)),
            (60., 1., 1., (255, 255, 0)),
            (120., 1., 1., (0, 255, 0)),
            (180., 1., 1., (0, 255, 255)),
            (240., 1., 1., (0, 0, 255)),
            (300., 1., 1., (255, 0, 255)),
            (0., 0., 1., (255, 255, 255)),
            (0., 0., 0., (0, 0, 0)),
        ];
        for (h, s, v, rgb) in cases {
            assert_eq!(hsv_to_rgb(h, s, v), rgb, "hsv({h},{s},{v})");
        }
    }

    #[test]
    fn eqho_purple_round_trips() {
        // #502fef
        let (h, s, v) = rgb_to_hsv(0x50, 0x2f, 0xef);
        let (r, g, b) = hsv_to_rgb(h, s, v);
        assert_eq!((r, g, b), (0x50, 0x2f, 0xef));
    }

    #[test]
    fn all_rgb_corners_round_trip_exactly() {
        for r in [0u8, 51, 102, 153, 204, 255] {
            for g in [0u8, 51, 102, 153, 204, 255] {
                for b in [0u8, 51, 102, 153, 204, 255] {
                    let (h, s, v) = rgb_to_hsv(r, g, b);
                    assert_eq!(hsv_to_rgb(h, s, v), (r, g, b), "rgb({r},{g},{b})");
                }
            }
        }
    }

    #[test]
    fn out_of_range_inputs_are_normalized() {
        assert_eq!(hsv_to_rgb(360., 1., 1.), (255, 0, 0));
        assert_eq!(hsv_to_rgb(-60., 1., 1.), (255, 0, 255));
        assert_eq!(hsv_to_rgb(0., 2., 2.), (255, 0, 0));
        assert_eq!(hsv_to_rgb(0., -1., -1.), (0, 0, 0));
    }
}
