// One shader, two instanced-quad passes:
//   vs_quad  : solid alpha-blended quads (residency cells, gridlines, activation lamps, bloom)
//   vs_glyph : textured quads out of the glyph atlas (Matrix rain glyphs)

struct Globals {
    res: vec2<f32>,    // physical surface px
    scale: f32,        // dpr: CSS px -> physical px
    _pad: f32,
};
@group(0) @binding(0) var<uniform> G: Globals;

struct Inst {
    rect: vec4<f32>,    // x, y, w, h  in CSS px (top-left origin)
    color: vec4<f32>,   // straight alpha
};
@group(1) @binding(0) var<storage, read> quads: array<Inst>;

struct GInst {
    rect: vec4<f32>,    // x, y, w, h
    uv: vec4<f32>,      // u0, v0, du, dv
    color: vec4<f32>,
};
@group(1) @binding(0) var<storage, read> glyphs: array<GInst>;

@group(2) @binding(0) var atlas: texture_2d<f32>;
@group(2) @binding(1) var samp: sampler;

struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
};

// Colours are authored in sRGB (the same values the bench page used in CSS).
// The swapchain is an sRGB format, so the hardware encodes linear->sRGB on
// write; converting here keeps every authored colour exactly as intended
// instead of washing the dark greens out to grey-green.
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

const CORNERS: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
);

// geometry arrives in CSS px (the same coordinate space the web page draws in); the surface is
// physical px, so the dpr is applied here
fn to_ndc(px: vec2<f32>) -> vec4<f32> {
    let p = px * G.scale;
    return vec4<f32>(p.x / G.res.x * 2.0 - 1.0, 1.0 - p.y / G.res.y * 2.0, 0.0, 1.0);
}

@vertex
fn vs_quad(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VSOut {
    let c = CORNERS[vi];
    let r = quads[ii].rect;
    var out: VSOut;
    out.pos = to_ndc(r.xy + c * r.zw);
    out.color = quads[ii].color;
    out.uv = c;
    return out;
}

@fragment
fn fs_quad(in: VSOut) -> @location(0) vec4<f32> {
    return vec4<f32>(srgb_to_linear(in.color.rgb) * in.color.a, in.color.a);
}

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VSOut {
    let c = CORNERS[vi];
    let g = glyphs[ii];
    var out: VSOut;
    out.pos = to_ndc(g.rect.xy + c * g.rect.zw);
    out.color = g.color;
    out.uv = g.uv.xy + c * g.uv.zw;
    return out;
}

@fragment
fn fs_glyph(in: VSOut) -> @location(0) vec4<f32> {
    let cov = textureSample(atlas, samp, in.uv).r;
    let a = cov * in.color.a;
    return vec4<f32>(srgb_to_linear(in.color.rgb) * a, a);
}
