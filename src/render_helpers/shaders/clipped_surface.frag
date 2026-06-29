#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float niri_scale;

uniform vec2 geo_size;
uniform vec4 corner_radius;
uniform mat3 input_to_geo;
uniform float chromatic;

float niri_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);
vec2 niri_refraction_sample_coords(vec2 input_coords, vec2 coords_geo);
vec4 postprocess(vec4 color, vec2 coords_geo);

void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);

    // Keep geometry math in clip-region space, then map the refracted point
    // back to texture/input space for sampling. This keeps expanded sampling
    // padding from changing the apparent lens strength.
    vec2 sample_coords = niri_refraction_sample_coords(v_coords, coords_geo.xy);
    vec2 offset = sample_coords - v_coords;

    // Chromatic aberration: split the R/G/B samples along the displacement
    // direction. `chromatic` is shared with postprocess.frag and defaults to 0.
    vec4 color;
    if (chromatic > 0.0) {
        vec2 dir = normalize(offset + vec2(0.00001));
        float split = length(offset) * chromatic * 6.0;
        vec2 rcoord = clamp(sample_coords + dir * split, vec2(0.001), vec2(0.999));
        vec2 gcoord = clamp(sample_coords, vec2(0.001), vec2(0.999));
        vec2 bcoord = clamp(sample_coords - dir * split, vec2(0.001), vec2(0.999));
        vec4 cr = texture2D(tex, rcoord);
        vec4 cg = texture2D(tex, gcoord);
        vec4 cb = texture2D(tex, bcoord);
        color = vec4(cr.r, cg.g, cb.b, max(cr.a, max(cg.a, cb.a)));
    } else {
        color = texture2D(tex, clamp(sample_coords, vec2(0.001), vec2(0.999)));
    }
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    color = postprocess(color, coords_geo.xy);

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside geometry.
        color = vec4(0.0);
    } else {
        // Apply corner rounding inside geometry.
        color = color * niri_rounding_alpha(coords_geo.xy * geo_size, geo_size, corner_radius);
    }

    // Apply final alpha and tint.
    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
