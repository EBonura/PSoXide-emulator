# Texture filter specification: Edge

This document specifies the Edge texture filter of the hardware renderer. It
was written first, from the mathematics below and nothing else, and
`prim.wgsl` is an implementation of it. No shader, filter or scaler source
from any other project was read for this work (see `PROVENANCE.md`). The
filter was first drafted together with a second resampler (a clamped
Catmull-Rom cubic) and a plain bilinear mode; only None and Edge ship, and the
section numbers below keep their original values so that references in the
shader stay valid. Section 4 is therefore empty.

## 1. Where the filter runs

The hardware renderer draws every textured PS1 primitive at an internal
resolution of S times the native one. For each fragment the pixel shader
receives an interpolated texture coordinate `uv` (in texel units, the same
space as the 8-bit U/V of the GPU) and fetches texels from the VRAM texture.
The toggle `u_texfilter.x` selects how:

| value | name | taps | description |
|---|---|---|---|
| 0 | None | 1 | the texel containing `uv` (PS1-native point sampling) |
| 1 | Edge | 8 | edge-directed interpolation (section 5) |

None is not touched. Everything below applies only to value 1.

## 2. Taps and what a tap means

The filter reads texel colours around the sample position. Let

    p = uv - (0.5, 0.5)        (texel centres sit at integer + 0.5)
    b = floor(p)               (integer coordinates of the top-left tap)
    f = p - b                  (fractional position inside the cell, in [0,1))

The four texels around the sample are named by their offset from `b`:

    A = T(0,0)   B = T(1,0)
    C = T(0,1)   D = T(1,1)

and `f = (fx, fy)` says where inside the A-B-C-D cell the sample falls. A tap
`T(i,j)` is the colour of texel `b + (i,j)`, fetched exactly the way the
nearest-texel path fetches one:

1. the coordinate is truncated and wrapped to 8 bits (negative coordinates
   clamp to 0 first);
2. the texture window (GP0 E2) is applied to the 8-bit coordinate;
3. the texel is read through the page's colour depth, so 4 and 8 bit pages go
   through the CLUT and 15 bit pages are read directly;
4. the 16-bit word becomes a display-code RGB triple in [0,1] by the existing
   5-to-8 bit replication.

Because the window and the CLUT are applied per tap, a filter never blends
across a texture-window wrap or invents a colour that is not in the palette
neighbourhood the game actually sampled.

**Transparency.** A texel word of exactly zero is transparent. The primitive's
silhouette, and the semi-transparency (STP) pass tests, are decided by the
nearest texel alone, before the filter runs. So when
the filter runs, the nearest texel `N` is known to be opaque. A transparent tap
must contribute no colour of its own, so it is replaced before filtering:

1. An inner tap (A, B, C or D) that is transparent takes the colour of its
   neighbour in the same row of the cell if that is opaque, else the
   neighbour in the same column, else `N`. Along a cut-out's border this
   continues the border colour outwards (the usual "dilated edge" of a
   filtered sprite) instead of mixing in whatever lies beyond it.
2. An outer tap that is transparent takes the resolved inner tap obtained by
   clamping its own offsets into the cell (`i` and `j` into 0..1).

A transparent neighbour thus leaves no colour of its own in the visible edge
(binary alpha), and every weight sum stays equal to 1. The same rule keeps a
texture's own border clean when the quad is drawn next to unrelated VRAM
contents, which are as likely to be transparent as not.

All blending happens on the display-code values (the gamma-encoded 5-to-8 bit
codes, not linear light). The modulate step that follows (tint multiply,
dither) is shared and unchanged.

## 3. Colour distance

The Edge filter needs a number for "how different are these two texels":

    d(p, q) = ( |pr - qr| + |pg - qg| + |pb - qb| ) / 3

in display-code units, so d is in [0,1]. One step of one 5-bit channel is
about 0.010. It is a plain L1 distance; the filter only needs a monotone,
cheap measure, and L1 has no square root.

Two helpers are used below.

    smoothstep(lo, hi, x)  = t*t*(3 - 2t),  t = clamp((x - lo) / (hi - lo), 0, 1)

    sharpen(f, g)          = mix(f, smoothstep(0.3, 0.7, f), g)

`sharpen` pushes an interpolation fraction towards 0 or 1 so a transition
becomes narrower while keeping `f = 0`, `0.5` and `1` fixed. `g` in [0,1] says
how much to push. Because 0.3 + 0.7 = 1, `sharpen(1 - f, g) = 1 - sharpen(f, g)`:
the result does not depend on which side an edge is walked from.

    edge_mix(a, b, f)  = mix(a, b, sharpen(f, smoothstep(0.08, 0.25, d(a, b))))

