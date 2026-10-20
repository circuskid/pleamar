//! The lens: seeing what is behind the glass so it can be bent.
//!
//! Real glass does not show what is behind it as is: near the edge it bends
//! it, like a drop. That needs the pixels behind, and in Wayland only the
//! compositor has them. It is asked for a capture of the screen where the
//! surface is —with the surface on top, which is the only thing it can give—
//! and it gets unmixed: capture = ours + (1 − our alpha) · background, and we
//! know ours because we paint it on a canvas before presenting it. Measured:
//! the unmixed background is off from the real one by less than half a level
//! out of 255 with a 30 % tint, and by less than two with an 85 % edge.
//!
//! With the background already clean it is blurred a little —the frosting of
//! the glass— and the shapes shader reads it displaced according to the bevel.
//! So pleamar paints the lens itself, without the compositor knowing anything
//! about glass.

use crate::platform::Backdrop;

// How what is behind gets frosted is the scene's: see `blur.rs`.

pub struct Pipelines {
    unmix: wgpu::RenderPipeline,
    /// Unmixing into a draft while counting what says the capture is of
    /// another frame, and keeping the draft only if almost nothing did
    /// (`backdrop_check.wgsl`); none where a fragment cannot count (OpenGL).
    checked: Option<(wgpu::RenderPipeline, wgpu::RenderPipeline)>,
    own: wgpu::RenderPipeline,
    own_over: wgpu::RenderPipeline,
    linear: wgpu::Sampler,
}

impl Pipelines {
    pub fn new(d: &wgpu::Device) -> Pipelines {
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("backdrop"), source: wgpu::ShaderSource::Wgsl(include_str!("backdrop.wgsl").into()) });
        let pipeline_to = |fs: &str, format: wgpu::TextureFormat| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: None,
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = |fs: &str| pipeline_to(fs, wgpu::TextureFormat::Rgba8Unorm);
        let linear = d.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        // The same file and the counting, as one module: only where it builds.
        let checked = {
            let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
            let source = format!("{}{}", include_str!("backdrop.wgsl"), include_str!("backdrop_check.wgsl"));
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("backdrop, checked"), source: wgpu::ShaderSource::Wgsl(source.into()) });
            let make = |fs: &str| {
                d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(fs),
                    layout: None,
                    vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some(fs),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            };
            let both = (make("unmix_checked"), make("settle"));
            match pollster::block_on(scope.pop()) {
                None => Some(both),
                Some(e) => {
                    eprintln!("render · the glass unmixes each capture without checking it is of the frame it was taken with ({e})");
                    None
                }
            }
        };
        Pipelines {
            unmix: pipeline("unmix"),
            checked,
            own: pipeline("own"),
            own_over: pipeline("own_over"),
            linear,
        }
    }
}

pub fn texture(d: &wgpu::Device, label: &str, (w, h): (u32, u32), format: wgpu::TextureFormat, usage: wgpu::TextureUsages) -> (wgpu::Texture, wgpu::TextureView) {
    let t = d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let v = t.create_view(&Default::default());
    (t, v)
}

/// What a surface with glass has: where each frame is painted before
/// presenting it, the last capture, and the unmixed background —sharp and
/// frosted—.
/// How long a new lens keeps capturing back to back before it relies on
/// «only when something changes», and at least how many captures. The compositor
/// fades a layer in when it appears —a few hundred milliseconds in which its real
/// opacity is less than 1—, and what is unmixed from those captures is wrong:
/// by a little under something dark, by everything under something bright.
/// Something that keeps moving replaces it at once; a glass or a shader that
/// stays still from its first frame kept it for ever.
pub const WARM_UP: u32 = 3;
pub const WARM_UP_TIME: std::time::Duration = std::time::Duration::from_millis(1500);

