//! The frosting behind glass, as a chain of passes a scene can write itself:
//!
//! ```plm
//! blur = file "blur.wgsl" {
//!     passes: down 0.5 repeat 4, up 2 repeat 3, grade 1
//!     values: 1.5, 0.02, 1.1, 0.9
//!     reach: 32
//! }
//! ```
//!
//! Each pass names a function in the file, `fn down(s: Blur) -> vec4<f32>`, and
//! how big its target is next to the one before (the first, next to the
//! backdrop): `0.5` halves it, `2` doubles it. `repeat n` runs it n times in a
//! row, and `keep k` (0 to 3) keeps that pass's picture for any later one to
//! read with `kept(s, k, uv)`. A pass reads the one before with `prev(s, uv)`
//! and the sharp backdrop with `input(s, uv)`; the last pass's picture is what
//! the glass shows. Pictures are in premultiplied alpha, as 16-bit floats.
//!
//! The order and the sizes are the CPU's to know before a frame is drawn —each
//! pass needs the whole picture before it finished—, so they are said here,
//! and only the colour of each pixel is the shader's. `values:` are up to eight
//! numbers, `s.a` and `s.b`, expressions like any other, worked out every
//! frame; `reach:` how far the frosting reaches past a glass, in logical
//! pixels, which is how much of what is behind is captured around it.
//!
//! With no `blur` a scene gets the frosting it always had (`DEFAULT_SOURCE`).

use std::sync::atomic::{AtomicU32, Ordering};

/// What a pass receives. The same text checks it and goes into the real
/// shader, so the two cannot disagree.
pub const PRELUDE: &str = "struct Blur {
    pos: vec2<f32>,
    size: vec2<f32>,
    uv: vec2<f32>,
    source: vec2<f32>,
    scale: f32,
    index: f32,
    count: f32,
    step: f32,
    a: vec4<f32>,
    b: vec4<f32>,
};
";

/// While checking, the readers exist but read nothing.
const CHECK_HELPERS: &str = "fn prev(s: Blur, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
fn input(s: Blur, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
fn kept(s: Blur, k: i32, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(0.0); }
";

/// The real ones, over the bindings every pass gets.
const REAL_HELPERS: &str = "struct BlurUniform {
    size: vec2<f32>,
    source: vec2<f32>,
    scale: f32,
    index: f32,
    count: f32,
    step: f32,
    a: vec4<f32>,
    b: vec4<f32>,
};
@group(0) @binding(0) var blur_prev: texture_2d<f32>;
@group(0) @binding(1) var blur_input: texture_2d<f32>;
@group(0) @binding(2) var blur_kept0: texture_2d<f32>;
@group(0) @binding(3) var blur_kept1: texture_2d<f32>;
@group(0) @binding(4) var blur_kept2: texture_2d<f32>;
@group(0) @binding(5) var blur_kept3: texture_2d<f32>;
@group(0) @binding(6) var blur_sampler: sampler;
@group(0) @binding(7) var<uniform> blur_u: BlurUniform;
fn prev(s: Blur, uv: vec2<f32>) -> vec4<f32> { return textureSampleLevel(blur_prev, blur_sampler, uv, 0.0); }
fn input(s: Blur, uv: vec2<f32>) -> vec4<f32> { return textureSampleLevel(blur_input, blur_sampler, uv, 0.0); }
fn kept(s: Blur, k: i32, uv: vec2<f32>) -> vec4<f32> {
    switch k {
        case 0: { return textureSampleLevel(blur_kept0, blur_sampler, uv, 0.0); }
        case 1: { return textureSampleLevel(blur_kept1, blur_sampler, uv, 0.0); }
        case 2: { return textureSampleLevel(blur_kept2, blur_sampler, uv, 0.0); }
        default: { return textureSampleLevel(blur_kept3, blur_sampler, uv, 0.0); }
    }
}
struct BlurVertex {
    @builtin(position) pos: vec4<f32>,
};
@vertex
fn blur_vs(@builtin(vertex_index) v: u32) -> BlurVertex {
    let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u));
    var o: BlurVertex;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    return o;
}
fn blur_point(e: BlurVertex) -> Blur {
    var s: Blur;
    s.pos = e.pos.xy;
    s.size = blur_u.size;
    s.uv = e.pos.xy / blur_u.size;
    s.source = blur_u.source;
    s.scale = blur_u.scale;
    s.index = blur_u.index;
    s.count = blur_u.count;
    s.step = blur_u.step;
    s.a = blur_u.a;
    s.b = blur_u.b;
    return s;
}
";

