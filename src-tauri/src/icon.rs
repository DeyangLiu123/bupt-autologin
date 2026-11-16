//! 托盘图标在运行时按状态颜色绘制，不需要额外的图片资源。

use tauri::image::Image;

const SIZE: usize = 64;
const SAMPLES: usize = 4;

/// Wi‑Fi 扇形：底部圆点 + 两道 90° 圆弧。
fn inside(x: f32, y: f32) -> bool {
    let (cx, cy) = (32.0, 50.0);
    let dx = x - cx;
    let dy = cy - y;
    let r = (dx * dx + dy * dy).sqrt();
    if r <= 6.0 {
        return true;
    }
    if dy <= 0.0 || dx.abs() > dy {
        return false;
    }
    (15.0..=22.0).contains(&r) || (29.0..=36.0).contains(&r)
}

pub fn render(rgb: [u8; 3]) -> Image<'static> {
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let step = 1.0 / SAMPLES as f32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) * step;
                    let py = y as f32 + (sy as f32 + 0.5) * step;
                    hits += inside(px, py) as usize;
                }
            }
            if hits > 0 {
                let i = (y * SIZE + x) * 4;
                rgba[i..i + 3].copy_from_slice(&rgb);
                rgba[i + 3] = (hits * 255 / (SAMPLES * SAMPLES)) as u8;
            }
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}
