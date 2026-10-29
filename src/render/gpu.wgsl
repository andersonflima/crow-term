// Crow Term GPU renderer: one instanced quad per fill, glyph or decoration.
// Positions and texel coordinates are integers in physical pixels; glyph
// texels are fetched exactly (textureLoad), never filtered, so output matches
// the software framebuffer up to 8-bit blending rounding.
struct Globals {
    size: vec2<f32>,
    pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_atlas: texture_2d<f32>;
@group(0) @binding(2) var color_atlas: texture_2d<f32>;

struct Instance {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) kind: u32,
};
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32, instance: Instance) -> VsOut {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    let pixel = instance.rect.xy + corner * instance.rect.zw;
    var out: VsOut;
    out.position = vec4<f32>(
        pixel.x / globals.size.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.size.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = instance.uv + corner * instance.rect.zw;
    out.color = instance.color;
    out.kind = instance.kind;
    return out;
}

// kind 0: opaque fill; 1: coverage mask tinted with the color; 2: color glyph.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(floor(in.uv));
    if in.kind == 1u {
        return vec4<f32>(in.color.rgb, textureLoad(mask_atlas, texel, 0).r);
    }
    if in.kind == 2u {
        return textureLoad(color_atlas, texel, 0);
    }
    return vec4<f32>(in.color.rgb, 1.0);
}

// Block cursor: blend state computes 1 - destination (RGB inversion).
@fragment
fn fs_invert(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
