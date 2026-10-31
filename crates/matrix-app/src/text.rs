// Glyph atlas + text quads. Pure Rust: ab_glyph rasterises the outlines into an
// R8 coverage atlas that the glyph shader samples. No browser, no font stack
// other than the one TTF this app opens at startup.

use std::collections::HashMap;

use ab_glyph::{Font as _, ScaleFont as _};

use crate::render::GInst;

pub const ATLAS_CELL: usize = 32;
pub const ATLAS_COLS: usize = 16;

#[derive(Clone, Copy, Default)]
pub struct GlyphSlot {
    pub u0: f32,
    pub v0: f32,
    pub du: f32,
    pub dv: f32,
    pub ox: f32, // bitmap offset from the pen origin, CSS px
    pub oy: f32,
    pub w: f32,
    pub h: f32,
    pub adv: f32, // advance width, CSS px
}

pub struct Face {
    pub px: u32,
    pub size: usize,
    pub ascent: f32, // pen-to-line-top distance, CSS px
    pub pixels: Vec<u8>,
    pub slots: HashMap<char, GlyphSlot>,
}

/// ASCII printable plus the katakana range the rain alphabet uses. The rain
/// alphabet itself is defined in render.rs and is a subset of this.
pub fn alphabet() -> Vec<char> {
    let mut v: Vec<char> = (0x20u32..=0x7eu32).filter_map(char::from_u32).collect();
    for c in 0xff66u32..=0xff9du32 {
        if let Some(ch) = char::from_u32(c) {
            v.push(ch);
        }
    }
    for ch in "±÷×≠≤≥∞≡⊕⊗∴∵¬∂∇·→←↑↓".chars() {
        v.push(ch);
    }
    v.dedup();
    v
}

impl Face {
    pub fn build(font_path: &str, px: u32, dpr: f32) -> Result<Face, String> {
        // Leak the font bytes once: FontRef borrows them for the whole process,
        // and each face reuses the same mapping.
        static FONT: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
        let bytes: &'static [u8] = FONT.get_or_init(|| {
            let b = std::fs::read(font_path).unwrap_or_default();
            Box::leak(b.into_boxed_slice())
        });
        if bytes.is_empty() {
            return Err(format!("font not readable: {font_path}"));
        }
        let font = ab_glyph::FontRef::try_from_slice_and_index(bytes, 0)
            .map_err(|e| format!("font parse {font_path}: {e:?}"))?;
        let scale = ab_glyph::PxScale::from(px as f32 * dpr);
        let scaled = font.as_scaled(scale);
        let size = ATLAS_CELL * ATLAS_COLS;
        let ascent = scaled.ascent() / dpr;
        let mut pixels = vec![0u8; size * size];
        let mut slots = HashMap::new();
        let alpha = alphabet();
        for (i, ch) in alpha.iter().enumerate() {
            if i >= ATLAS_COLS * ATLAS_COLS {
                break;
            }
            let gid = font.glyph_id(*ch);
            let g = gid.with_scale(scale);
            let adv = scaled.h_advance(gid) / dpr;
            let adv = if adv > 0.0 { adv } else { px as f32 * 0.5 };
            // Characters with no outline (space, thin space, ...) still need a
            // slot: skipping them here made every space in the UI disappear and
            // every measured text width too small.
            let Some(outline) = font.outline_glyph(g) else {
                slots.insert(
                    *ch,
                    GlyphSlot { w: 0.0, h: 0.0, adv, ..Default::default() },
                );
                continue;
            };
            let b = outline.px_bounds();
            let cx = (i % ATLAS_COLS) * ATLAS_CELL;
            let cy = (i / ATLAS_COLS) * ATLAS_CELL;
            let mut any = false;
            outline.draw(|x, y, cov| {
                let (x, y) = (cx + x as usize, cy + y as usize);
                if x < size && y < size {
                    let v = (cov * 255.0) as u8;
                    if v > pixels[y * size + x] {
                        pixels[y * size + x] = v;
                    }
                    if v > 8 {
                        any = true;
                    }
                }
            });
            let w = ((b.max.x - b.min.x).ceil().max(1.0)) / dpr;
            let h = ((b.max.y - b.min.y).ceil().max(1.0)) / dpr;
            // The bitmap is written at the cell's top-left corner, so the UV
            // rect is the bitmap itself — not the whole 32 px cell (sampling the
            // cell would shrink every glyph into a corner of its quad).
            let bw = ((b.max.x - b.min.x).ceil().max(1.0)) as f32;
            let bh = ((b.max.y - b.min.y).ceil().max(1.0)) as f32;
            slots.insert(
                *ch,
                GlyphSlot {
                    u0: cx as f32 / size as f32,
                    v0: cy as f32 / size as f32,
                    du: bw.min(ATLAS_CELL as f32) / size as f32,
                    dv: bh.min(ATLAS_CELL as f32) / size as f32,
                    ox: if any { b.min.x.floor() / dpr } else { 0.0 },
                    oy: if any { b.min.y.floor() / dpr } else { 0.0 },
                    w: if any { w } else { 0.0 },
                    h: if any { h } else { 0.0 },
                    adv: if adv > 0.0 { adv } else { px as f32 * 0.5 },
                },
            );
        }
        Ok(Face { px, size, ascent, pixels, slots })
    }
}

/// Append the quads for one string at (x, y) (y is the TOP of the line box, not
/// the baseline: glyph bitmaps are anchored on the baseline, so the face's
/// ascent is added here). Returns the pen x after the last glyph. Characters
/// outside the atlas are skipped rather than replaced with an invented box.
pub fn push_str(
    out: &mut Vec<GInst>,
    face: &Face,
    x: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
    max_x: f32,
) -> f32 {
    let mut pen = x;
    for ch in text.chars() {
        let Some(s) = face.slots.get(&ch) else { continue };
        if s.w > 0.0 {
            if pen + s.adv > max_x {
                break;
            }
            out.push(GInst {
                rect: [pen + s.ox, y + face.ascent + s.oy, s.w, s.h],
                uv: [s.u0, s.v0, s.du, s.dv],
                color,
            });
        }
        pen += s.adv;
    }
    pen
}

pub fn text_width(face: &Face, text: &str) -> f32 {
    text.chars()
        .filter_map(|c| face.slots.get(&c).map(|s| s.adv))
        .sum()
}

/// Truncate a string so it fits in `max_w` CSS px, with a trailing "..".
pub fn fit(face: &Face, text: &str, max_w: f32) -> String {
    if text_width(face, text) <= max_w {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0.0;
    for ch in text.chars() {
        let adv = face.slots.get(&ch).map(|s| s.adv).unwrap_or(6.0);
        if w + adv > max_w - face.px as f32 * 0.6 {
            break;
        }
        w += adv;
        out.push(ch);
    }
    out.push_str("..");
    out
}
