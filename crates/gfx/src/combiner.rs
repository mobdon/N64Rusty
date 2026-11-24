//! The colour combiner, `(A - B) * C + D` per channel group per cycle, and
//! WGSL generated from its settings.

use std::fmt::Write;

/// Decoded `SETCOMBINE` settings: for each of the two cycles, the colour
/// inputs `[a, b, c, d]` and the alpha inputs `[a, b, c, d]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Combiner {
    pub color: [[u8; 4]; 2],
    pub alpha: [[u8; 4]; 2],
}

// Field positions in the 56 bits of a SETCOMBINE command, as
// (cycle, slot) -> (shift, width). Shifts count from bit 0 of the u64
// `(w0 & 0xFFFFFF) << 32 | w1`.
const COLOR: [[(u32, u32); 4]; 2] = [
    [(52, 4), (28, 4), (47, 5), (15, 3)],
    [(37, 4), (24, 4), (32, 5), (6, 3)],
];
const ALPHA: [[(u32, u32); 4]; 2] = [
    [(44, 3), (12, 3), (41, 3), (9, 3)],
    [(21, 3), (3, 3), (18, 3), (0, 3)],
];

impl Combiner {
    pub fn from_raw(raw: u64) -> Self {
        let f = |(sh, w): (u32, u32)| ((raw >> sh) & ((1 << w) - 1)) as u8;
        Self {
            color: COLOR.map(|c| c.map(f)),
            alpha: ALPHA.map(|c| c.map(f)),
        }
    }

    pub const fn to_raw(self) -> u64 {
        let mut raw = 0u64;
        let mut cyc = 0;
        while cyc < 2 {
            let mut slot = 0;
            while slot < 4 {
                let (sh, w) = COLOR[cyc][slot];
                raw |= (self.color[cyc][slot] as u64 & ((1 << w) - 1)) << sh;
                let (sh, w) = ALPHA[cyc][slot];
                raw |= (self.alpha[cyc][slot] as u64 & ((1 << w) - 1)) << sh;
                slot += 1;
            }
            cyc += 1;
        }
        raw
    }

    /// The same settings in both cycles.
    pub const fn both(color: [u8; 4], alpha: [u8; 4]) -> Self {
        Self {
            color: [color, color],
            alpha: [alpha, alpha],
        }
    }
}

// Input codes used by the built-in combiners below.
const PRIM: u8 = 3;
const TEXEL0: u8 = 1;
const ZERO_A: u8 = 15;
const ZERO_C: u8 = 31;
const ZERO_D: u8 = 7;

/// Combiner settings that output one input unchanged.
const fn pass(input: u8) -> u64 {
    Combiner::both(
        [ZERO_A, ZERO_A, ZERO_C, input],
        [ZERO_D, ZERO_D, ZERO_D, input],
    )
    .to_raw()
}

/// Outputs the primitive colour; used for fill rectangles.
pub(crate) const PRIM_ONLY: u64 = pass(PRIM);
/// Outputs texel 0; used for copy-mode texture rectangles.
pub(crate) const TEXEL0_ONLY: u64 = pass(TEXEL0);

/// Everything that changes the generated fragment shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShaderKey {
    pub combine: u64,
    pub two_cycle: bool,
    /// 0 none, 1 against the blend colour's alpha, 2 at one half.
    pub alpha_test: u8,
    pub flat: bool,
}

fn color_input(slot: usize, code: u8) -> &'static str {
    match (slot, code) {
        (_, 0) => "comb.rgb",
        (_, 1) => "t0.rgb",
        (_, 2) => "t1.rgb",
        (_, 3) => "u.prim.rgb",
        (_, 4) => "shade.rgb",
        (_, 5) => "u.env.rgb",
        (0, 6) | (3, 6) => "vec3f(1.0)",
        (0, 7) => "vec3f(noise)",
        (2, 7) => "vec3f(comb.a)",
        (2, 8) => "vec3f(t0.a)",
        (2, 9) => "vec3f(t1.a)",
        (2, 10) => "vec3f(u.prim.a)",
        (2, 11) => "vec3f(shade.a)",
        (2, 12) => "vec3f(u.env.a)",
        (2, 14) => "vec3f(u.misc.x)",
        // Chroma key, YUV conversion and LOD fraction: zero.
        // shortcut: LOD fraction is zero (no mipmapping); add it with mipmaps.
        _ => "vec3f(0.0)",
    }
}

