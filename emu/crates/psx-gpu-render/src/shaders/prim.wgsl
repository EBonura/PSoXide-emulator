// PSX hardware-renderer shader - VRAM-shaped target.
//
// The target is a `(1024 * S) × (512 * S)` texture (S = internal
// resolution multiplier). PSX vertex coords are in VRAM space
// (`pos.x ∈ 0..1024`, `pos.y ∈ 0..512`); the vertex shader maps them
// directly to NDC of that target. The wgpu viewport == target dims,
// so the rasterizer rasterises at S× density automatically - no
// per-S math anywhere in the shader.
//
// `HwVertex::flags` bit layout (must mirror `pipeline::flags`):
//   bits  0..=3   tpage_x_units   (× 64 = pixel x)
//   bit       4   tpage_y_index   (× 256 = pixel y; 0 or 256)
//   bits  5..=6   tex_depth        (0=4bpp, 1=8bpp, 2=15bpp)
//   bits  7..=12  clut_x_units    (× 16 = pixel x)
//   bits 13..=21  clut_y          (pixel y, 0..=511)
//   bit      22   TEXTURED
//   bit      23   RAW_TEXTURE     (skip tint modulate)
//   bit      24   SEMI_TRANS
//   bit      25   TEX_OPAQUE_PASS (discard STP texels)
//   bit      26   TEX_SEMI_PASS   (keep only STP texels)
//   bit      27   DITHER          (GP0(E1) bit 9 -- 4x4 ordered dither)
//
// `HwVertex::tex_window` packs GP0(E2) as four bytes:
//   bits  0..=7   mask_x in pixels
//   bits  8..=15  mask_y in pixels
//   bits 16..=23  offset_x in pixels
//   bits 24..=31  offset_y in pixels

const VRAM_W: u32 = 1024u;
const VRAM_H: u32 =  512u;
const VRAM_W_F: f32 = 1024.0;
const VRAM_H_F: f32 =  512.0;

@group(0) @binding(0) var vram: texture_2d<u32>;
// Texture filter in `.x`: 0 = nearest (PSX-native), 1 = bilinear. Global per
// frame; set by the toolbar toggle. `.y` holds the internal-resolution
// multiplier S, used to map a fragment back to its PSX-native pixel.
@group(0) @binding(1) var<uniform> u_texfilter: vec4<u32>;

struct VertexIn {
    @location(0) pos:   vec2<i32>,
    @location(1) color: vec4<f32>,
    @location(2) uv:    vec2<u32>,
    @location(3) flags: u32,
    @location(4) tex_window: u32,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0)       color:    vec4<f32>,
    // UV interpolates as f32 so the rasterizer gives a fresh sample
    // per fragment. WGSL can't interpolate integer types - passing
    // `vec2<u32>` here was a silent no-op (defaults to flat) and the
    // fragment got the provoking vertex's UV for every pixel.
    @location(1)       uv:       vec2<f32>,
    @location(2) @interpolate(flat) flags: u32,
    @location(3) @interpolate(flat) tex_window: u32,
}

// ---------------------------------------------------------------------------
// Fullscreen VRAM->target blit. Used by wireframe mode to rebuild the scaled
// persistent target from the (journal-clean) CPU VRAM every frame, so stale
// edges never accumulate in the target at any internal scale. One oversized
// triangle, no vertex buffer.
// ---------------------------------------------------------------------------

struct BlitOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_blit(@builtin(vertex_index) vi: u32) -> BlitOut {
    // (-1,-1), (3,-1), (-1,3): one triangle covering the whole target.
    let x = f32(i32((vi & 1u) * 4u) - 1);
    let y = f32(i32((vi >> 1u) * 4u) - 1);
    var out: BlitOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // NDC y points up, VRAM row 0 is the top row.
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@fragment
fn fs_blit(in: BlitOut) -> @location(0) vec4<f32> {
    let tx = min(u32(in.uv.x * 1024.0), 1023u);
    let ty = min(u32(in.uv.y * 512.0), 511u);
    let word = textureLoad(vram, vec2<u32>(tx, ty), 0).r;
    return vec4<f32>(bgr15_to_rgb(word), 1.0);
}

const FLAG_TEXTURED:    u32 = 1u << 22u;
const FLAG_RAW_TEXTURE: u32 = 1u << 23u;
const FLAG_TEX_OPAQUE_PASS: u32 = 1u << 25u;
const FLAG_TEX_SEMI_PASS:   u32 = 1u << 26u;
const FLAG_DITHER:      u32 = 1u << 27u;

// PSX-SPX signed 4x4 ordered-dither offsets, indexed by
// `(y & 3) * 4 + (x & 3)` -- byte-for-byte the CPU rasterizer's
// `DITHER_OFFSETS` (emulator-core gpu/blend.rs).
const DITHER_OFFSETS = array<i32, 16>(
    -4, 0, -3, 1,
     2, -2, 3, -1,
    -3, 1, -4, 0,
     3, -1, 2, -2,
);

// Apply the ordered dither and truncate to 15bpp, then re-expand to
// display codes the way `bgr15_to_rgb` does. `rgb` is in display-code
// space (0..1 == 0..255); `frag` is the fragment's target-space
// position, converted here to the PSX-native pixel the CPU indexes
// the matrix by. Mirrors `dither_rgb`: add the signed offset to the
// 8-bit channel, clamp to 0..=255, keep the high 5 bits.
fn dither_to_bgr15(rgb: vec3<f32>, frag: vec2<f32>) -> vec3<f32> {
    let scale = f32(max(u_texfilter.y, 1u));
    let px = i32(floor(frag.x / scale));
    let py = i32(floor(frag.y / scale));
    let off = DITHER_OFFSETS[u32(py & 3) * 4u + u32(px & 3)];
    var out: vec3<f32>;
    for (var i = 0; i < 3; i++) {
        let c8 = i32(round(rgb[i] * 255.0));
        let c5 = u32(clamp(c8 + off, 0, 255) >> 3u);
        out[i] = f32((c5 << 3u) | (c5 >> 2u)) / 255.0;
    }
    return out;
}

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    // PSX-VRAM-space (0..1024, 0..512) → NDC (-1..+1, Y-flipped).
    let pos_psx = vec2<f32>(f32(in.pos.x), f32(in.pos.y));
    let ndc_xy = (pos_psx / vec2<f32>(VRAM_W_F, VRAM_H_F)) * 2.0 - 1.0;
    var out: VertexOut;
    out.position = vec4<f32>(ndc_xy.x, -ndc_xy.y, 0.0, 1.0);
    out.color    = in.color;
    out.uv       = vec2<f32>(f32(in.uv.x), f32(in.uv.y));
    out.flags    = in.flags;
    out.tex_window = in.tex_window;
    return out;
}