pub struct Lens {
    pub px: (u32, u32),
    /// Two canvases: one gets painted, the other is the one the compositor has.
    canvases: [(wgpu::Texture, wgpu::TextureView); 2],
    next: usize,
    presented: Option<usize>,
    capture: Option<(wgpu::Texture, wgpu::TextureView, (u32, u32))>,
    /// The unmixed background, in two to go back and forth: each time the one
    /// that is not the current one is written, reading the current one where
    /// it cannot be unmixed.
    backgrounds: [(wgpu::Texture, wgpu::TextureView); 2],
    current: usize,
    /// What a capture unmixes to before it is known to be of the right frame,
    /// and how many pixels were looked at and how many said it is not
    /// (`Pipelines::checked`).
    draft: (wgpu::Texture, wgpu::TextureView),
    verdict: wgpu::Buffer,
    frost: Frost,
    /// A sample of the last capture, and of which box it was: so as not to
    /// repeat the work if nothing has changed.
    sample: Vec<u8>,
    pub bounds: [i32; 4],
    bounds_uniforms: wgpu::Buffer,
    /// A received capture whose background is still to be unmixed: which
    /// canvas was presented and with which clip.
    pending: Option<(usize, (u32, u32, u32, u32))>,
    /// There is already a background to show.
    pub ready: bool,
    /// How many captures it has taken in, and since when it exists: see `WARM_UP`.
    pub received: u32,
    born: std::time::Instant,
    /// What the shapes shader reads: the sharp background and the frosted one.
    pub group: Option<wgpu::BindGroup>,
}

impl Lens {
    /// The blur plan its frosting was made for.
    pub fn blur_generation(&self) -> u64 {
        self.frost.generation
    }