fn alpha_input(slot: usize, code: u8) -> &'static str {
    match (slot, code) {
        (2, 0) => "0.0", // LOD fraction
        (_, 0) => "comb.a",
        (_, 1) => "t0.a",
        (_, 2) => "t1.a",
        (_, 3) => "u.prim.a",
        (_, 4) => "shade.a",
        (_, 5) => "u.env.a",
        (2, 6) => "u.misc.x",
        (_, 6) => "1.0",
        _ => "0.0",
    }
}

/// WGSL for one combiner setting: vertex stage `vs`, fragment stage `fs`.
///
/// One-cycle mode uses the second cycle's settings, as the hardware does;
/// two-cycle mode runs the first cycle, then the second with its output as
/// `COMBINED`.
pub fn fragment_shader(combine: u64, two_cycle: bool) -> String {
    shader(ShaderKey {
        combine,
        two_cycle,
        alpha_test: 0,
        flat: false,
    })
}

pub(crate) fn shader(key: ShaderKey) -> String {
    let c = Combiner::from_raw(key.combine);
    let interp = if key.flat { "@interpolate(flat)" } else { "" };
    let mut s = format!(
        "struct U {{ prim: vec4f, env: vec4f, blend: vec4f, misc: vec4f }}
@group(0) @binding(0) var<uniform> u: U;
@group(1) @binding(0) var tex0: texture_2d<f32>;
@group(1) @binding(1) var smp0: sampler;
@group(1) @binding(2) var tex1: texture_2d<f32>;
@group(1) @binding(3) var smp1: sampler;

struct VOut {{
    @builtin(position) pos: vec4f,
    @location(0) {interp} color: vec4f,
    @location(1) uv0: vec2f,
    @location(2) uv1: vec2f,
}}

@vertex
fn vs(@location(0) pos: vec4f, @location(1) color: vec4f, @location(2) uv0: vec2f, @location(3) uv1: vec2f) -> VOut {{
    return VOut(pos, color, uv0, uv1);
}}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {{
    let shade = v.color;
    let t0 = textureSample(tex0, smp0, v.uv0);
    let t1 = textureSample(tex1, smp1, v.uv1);
    let noise = fract(sin(dot(v.pos.xy, vec2f(12.9898, 78.233))) * 43758.5453);
    var comb = vec4f(0.0);
"
    );
    let cycles: &[usize] = if key.two_cycle { &[0, 1] } else { &[1] };
    for &cyc in cycles {
        let [a, b, cc, d] = [0, 1, 2, 3].map(|k| color_input(k, c.color[cyc][k]));
        let [aa, ab, ac, ad] = [0, 1, 2, 3].map(|k| alpha_input(k, c.alpha[cyc][k]));
        let _ = writeln!(
            s,
            "    comb = clamp(vec4f(({a} - {b}) * {cc} + {d}, ({aa} - {ab}) * {ac} + {ad}), vec4f(0.0), vec4f(1.0));"
        );
    }
    match key.alpha_test {
        1 => s.push_str("    if (comb.a < u.blend.a) { discard; }\n"),
        2 => s.push_str("    if (comb.a < 0.5) { discard; }\n"),
        _ => {}
    }
    s.push_str("    return comb;\n}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_round_trip() {
        let c = Combiner {
            color: [[1, 2, 3, 4], [5, 6, 7, 0]],
            alpha: [[1, 2, 3, 4], [5, 6, 7, 0]],
        };
        assert_eq!(Combiner::from_raw(c.to_raw()), c);
        assert_eq!(c.to_raw() >> 56, 0);
    }

    #[test]
    fn builtins_pass_one_input() {
        let p = Combiner::from_raw(PRIM_ONLY);
        assert_eq!(p.color[1], [15, 15, 31, 3]);
        assert_eq!(p.alpha[1], [7, 7, 7, 3]);
        assert!(shader(ShaderKey {
            combine: PRIM_ONLY,
            two_cycle: false,
            alpha_test: 0,
            flat: false
        })
        .contains("(vec3f(0.0) - vec3f(0.0)) * vec3f(0.0) + u.prim.rgb"));
    }

    #[test]
    fn modulate_texture_by_shade() {
        // (TEXEL0 - 0) * SHADE + 0, alpha the same.
        let raw = Combiner::both([1, 15, 4, 7], [1, 7, 4, 7]).to_raw();
        let s = fragment_shader(raw, false);
        assert!(s.contains("(t0.rgb - vec3f(0.0)) * shade.rgb + vec3f(0.0)"));
        assert!(s.contains("(t0.a - 0.0) * shade.a + 0.0"));
    }
}
