// Client frame mask. See `gpui::ScaledClientFrame`.

// Matches `ClientFrameUniforms`.
struct ClientFrameUniforms {
    bounds: vec4<f32>,
    corner_radii: vec4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: ClientFrameUniforms;

@vertex
fn vs_client_frame(@builtin(vertex_index) vertex_id: u32) -> @builtin(position) vec4<f32> {
    let vertices = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(vertices[vertex_id], 0.0, 1.0);
}

// The alpha is the coverage of the window's rounded shape. The pipeline multiplies the target
// by it, so pixels outside the shape become transparent. Shadows clipped out of the same shape
// use the complementary coverage, which keeps the edge seamless.
@fragment
fn fs_client_frame(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let half_size = uniforms.bounds.zw * 0.5;
    let center_to_point = position.xy - uniforms.bounds.xy - half_size;
    // Top-left, top-right, bottom-right, and bottom-left radii.
    let top_radius = select(uniforms.corner_radii.y, uniforms.corner_radii.x, center_to_point.x < 0.0);
    let bottom_radius = select(uniforms.corner_radii.z, uniforms.corner_radii.w, center_to_point.x < 0.0);
    let radius = select(bottom_radius, top_radius, center_to_point.y < 0.0);
    let corner_center_to_point = abs(center_to_point) - half_size + radius;
    let distance = length(max(corner_center_to_point, vec2<f32>(0.0)))
        + min(max(corner_center_to_point.x, corner_center_to_point.y), 0.0)
        - radius;
    return vec4<f32>(0.0, 0.0, 0.0, saturate(0.5 - distance));
}