/// Without a `blur` of its own a scene gets the frosting pleamar always had:
/// a gaussian at half size, across and then down, of 4 px of the half picture
/// for each unit of scale.
pub const DEFAULT_SOURCE: &str = "fn weight(s: Blur, k: i32) -> f32 {
    let sigma = 4.0 * s.scale;
    return exp(-f32(k * k) / (2.0 * sigma * sigma));
}
fn radius(s: Blur) -> i32 {
    return i32(ceil(4.0 * s.scale * 2.5));
}
fn wide(s: Blur) -> vec4<f32> {
    var total = vec4<f32>(0.0);
    var sum = 0.0;
    for (var k = -radius(s); k <= radius(s); k++) {
        total += input(s, s.uv + vec2<f32>(f32(k) / s.size.x, 0.0)) * weight(s, k);
        sum += weight(s, k);
    }
    return total / sum;
}
fn tall(s: Blur) -> vec4<f32> {
    var total = vec4<f32>(0.0);
    var sum = 0.0;
    for (var k = -radius(s); k <= radius(s); k++) {
        total += prev(s, s.uv + vec2<f32>(0.0, f32(k) / s.size.y)) * weight(s, k);
        sum += weight(s, k);
    }
    return total / sum;
}
";

/// `DEFAULT_SOURCE`'s passes, values and reach.
pub const DEFAULT_PASSES: &[(&str, f32, u32, Option<u8>)] = &[("wide", 0.5, 1, None), ("tall", 1.0, 1, None)];
pub const DEFAULT_VALUES: [f32; 8] = [0.0; 8];
pub const DEFAULT_REACH: f32 = 22.0;

/// One pass as written: its function, its size next to the one before, how
/// many times in a row, and the slot it keeps its picture in.
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    pub function: String,
    pub scale: f32,
    pub repeat: u32,
    pub keep: Option<u8>,
}

/// One draw: a pass with its repeats unfolded.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub entry: String,
    pub scale: f32,
    pub index: u32,
    pub count: u32,
    pub keep: Option<u8>,
}

/// A frosting already read and checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The whole module: the readers, the bindings, its code, one entry point
    /// per function a pass names.
    pub code: String,
    pub entries: Vec<String>,
    pub steps: Vec<Step>,
    /// In logical pixels.
    pub reach: f32,
}

/// How far the frosting in use reaches past a glass, in logical pixels: the
/// renderer captures that much more around each one.
static REACH: AtomicU32 = AtomicU32::new(0);

pub fn reach() -> f32 {
    match f32::from_bits(REACH.load(Ordering::Relaxed)) {
        r if r > 0.0 => r,
        _ => DEFAULT_REACH,
    }
}

pub fn set_reach(r: f32) {
    REACH.store(r.to_bits(), Ordering::Relaxed);
}

/// The frosting without a `blur`: checked like any other, so it cannot go out
/// wrong.
pub fn default_plan() -> Plan {
    let passes: Vec<Pass> = DEFAULT_PASSES.iter().map(|(f, s, r, k)| Pass { function: f.to_string(), scale: *s, repeat: *r, keep: *k }).collect();
    build(DEFAULT_SOURCE, "the default blur", &passes, DEFAULT_REACH).expect("the default blur is valid")
}

/// Reads and checks a frosting. The error comes with the line inside the file.
/// `shown` is how the scene wrote its path.
pub fn load(path: &std::path::Path, shown: &str, passes: &[Pass], reach: f32) -> Result<Plan, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{shown} cannot be read: {e}"))?;
    build(&source, shown, passes, reach)
}

