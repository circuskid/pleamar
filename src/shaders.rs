//! A scene's own shaders: `shader aurora = file "aurora.wgsl"`.
//!
//! This is the door left open for what the language did not foresee. Whoever
//! writes one writes ONE function in WGSL, `fn shade(s: Shader) -> vec4<f32>`:
//! a colour for each point of its box. Everything else is pleamar's: where the
//! box is, its rounded corners, its opacity, clipping, being painted only when
//! something changed. What it can read comes in `s` —where it is, how big, the
//! time, the pointer, eight numbers and two colours from the scene— and, if it
//! asks for it, what is behind the surface (`behind`, `behind_frosted`), which is
//! what no QtQuick `ShaderEffect` can have.
//!
//! It is read and checked when the scene is read, with naga —the same compiler
//! wgpu uses—, so a mistake comes out like any other in the scene, with its file
//! and its line, and never as a crash while painting. It cannot declare
//! bindings, entry points or global variables: all it can touch is what it is
//! given. Its names are prefixed on the way in (`u3_shade`), so two shaders can
//! each have their own `wave` without stepping on each other or on pleamar's.

use std::collections::HashSet;
use std::path::Path;

/// What a shader receives. The same text is the prelude used to check it and the
/// one that goes into the real shader, so the two cannot disagree.
pub const PRELUDE: &str = "struct Shader {
    pos: vec2<f32>,
    size: vec2<f32>,
    uv: vec2<f32>,
    time: f32,
    scale: f32,
    pointer: vec2<f32>,
    hovered: f32,
    a: vec4<f32>,
    b: vec4<f32>,
    color: vec4<f32>,
    color2: vec4<f32>,
    origin: vec2<f32>,
    layer: i32,
};
";

/// While checking, `behind` exists but reads nothing: what matters is that it is
/// called correctly.
const CHECK_HELPERS: &str = "fn behind(s: Shader, at: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
fn behind_frosted(s: Shader, at: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
fn inside(s: Shader, at: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
";

/// The real ones: what is behind, unmixed by the lens —straight colour, and in
/// the alpha how much of it is known (0 under something we painted solid)—.
const REAL_HELPERS: &str = "fn shader_behind(s: Shader, at: vec2<f32>, frosted: bool) -> vec4<f32> {
    if (u.backdrop.x < 0.5) { return vec4<f32>(0.0); }
    let size = u.header.xy * u.header.w;
    let q = (s.origin + at - u.hud.xw) * u.header.w / size;
    var v = textureSampleLevel(backdrop_sharp, backdrop_sampler, q, 0.0);
    if (frosted) { v = textureSampleLevel(backdrop_blurred, backdrop_sampler, q, 0.0); }
    return vec4<f32>(v.rgb / max(v.a, 0.001), v.a);
}
fn behind(s: Shader, at: vec2<f32>) -> vec4<f32> { return shader_behind(s, at, false); }
fn behind_frosted(s: Shader, at: vec2<f32>) -> vec4<f32> { return shader_behind(s, at, true); }
// What a group holds, for a group's shader (`group { shader: rain }`): straight
// colour, and in the alpha how much of it there is. Outside a group, nothing.
fn inside(s: Shader, at: vec2<f32>) -> vec4<f32> {
    if (s.layer < 0) { return vec4<f32>(0.0); }
    let dims = vec2<f32>(textureDimensions(layers));
    let q = (s.origin + at - u.hud.xw) * u.header.w / dims;
    let v = textureSampleLevel(layers, atlas_sampler, q, s.layer, 0.0);
    return vec4<f32>(v.rgb / max(v.a, 0.001), v.a);
}
";

/// A shader already read and checked.
#[derive(Clone, Debug)]
pub struct UserShader {
    /// Its code with its names prefixed, ready to go into the real shader.
    pub code: String,
    /// It reads `s.time`: while it is on screen, frames keep coming.
    pub animated: bool,
    /// It reads `s.pointer`/`s.hovered`: it is painted again when the mouse moves.
    pub pointer: bool,
    /// It calls `behind`: the surface captures what is behind its box.
    pub behind: bool,
}

/// Reads and checks a shader. The error comes with the line inside the file,
/// already written for whoever wrote it.
/// `shown` is how the scene wrote its path: that is what the error says.
pub fn load(path: &Path, shown: &str, index: usize) -> Result<UserShader, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{shown} cannot be read: {e}"))?;
    let module = check(&source, shown)?;
    // Its own names, to prefix them. Taken from what naga read, not guessed;
    // the prelude's are pleamar's.
    let given = ["Shader", "behind", "behind_frosted", "inside"];
    let mut own: HashSet<String> = HashSet::new();
    own.extend(module.functions.iter().filter_map(|(_, f)| f.name.clone()));
    own.extend(module.constants.iter().filter_map(|(_, c)| c.name.clone()));
    own.extend(module.types.iter().filter_map(|(_, t)| t.name.clone()));
    own.retain(|n| !given.contains(&n.as_str()));
    let code = prefix(&source, &own, &format!("u{index}_"));
    let uses = |word: &str| identifiers(&source).any(|w| w == word);
    Ok(UserShader {
        code,
        animated: uses("time"),
        pointer: uses("pointer") || uses("hovered"),
        behind: uses("behind") || uses("behind_frosted"),
    })
}

