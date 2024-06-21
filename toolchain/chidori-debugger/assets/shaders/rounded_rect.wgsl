#import bevy_pbr::{
    mesh_view_bindings::globals,
    forward_io::VertexOutput,
}

@group(2) @binding(0) var<uniform> width: f32;
@group(2) @binding(1) var<uniform> height: f32;
@group(2) @binding(2) var material_color_texture: texture_2d<f32>;
@group(2) @binding(3) var material_color_sampler: sampler;
@group(2) @binding(4) var<uniform> base_color: vec4<f32>;


fn sdRoundedBox(p: vec2<f32>, b: vec2<f32>, r: vec4<f32>) -> f32 {
    var r_mod = r;
    if (p.x <= 0.0) {
        r_mod = vec4<f32>(r.z, r.w, r_mod.z, r_mod.w);
    }
    if (p.y <= 0.0) {
        r_mod = vec4<f32>(r.y, r_mod.y, r_mod.z, r_mod.w);
    }
    let q = abs(p) - b + vec2<f32>(r_mod.x);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r_mod.x;
}

fn scaleUv(uv: vec2<f32>, scale: f32) -> vec2<f32> {
    return (uv - vec2<f32>(0.5)) * scale + vec2<f32>(0.5);
}



@fragment
fn fragment(
    mesh: VertexOutput,
) -> @location(0) vec4<f32> {
        let uv = mesh.uv;
        let corner_radius: f32 = 10.0;
        let aspect_ratio = width / height;
        let adjusted_uv = vec2<f32>(uv.x * aspect_ratio, uv.y);

        let border_radius = corner_radius / min(width, height);
        let dist = sdRoundedBox(adjusted_uv + vec2(-0.5 * aspect_ratio, -0.5), vec2<f32>(0.5 * aspect_ratio, 0.5), vec4<f32>(border_radius, border_radius, border_radius, border_radius));
        let aa: f32 = 0.005;
        let smooth_dist = smoothstep(0.0, aa, dist);
        if smooth_dist > 0.0 {
            let v = 1.0 - smooth_dist; // Invert for anti-aliasing effect
            return vec4<f32>(base_color.rgb, v); // Set alpha to 1.0 for visibility
        }

        var texture_color = textureSample(material_color_texture, material_color_sampler, mesh.uv);
        let color = base_color * vec4<f32>(texture_color.rgb * texture_color.a, texture_color.a);
        if texture_color.a == 0.0 {
            return base_color;
        }

//         If the texture is pure white at a give point

        return color;
}