    pub fn new(d: &wgpu::Device, format: wgpu::TextureFormat, px: (u32, u32), b: &crate::blur::Built) -> Lens {
        use wgpu::TextureUsages as U;
        let canvas = || texture(d, "canvas", px, format, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC);
        let background = || texture(d, "background", px, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC | U::COPY_DST);
        Lens {
            px,
            canvases: [canvas(), canvas()],
            next: 0,
            presented: None,
            capture: None,
            backgrounds: [background(), background()],
            current: 0,
            draft: texture(d, "draft", px, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING),
            verdict: d.create_buffer(&wgpu::BufferDescriptor { label: Some("verdict"), size: 8, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
            frost: Frost::new(d, px, b),
            sample: Vec::new(),
            pending: None,
            bounds: [0; 4],
            bounds_uniforms: d.create_buffer(&wgpu::BufferDescriptor { label: Some("bounds"), size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
            ready: false,
            received: 0,
            born: std::time::Instant::now(),
            group: None,
        }
    }

    /// Which of the two canvases this frame is painted on.
    pub fn next(&self) -> usize {
        self.next
    }

    /// Where this frame is painted.
    pub fn canvas(&self) -> &wgpu::TextureView {
        &self.canvases[self.next].1
    }

    /// What was painted goes to the screen: it is copied, and stays as the presented one.
    pub fn copy_to(&mut self, encoder: &mut wgpu::CommandEncoder, screen: &wgpu::Texture) {
        let (w, h) = self.px;
        encoder.copy_texture_to_texture(
            self.canvases[self.next].0.as_image_copy(),
            screen.as_image_copy(),
            wgpu::Extent3d { width: w.min(screen.width()), height: h.min(screen.height()), depth_or_array_layers: 1 },
        );
        self.presented = Some(self.next);
        self.next ^= 1;
    }

    /// A capture of the box `bounds` arrives (logical pixels of the surface):
    /// the background is unmixed and frosted, only there. `true` if something
    /// visible has changed, and it has to be painted again.
    pub fn receive(&mut self, g: &crate::gpu::Gpu, d: Backdrop, scale: f32, bounds: [i32; 4]) -> bool {
        let Some(presented) = self.presented else { return false };
        // A capture without data is one that failed.
        let Some(data) = d.data else { return false };
        let data: &[u8] = (*data).as_ref();
        // Counted even if it brings nothing new: a still background must not
        // keep the warm-up going for ever. During the warm-up none is skipped
        // for looking like the last one: the first ones are the ones not to
        // trust, and with a still scene the next ones look exactly like them.
        self.received += 1;
        // The same as the previous one —or almost: the rounding of going back
        // and forth gives the odd stray level—, and there is nothing new
        // behind. Without this, painting the lens changes the capture, which
        // changes the lens, and so on without end. One byte in thirteen is
        // looked at, which is enough to know whether something has moved.
        let sample: Vec<u8> = data.iter().copied().step_by(13).collect();
        if self.ready && !self.warming() && bounds == self.bounds && sample.len() == self.sample.len() && sample.iter().zip(&self.sample).all(|(a, b)| a.abs_diff(*b) <= 3) {
            return false;
        }
        self.sample = sample;
        self.bounds = bounds;
        let dev = &g.device;
        let size = (d.width, d.height);
        if self.capture.as_ref().is_none_or(|f| f.2 != size) {
            let (tx, v) = texture(dev, "capture", size, wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            self.capture = Some((tx, v, size));
        }
        let capture = self.capture.as_ref().unwrap();
        g.queue.write_texture(
            capture.0.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(d.stride), rows_per_image: Some(d.height) },
            wgpu::Extent3d { width: d.width, height: d.height, depth_or_array_layers: 1 },
        );
        // The box in real pixels, inside the canvas.
        let (lw, lh) = self.px;
        let x0 = ((bounds[0] as f32 * scale).floor() as u32).min(lw);
        let y0 = ((bounds[1] as f32 * scale).floor() as u32).min(lh);
        let x1 = (((bounds[0] + bounds[2]) as f32 * scale).ceil() as u32).min(lw);
        let y1 = (((bounds[1] + bounds[3]) as f32 * scale).ceil() as u32).min(lh);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        let scissor = (x0, y0, x1 - x0, y1 - y0);
        g.queue.write_buffer(&self.bounds_uniforms, 0, bytemuck::cast_slice(&[bounds[0] as f32 * scale, bounds[1] as f32 * scale, bounds[2] as f32 * scale, bounds[3] as f32 * scale, d.opacity, 0.0, 0.0, 0.0]));
        // The passes go in the same submission as the frame being painted now
        // —it has to be painted again—: a separate one cost one more `submit`,
        // a third of a millisecond per capture.
        self.pending = Some((presented, scissor));
        let new = self.current ^ 1;
        self.group = Some(g.backdrop_group(&self.backgrounds[new].1, self.frost.result()));
        self.ready = true;
        true
    }

    /// The unmixed background as it stands, once there is one.
    pub fn background(&self) -> Option<&wgpu::TextureView> {
        self.ready.then(|| &self.backgrounds[self.current].1)
    }

    /// Still in its first moments: see `WARM_UP`.
    pub fn warming(&self) -> bool {
        self.received < WARM_UP || self.born.elapsed() < WARM_UP_TIME
    }

    /// Unmixes and frosts what the last capture left, in the frame's
    /// submission, before painting it.
    pub fn prepare(&mut self, g: &crate::gpu::Gpu, t: &Pipelines, enc: &mut wgpu::CommandEncoder, scale: f32) {
        let Some((presented, scissor)) = self.pending.take() else { return };
        let (Some(capture), (lw, lh)) = (self.capture.as_ref(), self.px) else { return };
        let dev = &g.device;
        // What is outside the box stays as it was: the new background starts
        // out as a copy of the current one, and only the inside is written.
        let new = self.current ^ 1;
        enc.copy_texture_to_texture(self.backgrounds[self.current].0.as_image_copy(), self.backgrounds[new].0.as_image_copy(), wgpu::Extent3d { width: lw, height: lh, depth_or_array_layers: 1 });
        let unmixing = |layout: &wgpu::BindGroupLayout, verdict: Option<&wgpu::Buffer>| {
            let mut entries = vec![
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&capture.1) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.canvases[presented].1) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&self.backgrounds[self.current].1) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&t.linear) },
                wgpu::BindGroupEntry { binding: 4, resource: self.bounds_uniforms.as_entire_binding() },
            ];
            if let Some(v) = verdict {
                entries.push(wgpu::BindGroupEntry { binding: 5, resource: v.as_entire_binding() });
            }
            dev.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout, entries: &entries })
        };
        match &t.checked {
            // Into the draft, counting; then into the new background, or what was there.
            Some((check, settle)) => {
                enc.clear_buffer(&self.verdict, 0, None);
                pass(enc, check, &self.draft.1, &unmixing(&check.get_bind_group_layout(0), Some(&self.verdict)), scissor);
                let settle_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &settle.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&self.draft.1) },
                        wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&self.backgrounds[self.current].1) },
                        wgpu::BindGroupEntry { binding: 8, resource: self.verdict.as_entire_binding() },
                    ],
                });
                pass(enc, settle, &self.backgrounds[new].1, &settle_group, scissor);
            }
            None => pass(enc, &t.unmix, &self.backgrounds[new].1, &unmixing(&t.unmix.get_bind_group_layout(0), None), scissor),
        }
        self.frost.run(g, enc, &self.backgrounds[new].1, scissor, scale);
        self.current = new;
    }
}