fn build(source: &str, shown: &str, passes: &[Pass], reach: f32) -> Result<Plan, String> {
    if passes.is_empty() {
        return Err(format!("{shown}: a blur has at least one pass (`passes: down 0.5 repeat 4, up 2 repeat 3`)"));
    }
    let module = check(source, shown)?;
    let mut entries: Vec<String> = Vec::new();
    for p in passes {
        let Some(f) = module.functions.iter().map(|(_, f)| f).find(|f| f.name.as_deref() == Some(&p.function)) else {
            return Err(format!("{shown}: a pass names `{}`, and there is no `fn {}(s: Blur) -> vec4<f32>` in it", p.function, p.function));
        };
        let arg_ok = f.arguments.len() == 1 && module.types[f.arguments[0].ty].name.as_deref() == Some("Blur");
        let result_ok = f.result.as_ref().is_some_and(|r| matches!(module.types[r.ty].inner, naga::TypeInner::Vector { size: naga::VectorSize::Quad, scalar } if scalar == naga::Scalar::F32));
        if !arg_ok || !result_ok {
            return Err(format!("{shown}: `{}` has to be exactly `fn {}(s: Blur) -> vec4<f32>`: it receives the pixel and returns its colour, premultiplied", p.function, p.function));
        }
        if !(p.scale > 0.0 && p.scale <= 4.0) {
            return Err(format!("{shown}: a pass's size goes from just over 0 to 4 times the one before (`{}` says {})", p.function, p.scale));
        }
        if !entries.contains(&p.function) {
            entries.push(p.function.clone());
        }
    }
    let mut steps = Vec::new();
    for p in passes {
        for k in 0..p.repeat.max(1) {
            // What it keeps is its last repeat's picture.
            let last = k + 1 == p.repeat.max(1);
            steps.push(Step { entry: format!("blur_entry_{}", p.function), scale: p.scale, index: k, count: p.repeat.max(1), keep: if last { p.keep } else { None } });
        }
    }
    if steps.len() > 32 {
        return Err(format!("{shown}: more than 32 passes, counting the repeats"));
    }
    let mut code = format!("{PRELUDE}{REAL_HELPERS}{source}\n");
    for f in &entries {
        code.push_str(&format!("@fragment\nfn blur_entry_{f}(e: BlurVertex) -> @location(0) vec4<f32> {{\n    return {f}(blur_point(e));\n}}\n"));
    }
    // The whole module too, so whatever the entry points add is caught now.
    let whole = naga::front::wgsl::parse_str(&code).map_err(|e| format!("{shown}: {}", e.message()))?;
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default());
    validator.validate(&whole).map_err(|e| format!("{shown}: {}", e.as_inner()))?;
    Ok(Plan { code, entries, steps, reach: reach.max(0.0) })
}

/// Parses and validates it against the prelude: no entry points, no bindings,
/// nothing that starts with `blur_` (the names around it are pleamar's).
fn check(source: &str, path: &str) -> Result<naga::Module, String> {
    let header = format!("{PRELUDE}{CHECK_HELPERS}");
    let offset = header.lines().count();
    let whole = format!("{header}{source}");
    let at = |line: usize, col: usize| format!("{path}:{}:{}", line.saturating_sub(offset).max(1), col);
    let module = match naga::front::wgsl::parse_str(&whole) {
        Ok(m) => m,
        Err(e) => {
            let where_ = e.location(&whole).map_or(path.to_string(), |l| at(l.line_number as usize, l.line_position as usize));
            return Err(format!("{where_}: {}", e.message()));
        }
    };
    if !module.entry_points.is_empty() {
        return Err(format!("{path}: a blur has no entry points of its own: its passes are plain functions, `fn down(s: Blur) -> vec4<f32>`"));
    }
    if !module.global_variables.is_empty() {
        return Err(format!("{path}: a blur declares no variables outside its functions, nor bindings: it reads through `prev`, `input` and `kept`"));
    }
    let taken = module.functions.iter().filter_map(|(_, f)| f.name.as_deref()).chain(module.types.iter().filter_map(|(_, t)| t.name.as_deref())).find(|n| n.starts_with("blur_"));
    if let Some(n) = taken {
        return Err(format!("{path}: `{n}`: names that start with `blur_` are pleamar's"));
    }
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default());
    if let Err(e) = validator.validate(&module) {
        let place = e.spans().filter(|(s, _)| s.is_defined()).last().map(|(s, _)| s.location(&whole));
        let where_ = place.map_or(path.to_owned(), |l| at(l.line_number as usize, l.line_position as usize));
        let mut why = vec![e.as_inner().to_string()];
        let mut source = std::error::Error::source(e.as_inner());
        while let Some(s) = source {
            why.push(s.to_string());
            source = s.source();
        }
        return Err(format!("{where_}: {}", crate::shaders::readable(&why.join(": "))));
    }
    Ok(module)
}