// PSX U/V are 8-bit per axis (so wrap on >255). Floor before the
// wrap matches the PSX nearest-neighbour rasterizer the compute
// backend already replicates pixel-for-pixel.
fn page_uv(uv: vec2<f32>) -> vec2<u32> {
    let ix = u32(max(uv.x, 0.0));
    let iy = u32(max(uv.y, 0.0));
    return vec2<u32>(ix & 0xFFu, iy & 0xFFu);
}

fn apply_tex_window(uv8: vec2<u32>, tex_window: u32) -> vec2<u32> {
    let mask_x = tex_window & 0xFFu;
    let mask_y = (tex_window >> 8u) & 0xFFu;
    let off_x = (tex_window >> 16u) & 0xFFu;
    let off_y = (tex_window >> 24u) & 0xFFu;
    return vec2<u32>(
        (uv8.x & (~mask_x & 0xFFu)) | (off_x & mask_x),
        (uv8.y & (~mask_y & 0xFFu)) | (off_y & mask_y),
    );
}

fn tpage_origin(flags: u32) -> vec2<u32> {
    let tx = (flags & 0xFu) * 64u;
    let ty = ((flags >> 4u) & 1u) * 256u;
    return vec2<u32>(tx, ty);
}

fn clut_origin(flags: u32) -> vec2<u32> {
    let cx = ((flags >> 7u) & 0x3Fu) * 16u;
    let cy = (flags >> 13u) & 0x1FFu;
    return vec2<u32>(cx, cy);
}

fn tex_depth(flags: u32) -> u32 {
    return (flags >> 5u) & 0x3u;
}

fn vram_load(x: u32, y: u32) -> u32 {
    let xx = x & (VRAM_W - 1u);
    let yy = y & (VRAM_H - 1u);
    return textureLoad(vram, vec2<i32>(i32(xx), i32(yy)), 0).r;
}

// BGR15 → display-code 0..1 RGB. Bit-replicates 5→8 the same way
// `Vram::to_rgba8` does on the CPU side, so colour quantisation
// matches the existing reference. This is intentionally NOT linear:
// PSX tint/modulate math happens in integer display-code space.
fn bgr15_to_rgb(word: u32) -> vec3<f32> {
    let r5 = word & 0x1Fu;
    let g5 = (word >> 5u) & 0x1Fu;
    let b5 = (word >> 10u) & 0x1Fu;
    let r8 = (r5 << 3u) | (r5 >> 2u);
    let g8 = (g5 << 3u) | (g5 >> 2u);
    let b8 = (b5 << 3u) | (b5 >> 2u);
    return vec3<f32>(f32(r8), f32(g8), f32(b8)) / 255.0;
}

fn srgb_channel_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn srgb_to_linear(rgb: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        srgb_channel_to_linear(rgb.r),
        srgb_channel_to_linear(rgb.g),
        srgb_channel_to_linear(rgb.b),
    );
}