/// One full-screen triangle with `pipeline`, clipped to `scissor`, onto what is there.
fn pass(enc: &mut wgpu::CommandEncoder, pipeline: &wgpu::RenderPipeline, target: &wgpu::TextureView, group: &wgpu::BindGroup, scissor: (u32, u32, u32, u32)) {
    let mut p = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    p.set_pipeline(pipeline);
    p.set_bind_group(0, group, &[]);
    p.set_scissor_rect(scissor.0, scissor.1, scissor.2, scissor.3);
    p.draw(0..3, 0..1);
}

/// The frosting's pictures, one per pass of the scene's blur (`blur.rs`),
/// sized for it; the last is what the shapes shader reads.
struct Frost {
    levels: Vec<(wgpu::Texture, wgpu::TextureView)>,
    uniforms: Vec<wgpu::Buffer>,
    /// The plan it was made for: one made for another is made again.
    generation: u64,
}

impl Frost {
    fn new(d: &wgpu::Device, px: (u32, u32), b: &crate::blur::Built) -> Frost {
        use wgpu::TextureUsages as U;
        let mut size = (px.0 as f32, px.1 as f32);
        let mut levels = Vec::new();
        for step in &b.plan.steps {
            size = (size.0 * step.scale, size.1 * step.scale);
            let px = ((size.0.ceil() as u32).clamp(1, 8192), (size.1.ceil() as u32).clamp(1, 8192));
            levels.push(texture(d, "frost", px, crate::blur::FORMAT, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING));
        }
        let uniforms = b.plan.steps.iter().map(|_| d.create_buffer(&wgpu::BufferDescriptor { label: Some("frost"), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })).collect();
        Frost { levels, uniforms, generation: b.generation }
    }

    fn result(&self) -> &wgpu::TextureView {
        &self.levels[self.levels.len() - 1].1
    }

