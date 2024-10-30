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


//        let target_width = 620.0;
//        let target_height = 320.0;
//        let target_aspect_ratio = target_width / target_height;

        // Calculate scale factors for the UV adjustment
//        let scale_x =  width / target_width;
//        let scale_y = height / target_height;

//        let adjusted_uv_target = vec2<f32>(uv.x * scale_x, uv.y * scale_y);
        let adjusted_uv_target = vec2<f32>(uv.x, uv.y);


        var texture_color = textureSample(material_color_texture, material_color_sampler, adjusted_uv_target);
        let color = vec4<f32>(texture_color.rgb * texture_color.a, texture_color.a);

        let border_radius = corner_radius / min(width, height);
        let dist = sdRoundedBox(adjusted_uv + vec2(-0.5 * aspect_ratio, -0.5), vec2<f32>(0.5 * aspect_ratio, 0.5), vec4<f32>(border_radius, border_radius, border_radius, border_radius));
        let aa: f32 = 0.005;
        let smooth_dist = smoothstep(0.0, aa, dist);
        if smooth_dist > 0.0 {
            return vec4<f32>(color.rgb, 0.0); // Set alpha to 1.0 for visibility
        }

        if texture_color.a == 0.0 {
            return base_color;
        }

//         If the texture is pure white at a give point

        return color;
}
