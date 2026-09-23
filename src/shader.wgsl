// One pipeline for everything abner draws: rounded/bordered rects, video
// quads, compare modes (delta / split / checker / blend), and glyph quads.
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
// The launch wordmark. One texture for the life of the process, bound in
// every group so mode 7 needs no batch key of its own.
@group(0) @binding(5) var tex_l: texture_2d<f32>;
@group(0) @binding(6) var tex_m: texture_2d<f32>;
// The launch plate — bound in every group for the same reason as the
// wordmark, so modes 10 and 11 need no batch key either.
@group(0) @binding(7) var tex_p: texture_2d<f32>;

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
    // Mode 0 reuses the (otherwise unused) uv slot as a flat border
    // colour, so borders cost no extra vertex attribute.
    @location(7) @interpolate(flat) border: vec4<f32>,
    @location(8) @interpolate(flat) pad: f32,
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
    out.border = in.uv;
    out.pad = in.pad;
    return out;
}

/// Signed distance to a rounded box centred on the origin. Negative
/// inside, positive outside — the standard iq formulation.
fn sd_round_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

/// Signed distance to the triangle (a, b, c) — iq's exact formulation.
fn sd_triangle(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>, c: vec2<f32>) -> f32 {
    let e0 = b - a; let e1 = c - b; let e2 = a - c;
    let v0 = p - a; let v1 = p - b; let v2 = p - c;
    let pq0 = v0 - e0 * clamp(dot(v0, e0) / dot(e0, e0), 0.0, 1.0);
    let pq1 = v1 - e1 * clamp(dot(v1, e1) / dot(e1, e1), 0.0, 1.0);
    let pq2 = v2 - e2 * clamp(dot(v2, e2) / dot(e2, e2), 0.0, 1.0);
    let s = sign(e0.x * e2.y - e0.y * e2.x);
    let d = min(min(vec2<f32>(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
                    vec2<f32>(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
                    vec2<f32>(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)));
    return -sqrt(d.x) * sign(d.y);
}

/// Analytic 1-pixel coverage from a signed distance, using the screen
/// derivative so the antialiasing stays one PHYSICAL pixel wide at any
/// scale factor or window size.
fn cov(d: f32) -> f32 {
    return clamp(0.5 - d / max(fwidth(d), 1e-4), 0.0, 1.0);
}

/// UI colours are authored as sRGB hex (straight off the design), but the
/// surface is `*UnormSrgb` — the hardware encodes whatever the shader
/// writes, so a raw sRGB value gets encoded a second time and lands pale
/// and desaturated. Decode here so #a6e22e reaches the glass as #a6e22e.
/// Video modes need no such fix: their textures are sRGB too, so sampling
/// already decodes them to linear.
fn ui_color(c: vec4<f32>) -> vec4<f32> {
    let lo = c.rgb / 12.92;
    let hi = pow((c.rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return vec4<f32>(select(hi, lo, c.rgb <= vec3<f32>(0.04045)), c.a);
}

// Modes (keep in sync with render.rs):
// 0 rect (p0 radius, p1 border width, uv = border colour, pad = fade-up)
// 1 video A  2 delta |A-B|*gain  3 split at p0  4 checker(p0 px)
// 5 blend mix(A,B,p0)  6 glyph (tex_g.r * color)  7 logo (tex_l * color.a)
// 8 binary mask (tex_m.r selects red/blue, alpha 0.5)
// 9 triangle filling the quad, pointing right (p1 = 1: left), p0 corner radius
// 10 launch plate (tex_p * color.a)
// 11 wordmark projected onto the plate's floor (p0 pinhole distance,
//    p1 floor tilt, pad the standing mark's height, uv slot = its ink box)
// 12 the wordmark's silhouette as a soft shadow (p0 = mip level)

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    // Sample unconditionally (uniform control flow), select after.
    let a = textureSample(tex_a, samp, in.uv);
    let b = textureSample(tex_b, samp, in.uv);
    let g = textureSample(tex_g, samp, in.uv).r;
    let l = textureSample(tex_l, samp, in.uv);
    let pl = textureSample(tex_p, samp, in.uv);
    switch in.mode {
        case 0u: {
            let half = in.size * 0.5;
            let r = clamp(in.p0, 0.0, min(half.x, half.y));
            let d = sd_round_box(in.local - half, half, r);
            var col = ui_color(in.color);
            // Border band: the outer `p1` pixels take the border colour,
            // so a transparent fill leaves a hairline outline. Mixed
            // PREMULTIPLIED: straight colours of different alpha mix into
            // a fringe — a faint white border over an opaque dark fill
            // came out as a solid grey ring along the AA edge, whatever
            // the border's alpha said.
            if in.p1 > 0.0 {
                let bd = ui_color(in.border);
                let pm = mix(vec4<f32>(bd.rgb * bd.a, bd.a), vec4<f32>(col.rgb * col.a, col.a),
                             cov(d + in.p1));
                col = vec4<f32>(pm.rgb / max(pm.a, 1e-5), pm.a);
            }
            var alpha = col.a * cov(d);
            // Bottom-anchored scrim: opaque at the bottom edge, fading
            // out toward the top (the transport's gradient backing). The
            // ramp is gamma'd rather than linear — a straight ramp is
            // still ~70% transparent where the controls sit, which loses
            // them entirely against bright footage.
            // …and it SATURATES rather than ramping the whole height:
            // blending is linear-space on an sRGB target, so even 0.7
            // alpha only cuts perceived brightness by ~40%. The lower
            // part is fully opaque; only the lead-in above the controls
            // gradates.
            // pad 2 is the same ramp hung the other way up (opaque at
            // the top edge): the launch plate needs a ground under the
            // traffic lights as well as one under its message.
            if in.pad > 0.5 {
                let f = clamp(in.local.y / max(in.size.y, 1.0), 0.0, 1.0);
                let t = select(f, 1.0 - f, in.pad > 1.5);
                alpha = alpha * smoothstep(0.0, 0.5, t);
            }
            return vec4<f32>(col.rgb, alpha);
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
            let c = ui_color(in.color);
            return vec4<f32>(c.rgb, c.a * g);
        }
        case 8u: {
            // Exact binary texel selection: no purple interpolation at mask edges.
            let size = textureDimensions(tex_m);
            let p = clamp(vec2<i32>(in.uv * vec2<f32>(size)), vec2<i32>(0), vec2<i32>(size) - vec2<i32>(1));
            let painted = textureLoad(tex_m, p, 0).r > 0.5;
            return vec4<f32>(select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), painted), 0.5);
        }
        case 9u: {
            // Inset by the radius, then grow back by it: rounded corners
            // without changing the triangle's footprint.
            let r = in.p0;
            let w = in.size.x; let h = in.size.y;
            var p = in.local;
            if in.p1 > 0.5 { p.x = w - p.x; }
            let d = sd_triangle(p, vec2<f32>(r, r * 1.7), vec2<f32>(w - r * 1.2, h * 0.5),
                                vec2<f32>(r, h - r * 1.7)) - r;
            let c = ui_color(in.color);
            return vec4<f32>(c.rgb, c.a * cov(d));
        }
        case 7u: {
            // The logo texture is sRGB, so sampling already decoded it —
            // no ui_color here. Its alpha is straight (not premultiplied),
            // which is what ALPHA_BLENDING expects; color.a fades the
            // whole mark.
            return vec4<f32>(l.rgb, l.a * in.color.a);
        }
        case 10u: {
            // Already decoded by the sample (the plate texture is sRGB),
            // so no ui_color. color.a is the launch window's alpha: the
            // desktop still shows faintly through, as it did when this
            // window was a flat clear.
            return vec4<f32>(pl.rgb, pl.a * in.color.a);
        }
        case 11u: {
            // Lay the mark flat on the floor. The quad is hinged along
            // its TOP edge (the horizon) and tilted `p1` away from the
            // viewer, so inverting the pinhole projection turns a screen
            // row into a distance along the ground: near the horizon a
            // pixel covers a lot of mark, near the viewer very little,
            // which is the same hyperbola that makes the grid converge.
            // A straight vertical flip would be a mirror on glass; this
            // is light on a floor.
            let ct = cos(in.p1);
            let st = sin(in.p1);
            let persp = max(in.p0, 1.0);
            let src_h = max(in.pad, 1.0);
            let y_src = in.local.y * persp / max(persp * ct + in.local.y * st, 1e-3);
            // How much the plane widens at that distance, and the mark's
            // own width — the quad was sized as width * this at its far
            // end, so dividing recovers it without another attribute.
            let k = persp / max(persp - y_src * st, 1e-3);
            let src_w = in.size.x * max(persp - src_h * st, 1e-3) / persp;
            let x_src = (in.local.x - in.size.x * 0.5) / max(k, 1e-3) + src_w * 0.5;
            let inside = y_src <= src_h && x_src >= 0.0 && x_src <= src_w;
            // v runs backwards: the ground meets the mark at its baseline.
            let u = mix(in.border.x, in.border.z, clamp(x_src / src_w, 0.0, 1.0));
            let v = mix(in.border.w, in.border.y, clamp(y_src / src_h, 0.0, 1.0));
            // Explicit LOD, not a derivative: this is non-uniform control
            // flow, and the softening is wanted anyway — the reflection
            // blurs as it comes toward the viewer.
            let s = textureSampleLevel(tex_l, samp, vec2<f32>(u, v), clamp(log2(k) * 2.0 + 1.2, 0.0, 3.0));
            let f = clamp(in.local.y / max(in.size.y, 1.0), 0.0, 1.0);
            // Fades IN as well as out: at the horizon itself the
            // projection is barely foreshortened, so a full-strength
            // first row lands as a second copy of the tagline sitting
            // directly under the real one.
            let fade = (1.0 - smoothstep(0.1, 1.0, f)) * smoothstep(0.0, 0.16, f);
            // Broken into bands at the plate's own scanline pitch, so it
            // reads as a disturbed surface rather than a second logo.
            let band = select(0.35, 1.0, fract(in.local.y / 7.0) > 0.42);
            let keep = select(0.0, 1.0, inside);
            return vec4<f32>(s.rgb, s.a * in.color.a * fade * band * keep);
        }
        case 12u: {
            // A contact shadow for the standing mark: its own silhouette
            // taken from a coarse mip so the edge is soft, flattened to
            // black. The plate's lit horizon runs straight behind the
            // wordmark, which leaves the metal nothing to sit against —
            // the one surface where the brand book allows the lockup a
            // drop shadow, and it reads as contact with the floor.
            let sh = textureSampleLevel(tex_l, samp, in.uv, in.p0);
            return vec4<f32>(0.0, 0.0, 0.0, sh.a * in.color.a);
        }
        default: {
            return in.color;
        }
    }
}
