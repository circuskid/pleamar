
// ── unmix, checked ────────────────────────────────────────────────
// Where the card can count from a fragment: the same unmixing, into a draft,
// counting the pixels that say the capture is of another frame of ours than
// the canvas (the compositor put together one we presented before or after).
// Then `settle` keeps the draft only if almost none said so. Pixel by pixel,
// a capture of another frame left what passed for possible: our own last
// frame, and colours that never were, in the glass for a moment.
@group(0) @binding(5) var<storage, read_write> verdict: array<atomic<u32>, 2>;

@fragment
fn unmix_checked(e: VertexOut) -> @location(0) vec4<f32> {
    let u = unmix_at(e.pos.xy);
    atomicAdd(&verdict[0], 1u);
    if (u.wrong) {
        atomicAdd(&verdict[1], 1u);
    }
    return u.value;
}

@group(0) @binding(6) var draft: texture_2d<f32>;
@group(0) @binding(7) var kept: texture_2d<f32>;
@group(0) @binding(8) var<storage, read> counted: array<u32, 2>;

@fragment
fn settle(e: VertexOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(e.pos.xy);
    // More than one pixel in a hundred (and more than a handful, an edge's
    // rounding): the capture is not of this canvas, and what was there stays.
    if (counted[1] > 16u && counted[1] * 100u > counted[0]) {
        return textureLoad(kept, px, 0);
    }
    return textureLoad(draft, px, 0);
}
