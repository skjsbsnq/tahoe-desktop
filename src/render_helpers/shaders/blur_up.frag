#version 100

precision highp float;

varying vec2 v_coords;

uniform sampler2D tex;
uniform vec2 tex_scale;
uniform vec4 tex_bounds;
uniform vec2 half_pixel;
uniform float offset;

void main() {
    vec2 o = half_pixel * offset;

    vec2 coords = clamp(v_coords * tex_scale, tex_bounds.xy, tex_bounds.zw);
    vec2 offset = o * tex_scale;
    vec4 sum = vec4(0.0);

    // Four edge centers
    sum += texture2D(tex, clamp(coords + vec2(-offset.x * 2.0, 0.0), tex_bounds.xy, tex_bounds.zw));
    sum += texture2D(tex, clamp(coords + vec2( offset.x * 2.0, 0.0), tex_bounds.xy, tex_bounds.zw));
    sum += texture2D(tex, clamp(coords + vec2(0.0, -offset.y * 2.0), tex_bounds.xy, tex_bounds.zw));
    sum += texture2D(tex, clamp(coords + vec2(0.0,  offset.y * 2.0), tex_bounds.xy, tex_bounds.zw));

    // Four diagonal corners
    sum += texture2D(tex, clamp(coords + vec2(-offset.x,  offset.y), tex_bounds.xy, tex_bounds.zw)) * 2.0;
    sum += texture2D(tex, clamp(coords + vec2( offset.x,  offset.y), tex_bounds.xy, tex_bounds.zw)) * 2.0;
    sum += texture2D(tex, clamp(coords + vec2(-offset.x, -offset.y), tex_bounds.xy, tex_bounds.zw)) * 2.0;
    sum += texture2D(tex, clamp(coords + vec2( offset.x, -offset.y), tex_bounds.xy, tex_bounds.zw)) * 2.0;

    gl_FragColor = sum / 12.0;
}