/// Parses and validates it against the prelude, and demands `fn shade(s: Shader) -> vec4<f32>`.
fn check(source: &str, path: &str) -> Result<naga::Module, String> {
    let header = format!("{PRELUDE}{CHECK_HELPERS}");
    let offset = header.lines().count();
    let whole = format!("{header}{source}");
    // The lines of whatever naga says, moved back to the user's file.
    let at = |line: usize, col: usize| format!("{path}:{}:{}", line.saturating_sub(offset).max(1), col);
    let module = match naga::front::wgsl::parse_str(&whole) {
        Ok(m) => m,
        Err(e) => {
            let where_ = e.location(&whole).map_or(format!("{path}"), |l| at(l.line_number as usize, l.line_position as usize));
            return Err(format!("{where_}: {}", e.message()));
        }
    };
    if !module.entry_points.is_empty() {
        return Err(format!("{path}: a scene's shader has no entry points (`@fragment`, `@vertex`, `@compute`): only `fn shade(s: Shader) -> vec4<f32>` and whatever helps it"));
    }
    if !module.global_variables.is_empty() {
        return Err(format!("{path}: a scene's shader declares no variables outside its functions, nor bindings: all it can read comes in `s`, and what is behind through `behind(s, at)`"));
    }
    let shade = module.functions.iter().find(|(_, f)| f.name.as_deref() == Some("shade")).map(|(_, f)| f);
    let Some(shade) = shade else {
        return Err(format!("{path}: it is missing `fn shade(s: Shader) -> vec4<f32>`, which is what pleamar calls for each point of its box"));
    };
    let arg_ok = shade.arguments.len() == 1 && module.types[shade.arguments[0].ty].name.as_deref() == Some("Shader");
    let result_ok = shade.result.as_ref().is_some_and(|r| matches!(module.types[r.ty].inner, naga::TypeInner::Vector { size: naga::VectorSize::Quad, scalar } if scalar == naga::Scalar::F32));
    if !arg_ok || !result_ok {
        return Err(format!("{path}: `shade` has to be exactly `fn shade(s: Shader) -> vec4<f32>`: it receives the point and returns its colour, with its alpha"));
    }
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default());
    if let Err(e) = validator.validate(&module) {
        // The innermost place it points at, and the whole chain of why: naga says
        // «the function is invalid» first and the real reason further down.
        let place = e.spans().filter(|(s, _)| s.is_defined()).last().map(|(s, _)| s.location(&whole));
        let where_ = place.map_or(path.to_owned(), |l| at(l.line_number as usize, l.line_position as usize));
        let mut why = vec![e.as_inner().to_string()];
        let mut source = std::error::Error::source(e.as_inner());
        while let Some(s) = source {
            why.push(s.to_string());
            source = s.source();
        }
        return Err(format!("{where_}: {}", readable(&why.join(": "))));
    }
    Ok(module)
}

/// naga's message without its insides: «Function [2] 'shade' is invalid: The
/// `return` expression Some([2]) does not match…» says the same without the
/// handles, which mean nothing to whoever wrote the shader.
pub(crate) fn readable(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(k) = rest.find(|c| c == '[' || c == 'S') {
        let (before, after) = rest.split_at(k);
        out.push_str(before);
        let handle = |t: &str| -> Option<usize> {
            let inner = t.strip_prefix('[')?;
            let end = inner.find(']')?;
            inner[..end].chars().all(|c| c.is_ascii_digit()).then_some(end + 2)
        };
        if let Some(n) = after.strip_prefix("Some(").and_then(|t| handle(t).filter(|n| t[*n..].starts_with(')')).map(|n| n + 6)) {
            if out.ends_with(' ') {
                out.pop();
            }
            rest = &after[n..];
        } else if let Some(n) = handle(after) {
            if out.ends_with(' ') {
                out.pop();
            }
            rest = &after[n..];
        } else {
            out.push_str(&after[..1]);
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The identifiers of a WGSL text, skipping comments.
fn identifiers(source: &str) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(&source[start..i]);
        } else {
            i += 1;
        }
    }
    out.into_iter()
}