    /// Frosts the sharp background in `scissor` (real pixels) through the
    /// blur's passes. Every pass is clipped to the scissor at its size, with
    /// the reach of the frosting around it.
    fn run(&self, g: &crate::gpu::Gpu, enc: &mut wgpu::CommandEncoder, sharp: &wgpu::TextureView, scissor: (u32, u32, u32, u32), scale: f32) {
        let b = g.blur();
        if b.generation != self.generation {
            return;
        }
        let dev = &g.device;
        let values = g.blur_values;
        let full = (sharp.texture().width() as f32, sharp.texture().height() as f32);
        let reach = b.plan.reach * scale.max(1.0);
        // The pictures kept for later passes, by slot; an unused one reads the backdrop.
        let mut kept: [&wgpu::TextureView; 4] = [sharp; 4];
        let mut prev = sharp;
        let mut prev_size = full;
        for (k, step) in b.plan.steps.iter().enumerate() {
            let (w, h) = (self.levels[k].0.width(), self.levels[k].0.height());
            let f = w as f32 / full.0;
            let m = (reach * f).ceil() as u32 + 2;
            let x0 = ((scissor.0 as f32 * f) as u32).saturating_sub(m);
            let y0 = ((scissor.1 as f32 * f) as u32).saturating_sub(m);
            let x1 = (((scissor.0 + scissor.2) as f32 * f).ceil() as u32 + m).min(w);
            let y1 = (((scissor.1 + scissor.3) as f32 * f).ceil() as u32 + m).min(h);
            let clip = (x0.min(w.saturating_sub(1)), y0.min(h.saturating_sub(1)), x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
            let u = [w as f32, h as f32, prev_size.0, prev_size.1, scale, step.index as f32, step.count as f32, k as f32, values[0], values[1], values[2], values[3], values[4], values[5], values[6], values[7]];
            g.queue.write_buffer(&self.uniforms[k], 0, bytemuck::cast_slice(&u));
            let group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &b.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(prev) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(sharp) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(kept[0]) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(kept[1]) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(kept[2]) },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(kept[3]) },
                    wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::Sampler(&g.blur_sampler) },
                    wgpu::BindGroupEntry { binding: 7, resource: self.uniforms[k].as_entire_binding() },
                ],
            });
            pass(enc, b.pipeline(&step.entry), &self.levels[k].1, &group, clip);
            if let Some(slot) = step.keep {
                kept[slot as usize] = &self.levels[k].1;
            }
            prev = &self.levels[k].1;
            prev_size = (w as f32, h as f32);
        }
    }
}

/// The lens of a scene that is the whole screen —pleamar-wm's—: there is no
/// compositor to ask what is behind, because what is behind is what the scene
/// painted itself before the glass (the wallpaper, the windows). Right before
/// a glass is painted, that is copied and frosted, and the glass bends it like
/// any other background.
///
/// A scene on a surface uses it too, when a glass of its has something of the
/// scene's own under it: then what is behind that glass is what the scene
/// painted before it, over whatever the capture says is behind the surface.
pub struct Own {
    pub px: (u32, u32),
    sharp: (wgpu::Texture, wgpu::TextureView),
    frost: Frost,
    /// What the shapes shader reads.
    pub group: wgpu::BindGroup,
}

impl Own {
    /// The blur plan its frosting was made for.
    pub fn blur_generation(&self) -> u64 {
        self.frost.generation
    }

    pub fn new(g: &crate::gpu::Gpu, px: (u32, u32)) -> Own {
        use wgpu::TextureUsages as U;
        let d = &g.device;
        let sharp = texture(d, "painted below the glass", px, wgpu::TextureFormat::Rgba8Unorm, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING);
        let frost = Frost::new(d, px, g.blur());
        let group = g.backdrop_group(&sharp.1, frost.result());
        Own { px, sharp, frost, group }
    }

    /// What has been painted so far in `painted`, in `scissor` (real pixels),
    /// becomes what is behind the glass; with `behind` —the unmixed background
    /// of a surface—, over that.
    pub fn take(&self, g: &crate::gpu::Gpu, t: &Pipelines, enc: &mut wgpu::CommandEncoder, painted: &wgpu::TextureView, behind: Option<&wgpu::TextureView>, scissor: (u32, u32, u32, u32), scale: f32) {
        let pipeline = if behind.is_some() { &t.own_over } else { &t.own };
        let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(painted) }];
        if let Some(behind) = behind {
            entries.push(wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(behind) });
        }
        let group = g.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries: &entries });
        pass(enc, pipeline, &self.sharp.1, &group, scissor);
        self.frost.run(g, enc, &self.sharp.1, scissor, scale);
    }
}

/// At most, one capture of what is behind every so often while painting.
pub const CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How much margin the frosting needs around a glass, in logical pixels:
/// the reach of the scene's blur.
pub fn margin() -> f32 {
    crate::blur::reach() + 2.0
}