/// The pipelines of a plan, built once per plan.
pub struct Built {
    pub plan: Plan,
    /// Bumped each time it is rebuilt: what holds pictures sized for the old
    /// plan makes them again.
    pub generation: u64,
    pipelines: std::collections::HashMap<String, wgpu::RenderPipeline>,
    pub layout: wgpu::BindGroupLayout,
}

/// The pictures between passes are 16-bit floats: a blur over a dark gradient
/// does not band, and what is unknown (alpha 0) does not blow out where it is
/// divided back.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

impl Built {
    pub fn new(d: &wgpu::Device, plan: Plan, generation: u64) -> Result<Built, String> {
        let texture = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let mut entries: Vec<wgpu::BindGroupLayoutEntry> = (0..6).map(texture).collect();
        entries.push(wgpu::BindGroupLayoutEntry { binding: 6, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None });
        entries.push(wgpu::BindGroupLayoutEntry { binding: 7, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None });
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("blur"), entries: &entries });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("blur"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("blur"), source: wgpu::ShaderSource::Wgsl(plan.code.as_str().into()) });
        let mut pipelines = std::collections::HashMap::new();
        for f in &plan.entries {
            let entry = format!("blur_entry_{f}");
            let p = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("blur_vs"), compilation_options: Default::default(), buffers: &[] },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some(&entry), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })] }),
                multiview_mask: None,
                cache: None,
            });
            pipelines.insert(entry, p);
        }
        if let Some(e) = pollster::block_on(scope.pop()) {
            return Err(e.to_string());
        }
        Ok(Built { plan, generation, pipelines, layout })
    }

    pub fn pipeline(&self, entry: &str) -> &wgpu::RenderPipeline {
        &self.pipelines[entry]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scene without a `blur` gets the old frosting: across at half size, then down.
    #[test]
    fn the_default_frosts() {
        let plan = default_plan();
        let entries: Vec<_> = plan.steps.iter().map(|s| (s.entry.as_str(), s.scale)).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, 0.5);
        assert_eq!(entries[1].1, 1.0);
        assert_eq!(plan.reach, DEFAULT_REACH);
    }

    /// `repeat` unfolds into steps that know their place in the run; `keep`
    /// keeps the picture the pass ends with, its last repeat's.
    #[test]
    fn repeats_unfold() {
        let source = "fn down(s: Blur) -> vec4<f32> { return prev(s, s.uv); }\nfn up(s: Blur) -> vec4<f32> { return prev(s, s.uv); }\n";
        let passes = [Pass { function: "down".into(), scale: 0.5, repeat: 3, keep: None }, Pass { function: "up".into(), scale: 2.0, repeat: 2, keep: Some(1) }];
        let plan = build(source, "test", &passes, 16.0).expect("valid");
        let steps: Vec<_> = plan.steps.iter().map(|s| (s.index, s.count, s.keep)).collect();
        assert_eq!(steps, [(0, 3, None), (1, 3, None), (2, 3, None), (0, 2, None), (1, 2, Some(1))]);
    }

    /// A pass naming a function the file does not have is refused.
    #[test]
    fn a_missing_function_is_an_error() {
        let passes = [Pass { function: "nope".into(), scale: 1.0, repeat: 1, keep: None }];
        assert!(build("fn down(s: Blur) -> vec4<f32> { return prev(s, s.uv); }\n", "test", &passes, 0.0).is_err());
    }
}