/// The same text, with the names in `own` carrying `prefix`. Comments are left
/// as they are; a field called like a function (`s.wave`) is not a function:
/// what follows a dot is left alone.
fn prefix(source: &str, own: &HashSet<String>, prefix: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len() + 64);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push_str(&source[start..i]);
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push_str(&source[start..i]);
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &source[start..i];
            let after_dot = source[..start].trim_end().ends_with('.');
            if own.contains(word) && !after_dot {
                out.push_str(prefix);
            }
            out.push_str(word);
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

/// What is added to pleamar's shader for a scene: the prelude, the real
/// helpers, each shader's code and the function that picks one by its number.
/// With no shaders, only the function, which draws nothing.
pub fn generate(shaders: &[UserShader]) -> String {
    let mut s = String::from("\n// ── the scene's own shaders ──\n");
    s.push_str(PRELUDE);
    s.push_str(REAL_HELPERS);
    for u in shaders {
        s.push_str(&u.code);
        s.push('\n');
    }
    s.push_str(
        "fn user_shader(el: Element, p: vec2<f32>) -> vec4<f32> {
    let local = to_local(p, el.t0, el.t1);
    var s: Shader;
    s.origin = el.dest.xy;
    s.pos = local - el.dest.xy;
    s.size = el.dest.zw;
    s.uv = s.pos / max(s.size, vec2<f32>(1.0));
    s.time = el.border.x;
    s.pointer = el.border.yz - el.dest.xy;
    s.hovered = el.border.w;
    s.scale = u.header.w;
    s.a = el.line;
    s.b = el.light;
    s.color = el.color0;
    s.color2 = el.color1;
    s.layer = -1;
    // Its box, with its corners: what is outside it is not painted.
    let corner = min(el.uv.x, min(s.size.x, s.size.y) * 0.5);
    let q = abs(s.pos - s.size * 0.5) - s.size * 0.5 + vec2<f32>(corner);
    let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - corner;
    let c = user_pick(u32(el.header.y), s);
    let a = clamp(c.a, 0.0, 1.0) * coverage(d);
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}

// A group's shader: its box is what the group holds; what it returns replaces it.
// (See `layer_with_effects`: dest is the box, uv the shader's number + 1 and the
// time, glass2 and glass3 its eight numbers.)
fn user_layer_shader(el: Element, p: vec2<f32>, layer: i32) -> vec4<f32> {
    var s: Shader;
    s.origin = el.dest.xy;
    s.pos = p - el.dest.xy;
    s.size = el.dest.zw;
    s.uv = s.pos / max(s.size, vec2<f32>(1.0));
    s.time = el.uv.y;
    s.pointer = vec2<f32>(-1e6);
    s.hovered = 0.0;
    s.scale = u.header.w;
    s.a = el.glass2;
    s.b = el.glass3;
    s.color = vec4<f32>(1.0);
    s.color2 = vec4<f32>(1.0);
    s.layer = layer;
    let c = user_pick(u32(el.uv.x) - 1u, s);
    let a = clamp(c.a, 0.0, 1.0);
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}

fn user_pick(k: u32, s: Shader) -> vec4<f32> {
    var c = vec4<f32>(0.0);
    switch (k) {
",
    );
    for k in 0..shaders.len() {
        s.push_str(&format!("        case {k}u: {{ c = u{k}_shade(s); }}\n"));
    }
    s.push_str(
        "        default: {}
    }
    return c;
}
",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_prefixed_but_not_fields_or_comments() {
        let own: HashSet<String> = ["wave", "shade"].iter().map(|s| s.to_string()).collect();
        let out = prefix("// wave here\nfn wave(x: f32) -> f32 { return x; }\nfn shade(s: Shader) -> vec4<f32> { let w = wave(s.time); return vec4<f32>(w); }", &own, "u0_");
        assert!(out.contains("// wave here"));
        assert!(out.contains("fn u0_wave("));
        assert!(out.contains("fn u0_shade("));
        assert!(out.contains("u0_wave(s.time)"));
        assert!(!out.contains("s.u0_"));
    }

    #[test]
    fn a_good_shader_passes_and_a_bad_one_says_where() {
        let dir = std::env::temp_dir().join(format!("pleamar-shader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.wgsl");
        std::fs::write(&good, "fn shade(s: Shader) -> vec4<f32> {\n    return vec4<f32>(s.uv, sin(s.time), 1.0);\n}\n").unwrap();
        let u = load(&good, "good.wgsl", 0).unwrap();
        assert!(u.animated && !u.behind);
        let bad = dir.join("bad.wgsl");
        std::fs::write(&bad, "fn shade(s: Shader) -> vec4<f32> {\n    return vec3<f32>(1.0);\n}\n").unwrap();
        let e = load(&bad, "bad.wgsl", 1).unwrap_err();
        assert!(e.starts_with("bad.wgsl:2:"), "{e}");
        assert!(!e.contains("[2]"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