// Sample the active texture page at PSX UV. Returns the raw 16-bit
// BGR15 texel; texel == 0 is reserved for transparency (caller
// discards).
fn sample_texel(flags: u32, uv8: vec2<u32>) -> u32 {
    let tp = tpage_origin(flags);
    let depth = tex_depth(flags);
    if depth == 0u {
        // 4 bpp: 4 indices per VRAM word, picked by uv.x % 4.
        let word = vram_load(tp.x + (uv8.x >> 2u), tp.y + uv8.y);
        let nibble = (word >> ((uv8.x & 3u) * 4u)) & 0xFu;
        let cl = clut_origin(flags);
        return vram_load(cl.x + nibble, cl.y);
    } else if depth == 1u {
        // 8 bpp: 2 indices per VRAM word, picked by uv.x & 1.
        let word = vram_load(tp.x + (uv8.x >> 1u), tp.y + uv8.y);
        let byte = (word >> ((uv8.x & 1u) * 8u)) & 0xFFu;
        let cl = clut_origin(flags);
        return vram_load(cl.x + byte, cl.y);
    }
    // 15 bpp direct.
    return vram_load(tp.x + uv8.x, tp.y + uv8.y);
}

// PSX modulate, in the CPU rasterizer's integer semantics
// (`modulate_tint_dithered`, emulator-core gpu/blend.rs):
//
//     c8 = min(tint8 * (t5 << 3) / 0x80, 255)     // truncating integer divide
//
// The float form this replaced (`tex * tint * 2`) is `tex * tint * 2/255`, and
// 2/255 != 1/128, so it ran ~0.4% bright -- up to 2/255 off, which is enough to
// flip a channel across a dither bucket boundary.
//
// `texel_rgb` arrives bit-replicated 5->8 (`bgr15_to_rgb`), but the CPU
// modulates `t5 << 3`. `t8 - floor(t8 / 32) == t5 << 3` for every 5-bit code,
// so undo the replication rather than re-quantising to 5 bits (which would
// band the filtered-texture paths).
//
// `RAW_TEXTURE` skips all of this and returns the texel verbatim.
fn modulate(texel_rgb: vec3<f32>, tint_rgba: vec4<f32>, raw: bool) -> vec3<f32> {
    if raw {
        return texel_rgb;
    }
    let t8 = round(texel_rgb * 255.0);
    let tex = t8 - floor(t8 / 32.0);
    let tint = round(tint_rgba.rgb * 255.0);
    return min(floor(tint * tex / 128.0), vec3<f32>(255.0)) / 255.0;
}

// One bilinear corner: premultiplied colour, coverage 0 on transparent texels
// (DuckStation "no edge blending" / binary alpha -- transparent neighbours
// never bleed colour into an opaque edge).
fn tex_corner(flags: u32, tex_window: u32, uvf: vec2<f32>) -> vec4<f32> {
    let uv8 = apply_tex_window(page_uv(uvf), tex_window);
    let w = sample_texel(flags, uv8);
    if w == 0u {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    return vec4<f32>(bgr15_to_rgb(w), 1.0);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let textured = (in.flags & FLAG_TEXTURED) != 0u;
    let dither = (in.flags & FLAG_DITHER) != 0u;
    if !textured {
        var rgb = in.color.rgb;
        if dither {
            rgb = dither_to_bgr15(rgb, in.position.xy);
        }
        return vec4<f32>(srgb_to_linear(rgb), in.color.a);
    }
    let uv8 = apply_tex_window(page_uv(in.uv), in.tex_window);
    let texel = sample_texel(in.flags, uv8);
    if texel == 0u {
        discard;
    }
    let stp = (texel & 0x8000u) != 0u;
    if ((in.flags & FLAG_TEX_OPAQUE_PASS) != 0u) && stp {
        discard;
    }
    if ((in.flags & FLAG_TEX_SEMI_PASS) != 0u) && !stp {
        discard;
    }
    // Bilinear (u_texfilter.x == 1) with binary-alpha edge handling: blend the
    // 2x2 texel colours premultiplied so transparent neighbours never bleed;
    // the silhouette/STP stay on the nearest (center) texel above. Seam-free
    // filtering isn't possible on PSX's packed VRAM (even DuckStation leaves
    // texture-boundary seams), so this matches the established emulator result.
    var tex_rgb = bgr15_to_rgb(texel);
    if u_texfilter.x == 1u {
        let uvf = in.uv - vec2<f32>(0.5, 0.5);
        let base = floor(uvf);
        let fr = uvf - base;
        let c00 = tex_corner(in.flags, in.tex_window, base);
        let c10 = tex_corner(in.flags, in.tex_window, base + vec2<f32>(1.0, 0.0));
        let c01 = tex_corner(in.flags, in.tex_window, base + vec2<f32>(0.0, 1.0));
        let c11 = tex_corner(in.flags, in.tex_window, base + vec2<f32>(1.0, 1.0));
        let acc = mix(mix(c00, c10, fr.x), mix(c01, c11, fr.x), fr.y);
        if acc.a > 0.0039 {
            tex_rgb = acc.rgb / acc.a;
        }
    }
    let raw = (in.flags & FLAG_RAW_TEXTURE) != 0u;
    var rgb = modulate(tex_rgb, in.color, raw);
    // Raw-texture primitives bypass the modulator on silicon and are
    // never dithered; the translator already withholds FLAG_DITHER
    // for them, so `raw` needs no second check here.
    if dither {
        rgb = dither_to_bgr15(rgb, in.position.xy);
    }
    return vec4<f32>(srgb_to_linear(rgb), 1.0);
}
