//! Software drawing of anti-aliased rounded rectangles into premultiplied ARGB8888.

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h] }
    }

    pub fn clear(&mut self) {
        self.px.fill(0);
    }

    /// Copies `sprite` with its top-left corner at (0, y); rows outside are clipped.
    pub fn blit_rows(&mut self, sprite: &Canvas, y: i32) {
        let w = self.w.min(sprite.w);
        for sy in 0..sprite.h {
            let dy = y + sy as i32;
            if dy < 0 || dy >= self.h as i32 {
                continue;
            }
            let d = dy as usize * self.w;
            self.px[d..d + w].copy_from_slice(&sprite.px[sy * sprite.w..sy * sprite.w + w]);
        }
    }

    /// Fills a rounded rectangle (x, y = top-left) with gray level `gray` and opacity `alpha`, both 0..=1.
    #[allow(clippy::too_many_arguments)] // a drawing primitive: plain numbers read best here
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, gray: f32, alpha: f32) {
        let (hw, hh) = (w * 0.5, h * 0.5);
        let (cx, cy) = (x + hw, y + hh);
        let r = r.min(hw).min(hh);
        let x0 = (x.floor().max(0.0)) as usize;
        let y0 = (y.floor().max(0.0)) as usize;
        let x1 = ((x + w).ceil().max(0.0) as usize).min(self.w);
        let y1 = ((y + h).ceil().max(0.0) as usize).min(self.h);
        for py in y0..y1 {
            let qy = (py as f32 + 0.5 - cy).abs() - (hh - r);
            let row = py * self.w;
            for px in x0..x1 {
                let qx = (px as f32 + 0.5 - cx).abs() - (hw - r);
                let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
                let cover = (0.5 - dist).clamp(0.0, 1.0) * alpha;
                if cover > 0.0 {
                    blend(&mut self.px[row + px], gray, cover);
                }
            }
        }
    }
}

/// Source-over blend of a gray pixel onto a premultiplied ARGB pixel.
fn blend(dst: &mut u32, gray: f32, a: f32) {
    let keep = 1.0 - a;
    let src_c = gray * a * 255.0;
    let d = *dst;
    let ch = |shift: u32, src: f32| ((((d >> shift) & 0xff) as f32 * keep + src + 0.5) as u32).min(255) << shift;
    *dst = ch(24, a * 255.0) | ch(16, src_c) | ch(8, src_c) | ch(0, src_c);
}
