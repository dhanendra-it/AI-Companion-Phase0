use tao::window::Icon;

const SIZE: usize = 64;

pub fn window_icon() -> Option<Icon> {
    let mut pixels = vec![0; SIZE * SIZE * 4];

    rounded_rect(&mut pixels, 2.0, 2.0, 60.0, 60.0, 17.0, [13, 23, 40, 255]);
    rounded_rect(&mut pixels, 4.0, 4.0, 56.0, 56.0, 15.0, [23, 38, 61, 255]);

    // Blue headphone band above the large rectangular robot head.
    let band = |t: f32| (11.0 + 42.0 * t, 36.0 - 27.0 * (1.0 - (2.0 * t - 1.0).powi(2)));
    stroke_curve(&mut pixels, band, 0.0, 1.0, 5.3, [8, 17, 35, 255]);
    stroke_curve(&mut pixels, band, 0.08, 0.92, 2.0, [46, 126, 255, 255]);
    stroke_curve(&mut pixels, band, 0.13, 0.88, 0.8, [71, 226, 255, 255]);

    // Squared head, luminous bezel, and dark glass face.
    rounded_rect(&mut pixels, 9.0, 25.0, 46.0, 36.0, 13.0, [7, 14, 29, 255]);
    rounded_rect(&mut pixels, 12.0, 27.0, 40.0, 32.0, 10.5, [104, 128, 174, 255]);
    rounded_rect(&mut pixels, 14.0, 28.5, 36.0, 29.0, 9.5, [222, 235, 255, 255]);
    rounded_rect(&mut pixels, 17.0, 31.5, 30.0, 23.0, 7.0, [8, 15, 30, 255]);

    // Headphone cups frame the head.
    rounded_rect(&mut pixels, 5.0, 37.0, 11.0, 19.0, 5.0, [9, 18, 39, 255]);
    rounded_rect(&mut pixels, 6.0, 38.0, 9.0, 17.0, 4.5, [48, 92, 218, 255]);
    rounded_rect(&mut pixels, 48.0, 38.0, 10.0, 17.0, 4.5, [48, 92, 218, 255]);
    rounded_rect(&mut pixels, 7.5, 40.0, 2.0, 13.0, 1.0, [44, 226, 255, 255]);
    rounded_rect(&mut pixels, 54.0, 40.0, 2.0, 13.0, 1.0, [44, 226, 255, 255]);

    // Two unmistakable cyan screen eyes.
    rounded_rect(&mut pixels, 22.0, 40.0, 7.5, 8.5, 2.2, [0, 222, 246, 255]);
    rounded_rect(&mut pixels, 34.5, 40.0, 7.5, 8.5, 2.2, [0, 222, 246, 255]);

    Icon::from_rgba(pixels, SIZE as u32, SIZE as u32).ok()
}

fn rounded_rect(buf: &mut [u8], x: f32, y: f32, w: f32, h: f32, r: f32, color: [u8; 4]) {
    let left = x.max(0.0) as usize;
    let top = y.max(0.0) as usize;
    let right = (x + w).min(SIZE as f32) as usize;
    let bottom = (y + h).min(SIZE as f32) as usize;
    for py in top..bottom {
        for px in left..right {
            let qx = ((px as f32 + 0.5) - (x + r)).max(0.0).max((x + w - r) - (px as f32 + 0.5));
            let qy = ((py as f32 + 0.5) - (y + r)).max(0.0).max((y + h - r) - (py as f32 + 0.5));
            if qx * qx + qy * qy <= r * r { put(buf, px, py, color); }
        }
    }
}

fn stroke_curve(buf: &mut [u8], curve: impl Fn(f32) -> (f32, f32), from: f32, to: f32, width: f32, color: [u8; 4]) {
    let steps = 80;
    let mut previous = curve(from);
    for i in 1..=steps {
        let t = from + (to - from) * i as f32 / steps as f32;
        let point = curve(t);
        line(buf, previous, point, width, color);
        previous = point;
    }
}

fn line(buf: &mut [u8], a: (f32, f32), b: (f32, f32), width: f32, color: [u8; 4]) {
    let min_x = (a.0.min(b.0) - width).max(0.0) as usize;
    let max_x = (a.0.max(b.0) + width).min(SIZE as f32) as usize;
    let min_y = (a.1.min(b.1) - width).max(0.0) as usize;
    let max_y = (a.1.max(b.1) + width).min(SIZE as f32) as usize;
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = if len2 == 0.0 { 0.0 } else { (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0) };
            let ex = px - (a.0 + t * dx);
            let ey = py - (a.1 + t * dy);
            if ex * ex + ey * ey <= width * width / 4.0 { put(buf, x, y, color); }
        }
    }
}

fn put(buf: &mut [u8], x: usize, y: usize, color: [u8; 4]) {
    let at = (y * SIZE + x) * 4;
    buf[at..at + 4].copy_from_slice(&color);
}
