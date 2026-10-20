// A dual kawase frosting: down at half size a few times, up again, a grade.
fn down(s: Blur) -> vec4<f32> {
    let o = s.a.x / s.source;
    return (prev(s, s.uv + o) + prev(s, s.uv - o) + prev(s, s.uv + vec2(o.x, -o.y)) + prev(s, s.uv - vec2(o.x, -o.y))) * 0.25;
}
fn up(s: Blur) -> vec4<f32> {
    let o = s.a.x / s.source;
    return (prev(s, s.uv + vec2(o.x, 0.0)) + prev(s, s.uv - vec2(o.x, 0.0)) + prev(s, s.uv + vec2(0.0, o.y)) + prev(s, s.uv - vec2(0.0, o.y))) * 0.25;
}
fn grade(s: Blur) -> vec4<f32> {
    let c = prev(s, s.uv);
    return vec4<f32>(c.rgb * (1.0 + s.a.y), c.a);
}