`edge_mix` is a linear blend whose transition tightens as the two colours
get more different. Nearly equal texels (a gradient) blend linearly; very
different texels (a hard edge) switch over in a band about 0.4 texel wide.
Properties used later: `edge_mix(a, b, f) = edge_mix(b, a, 1 - f)`, and it
returns `a` at `f = 0` and `b` at `f = 1`.

## 4. (not used)

The Catmull-Rom resampler drafted here was dropped before release.

## 5. Edge: edge-directed interpolation

### 5.1 Goal

Smooth diagonal staircases in magnified pixel art without blurring horizontal
or vertical edges, flat regions or dither patterns, and without ever
interpolating across an edge in a way that mixes the two sides into a muddy
band. Colours on one side of an edge are only blended with colours on that
side, or switched over in a narrow band.

### 5.2 The cell and the two diagonals

Use the cell `A B / C D` and `f` of section 2, plus the four outer corner
taps of the 4x4 block:

    Z = T(-1,-1)    Y = T(2,-1)
    V = T(-1, 2)    W = T( 2, 2)

The cell has two diagonals. The main diagonal runs A to D and extends outward
through Z and W. The anti diagonal runs B to C and extends outward through Y
and V. A diagonal edge in the image runs along one of them, and the texels on
that line are alike; the cell's other corners lie on the sides of the edge.

### 5.3 Deciding the orientation

Score how smooth each diagonal chain is. A low cost means "this chain is a
line of nearly constant colour":

    Cm = 2 d(A, D) + d(Z, A) + d(D, W)          (main chain  Z A D W)
    Ca = 2 d(B, C) + d(Y, B) + d(C, V)          (anti chain  Y B C V)

The middle pair counts double because the sample lies between those two
texels. The edge runs along the smoother chain. Define

    r  = |Cm - Ca| / (Cm + Ca + 0.0001)                    in [0, 1]
    o  = smoothstep(0.35, 0.8, r) * smoothstep(0.1, 0.3, max(Cm, Ca))

`r` is 0 when both chains are equally smooth (flat area, orthogonal edge,
checkerboard dither) and approaches 1 when exactly one is. The second factor
suppresses the choice where the chains differ only by noise: both costs have to
be a visible fraction of the colour range before orientation is trusted. `o`
is the weight given to the diagonal rendering of section 5.5. Where `o` is 0
the pixel is rendered by the axis rendering of section 5.4.

### 5.4 Axis rendering

Interpolate the way bilinear would (rows, then the column), with each blend
being an `edge_mix`:

    top    = edge_mix(A, B, fx)
    bottom = edge_mix(C, D, fx)
    axis   = edge_mix(top, bottom, fy)

Smooth gradients stay linear. A horizontal or vertical hard edge turns into a
crisp step with a transition about 0.4 texel wide, centred half way between the
texel centres, instead of bilinear's full-texel ramp. A texel is only blended
with its neighbour in proportion to how alike they are.

### 5.5 Diagonal rendering

Split the cell into two triangles along the line of the smoother chain, and
interpolate inside the triangle that contains the sample. Each triangle has a
base on the line and an apex on one side of it. For a sample inside a triangle,
let `wP` be the barycentric weight of the apex P, and let `q` be where the
sample projects onto the line: `L0` and `L1` are the line's end colours, and

    L(q) = mix(L0, L1, q)
    diag = edge_mix(L(q), P, wP)

Using `edge_mix` on the apex weight narrows the transition between the line and
the apex the same way the axis rendering does, so a diagonal edge between two
flat regions comes out crisp, positioned half way between them.

For the main chain (line A-D, apex B or C), with `s = fx - fy`:

    s >= 0:  apex B,  wP = s,      q = fy / (1 - s)     (L0 = A, L1 = D)
    s <  0:  apex C,  wP = -s,     q = fx / (1 + s)     (L0 = A, L1 = D)

For the anti chain (line B-C, apex A or D), with `u = fx + fy`:

    u <  1:  apex A,  wP = 1 - u,  q = fy / u           (L0 = B, L1 = C)
    u >= 1:  apex D,  wP = u - 1,  q = (1 - fx) / (2 - u)   (L0 = B, L1 = C)

