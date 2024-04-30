
#import bevy_pbr::mesh_functions::{get_model_matrix, mesh_position_local_to_clip}

struct Vertex {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,

    @location(3) i_pos_scale: vec4<f32>,
    @location(4) i_color: vec4<f32>,
    @location(5) b_color: vec4<f32>,
    @location(6) width: f32,
    @location(7) vertical_scale: f32,  // Additional scale for y-coordinate
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) border_color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) width: f32,
    @location(4) height: f32,
};

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;

    // Scale vertical position
    let scaled_y = vertex.position.y * vertex.vertical_scale;
    let scaled_x = vertex.position.x * vertex.width;

    // Adjust
    let adjusted_position = vec3<f32>(
        scaled_x,
        scaled_y,
        vertex.position.z
    );

    // Position is adjusted by the instance's position offset.
    let position = adjusted_position + vertex.i_pos_scale.xyz;

    // Compute the clip space position using the model matrix and adjusted position.
    out.clip_position = mesh_position_local_to_clip(
        get_model_matrix(0u),
        vec4<f32>(position, 1.0)  // Correct the w-component for proper clip space conversion.
    );

    // Output original UVs
    out.uv = vertex.uv;
    out.width = vertex.width;
    out.height = vertex.vertical_scale;
    out.color = vertex.i_color;
    out.border_color = vertex.b_color;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
//    let border_thickness = 2.0;  // Controls the thickness of the border.

    // Consider horizontal scaling in border detection
    // Adjust border thickness based on horizontal scale from vertex shader
//    let adjusted_border_thickness = border_thickness * (1.0 / in.width);

    // Check if the UVs are within the adjusted border region
//    if (in.uv.x < (border_thickness / in.width) || in.uv.y < (border_thickness / in.height) ||
//        in.uv.x > 1.0 - (border_thickness / in.width) || in.uv.y > 1.0 - (border_thickness / in.height)) {
//        return in.border_color;  // Render a black border
//    }

    return in.color;  // Original color inside the border
}
