// Backdrop filter passes. See `gpui::BackdropPass`.

// Matches `gpui::BackdropUniforms`.
struct BackdropUniforms {
    target_size: vec2<f32>,
    source_size: vec2<f32>,
    source_active_size: vec2<f32>,
    snapshot_size: vec2<f32>,
    snapshot_active_size: vec2<f32>,
    snapshot_origin: vec2<f32>,
    bounds: vec4<f32>,
    content_mask: vec4<f32>,
    corner_radii: vec4<f32>,
    sigma: f32,
    opacity: f32,
    pass_kind: u32,
    alpha_limit: f32,
    downsample_factor: f32,
    padding0: f32,
    padding1: f32,
    padding2: f32,
    tone: vec4<f32>,
}

const BACKDROP_PASS_HORIZONTAL: u32 = 0u;
const BACKDROP_PASS_COMPOSITE: u32 = 2u;

@group(0) @binding(0) var<uniform> uniforms: BackdropUniforms;
@group(0) @binding(1) var t_source: texture_2d<f32>;
@group(0) @binding(2) var t_snapshot: texture_2d<f32>;
@group(0) @binding(3) var s_backdrop: sampler;

@vertex
fn vs_backdrop(@builtin(vertex_index) vertex_id: u32) -> @builtin(position) vec4<f32> {
    let vertices = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(vertices[vertex_id], 0.0, 1.0);
}

// Samples at a position in the texture's pixels, clamped to the pixel centers of the region
// that holds this filter's snapshot. Scratch textures can be larger than it.
fn sample_source(position: vec2<f32>) -> vec4<f32> {
    let clamped = clamp(position, vec2<f32>(0.5), max(uniforms.source_active_size - 0.5, vec2<f32>(0.5)));
    return textureSampleLevel(t_source, s_backdrop, clamped / uniforms.source_size, 0.0);
}

fn sample_snapshot(position: vec2<f32>) -> vec4<f32> {
    let clamped = clamp(position, vec2<f32>(0.5), max(uniforms.snapshot_active_size - 0.5, vec2<f32>(0.5)));
    return textureSampleLevel(t_snapshot, s_backdrop, clamped / uniforms.snapshot_size, 0.0);
}

@fragment
fn fs_backdrop(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    if (uniforms.pass_kind != BACKDROP_PASS_COMPOSITE) {
        let horizontal = uniforms.pass_kind == BACKDROP_PASS_HORIZONTAL;
        let direction = select(vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), horizontal);
        let source_scale = select(1.0, uniforms.downsample_factor, horizontal);
        let sigma = max(uniforms.sigma, 0.25);
        let extent = i32(ceil(3.0 * sigma));
        var sum = vec4<f32>(0.0);
        var total = 0.0;
        for (var i = -extent; i <= extent; i++) {
            let weight = exp(-0.5 * f32(i * i) / (sigma * sigma));
            let center = (position.xy + direction * f32(i)) * source_scale;
            var tap: vec4<f32>;
            if (horizontal && uniforms.downsample_factor == 4.0) {
                // Four bilinear taps average a 4x4 box before downsampling.
                tap = (sample_source(center + vec2<f32>(1.0, 1.0))
                    + sample_source(center - vec2<f32>(1.0, 1.0))
                    + sample_source(center + vec2<f32>(1.0, -1.0))
                    + sample_source(center + vec2<f32>(-1.0, 1.0))) * 0.25;
            } else {
                tap = sample_source(center);
            }
            sum += tap * weight;
            total += weight;
        }
        return sum / total;
    }

    let local = position.xy - uniforms.snapshot_origin;
    let original = sample_snapshot(local);
    let filtered = sample_source(local / uniforms.downsample_factor);

    let half_size = uniforms.bounds.zw * 0.5;
    let delta = position.xy - uniforms.bounds.xy - half_size;
    let top_radius = select(uniforms.corner_radii.y, uniforms.corner_radii.x, delta.x < 0.0);
    let bottom_radius = select(uniforms.corner_radii.z, uniforms.corner_radii.w, delta.x < 0.0);
    let radius = select(bottom_radius, top_radius, delta.y < 0.0);
    let q = abs(delta) - half_size + radius;
    let distance = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
    let mask_end = uniforms.content_mask.xy + uniforms.content_mask.zw;
    let mask_distance = min(position.xy - uniforms.content_mask.xy, mask_end - position.xy);
    let coverage = saturate(min(-distance, min(mask_distance.x, mask_distance.y)) + 0.5) * uniforms.opacity;

    // Constrain premultiplied color to what a source-over `tone` fill could produce over
    // this pixel, without adding coverage.
    let tone = uniforms.tone.rgb;
    let tone_alpha = uniforms.tone.a;
    let lower = tone * tone_alpha * filtered.a;
    let upper = (tone * tone_alpha + (1.0 - tone_alpha)) * filtered.a;
    var treated = vec4<f32>(clamp(filtered.rgb, lower, upper), filtered.a);
    if (treated.a > uniforms.alpha_limit) {
        treated *= uniforms.alpha_limit / treated.a;
    }
    return mix(original, treated, coverage);
}