If a denominator is zero the sample is on an apex corner (`wP = 1`) and the
result is that corner. These are the standard barycentric coordinates of a
point in a triangle of the unit square; the projections follow from solving
`point = wP * apex + (1 - wP) * line_point` for the line point.

Which chain, and so which formula, is picked by the sign of `Cm - Ca`: the
main formulas when `Cm < Ca`, the anti formulas otherwise.

Worked example. A 45 degree staircase, light above-left and dark below-right,
gives cells with three light texels and one dark one, say `A = B = C =` light,
`D =` dark. Then `Cm` is large (it contains d(A, D)) and `Ca` is 0, so the anti
chain is the line, the apex on the D side differs from the line colour, and for
the part of the cell nearer D the result is a crisp light-to-dark switch along
a straight diagonal half way between D and the line B-C. Neighbouring cells
continue the same straight line, so the stairs become one clean diagonal.

### 5.6 Isolated pixels

A lone bright texel in a dark field also looks like "three alike and one
different" inside one cell. Rendering it diagonally would cut a corner off
every cell around it and shrink it to a diamond of half its area. The outer
tap tells the two cases apart: in a real edge the apex colour continues
beyond the apex; in a lone pixel it does not.

For both apexes P of the chosen line, take the apex's
far tap F (the outer corner tap on the same diagonal through P: Z for A, W for
D, Y for B, V for C) and the line colour `Lc = (L0 + L1) / 2`:

    u_P = d(F, P) / ( d(F, P) + d(F, Lc) + 0.0001 )
    rel = smoothstep(0.04, 0.12, d(P, Lc))
    sup = mix(1, 1 - smoothstep(0.35, 0.65, u_P), rel)

`u_P` is near 0 when F resembles P (the apex continues) and near 1 when F
resembles the line colour (P is isolated). `rel` makes the test irrelevant when
the apex is hardly different from the line at all. The cell's support is
`min(sup of both apexes)`, and the diagonal weight becomes `o * support`.

### 5.7 Result

    edge = mix(axis, diag, o * support)

The weights `o` and `support` are constant over a cell, and `axis` and `diag`
are both defined from the same `edge_mix` of the same two texels on every cell
side (on side AB, `fy = 0`, both renderings reduce to `edge_mix(A, B, fx)`; the
same holds for the other three sides). So the filter output is continuous
across cell boundaries even though the per-cell weights change. At a texel
centre it returns the texel.

Flat regions: all taps equal, every distance is 0, `o = 0`, the result is the
texel. Hard orthogonal pixel-art edges: the two chains cost the same, `o = 0`,
the axis rendering gives a crisp step. Checkerboard dither: same, `o = 0`, and
since `d` between neighbours is large the `edge_mix` switches over narrowly so
the pattern stays crisp.

## 6. What is deliberately not done

- The filter does not know the on-screen size of a texel (the shader does not
  receive screen-space derivatives of `uv`). The transition width constants in
  `sharpen` are fixed fractions of a texel. At low magnification the result
  approaches the nearest-texel look, at high magnification the transitions are
  wider in screen pixels but still much narrower than a plain linear ramp.
- The filter does not look across texture pages: a tap just reads VRAM at the
  wrapped, windowed coordinate. As with any sample-time filter, a texture packed against
  another one in VRAM shows seams at its boundary where a tap lands in the
  neighbour. Fixing that is not possible without knowing the texture's extent.
- The semi-transparency, dither and tint stages after the filter are shared
  with the None mode and are not changed.

## 7. Verification

The tests live in `emu/crates/psx-gpu-render/src/lib.rs` and run the real
shader headless on synthetic VRAM:

- a flat texture is unchanged;
- transparent texels leave no colour in the silhouette (the opaque pixels stay
  exactly the texel colour, and the silhouette area does not change);
- Edge keeps an orthogonal step orthogonal (all rows identical) and narrow;
- Edge turns a 45 degree staircase into a straight diagonal (the colour is
  constant along each `x + y = k` line) and leaves flat areas as they were;
- Edge keeps an isolated texel's area;
- a 4-bit CLUT texture drawn 1:1 comes out identical to None, since
  every fragment then samples a texel centre;
- a texture window that repeats the first half of a texture never reads the
  second half through any tap.

Two ignored tests produce the review material: `texture_filter_cost` times
None and Edge on stacked full-screen quads, and `texture_filter_sheet` writes
both renderings of a synthetic pixel-art texture. `--dump-hw` with
`--texture-filter all` writes one frame per filter, and frames with None
selected are byte-identical to the frames from the build before this change.
