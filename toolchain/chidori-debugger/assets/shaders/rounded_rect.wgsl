#import bevy_pbr::forward_io::VertexOutput

@group(2) @binding(0) var<uniform> width: f32;
@group(2) @binding(1) var<uniform> height: f32;
@group(2) @binding(2) var<uniform> color: vec4<f32>;

@fragment
fn fragment(
    mesh: VertexOutput,
) -> @location(0) vec4<f32> {
    let uv = mesh.uv;
    let corner_radius: f32 = 10.0;
    let aspect_ratio = width / height;
    let adjusted_uv = vec2<f32>(uv.x * aspect_ratio, uv.y);

    let border_radius = corner_radius / min(width, height);

    let center = vec2<f32>(0.5 * aspect_ratio, 0.5);
    let distance = length(max(abs(adjusted_uv - center) - center + border_radius, vec2<f32>(0.0)));
    if distance > border_radius {
        discard;
    }
    return color;
}
