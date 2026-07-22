// One pipeline for everything abner draws: flat rects, video quads,
// compare modes (delta / split / checker / blend), and glyph quads.
// Instanced unit quads, logical-pixel coordinates, top-left origin
// (the switchblade tile-shader shape, minus the grid machinery).

struct U {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex_a: texture_2d<f32>;
@group(0) @binding(2) var tex_b: texture_2d<f32>;
@group(0) @binding(3) var tex_g: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;

struct In {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) mode: f32,
    @location(5) p0: f32,
    @location(6) p1: f32,
    @location(7) pad: f32,
};

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) @interpolate(flat) mode: u32,
    @location(4) p0: f32,
    @location(5) p1: f32,
    @location(6) size: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, in: In) -> Out {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[vi];
    let local = c * in.size;
    let p = in.pos + local;
    var out: Out;
    out.pos = vec4<f32>((p / u.viewport) * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = mix(in.uv.xy, in.uv.zw, c);
    out.local = local;
    out.color = in.color;
    out.mode = u32(in.mode);
    out.p0 = in.p0;
    out.p1 = in.p1;
    out.size = in.size;
    return out;
}

// Modes (keep in sync with render.rs):
// 0 flat  1 video A  2 delta |A-B|*gain  3 split at p0  4 checker(p0 px)
// 5 blend mix(A,B,p0)  6 glyph (tex_g.r * color)

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    // Sample unconditionally (uniform control flow), select after.
    let a = textureSample(tex_a, samp, in.uv);
    let b = textureSample(tex_b, samp, in.uv);
    let g = textureSample(tex_g, samp, in.uv).r;
    switch in.mode {
        case 0u: {
            return in.color;
        }
        case 1u: {
            return vec4<f32>(a.rgb, 1.0);
        }
        case 2u: {
            return vec4<f32>(abs(a.rgb - b.rgb) * in.p1, 1.0);
        }
        case 3u: {
            let edge = in.p0 * in.size.x;
            if abs(in.local.x - edge) < 0.75 {
                return vec4<f32>(1.0, 1.0, 1.0, 0.9);
            }
            if in.local.x < edge {
                return vec4<f32>(a.rgb, 1.0);
            }
            return vec4<f32>(b.rgb, 1.0);
        }
        case 4u: {
            let cell = vec2<i32>(floor(in.local / max(in.p0, 1.0)));
            if ((cell.x + cell.y) & 1) == 0 {
                return vec4<f32>(a.rgb, 1.0);
            }
            return vec4<f32>(b.rgb, 1.0);
        }
        case 5u: {
            return vec4<f32>(mix(a.rgb, b.rgb, in.p0), 1.0);
        }
        case 6u: {
            return vec4<f32>(in.color.rgb, in.color.a * g);
        }
        default: {
            return in.color;
        }
    }
}
