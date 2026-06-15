uniform float noise;
uniform float saturation;
uniform vec4 bg_color;
uniform vec4 tint_color;
uniform float tint_amount;
uniform float edge_highlight;
uniform float refraction;

// Sin-less white noise by David Hoskins (MIT License).
// https://www.shadertoy.com/view/4djSRW
float hash12(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

vec3 saturate(vec3 color, float sat) {
    const vec3 w = vec3(0.2126, 0.7152, 0.0722);
    return mix(vec3(dot(color, w)), color, sat);
}

float value_noise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);

    float a = hash12(i);
    float b = hash12(i + vec2(1.0, 0.0));
    float c = hash12(i + vec2(0.0, 1.0));
    float d = hash12(i + vec2(1.0, 1.0));

    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

float glass_surface_detail() {
    float long_edge = max(max(geo_size.x, geo_size.y), 1.0);
    float area = max(geo_size.x * geo_size.y, 1.0);
    float long_fade = 1.0 - smoothstep(620.0, 980.0, long_edge);
    float area_fade = 1.0 - smoothstep(180000.0, 420000.0, area);

    return clamp(min(long_fade, area_fade), 0.0, 1.0);
}

float glass_rim(vec2 coords_geo) {
    vec2 coords = clamp(coords_geo, vec2(0.0), vec2(1.0));
    vec2 size = max(geo_size, vec2(1.0));
    vec2 edge = min(coords * size, (vec2(1.0) - coords) * size);
    float edge_dist = min(edge.x, edge.y);
    float rim_width = clamp(min(size.x, size.y) * 0.12, 10.0, 34.0);

    return 1.0 - smoothstep(2.0, rim_width, edge_dist);
}

float glass_height(vec2 coords_geo) {
    vec2 coords = clamp(coords_geo, vec2(0.0), vec2(1.0));
    float detail = glass_surface_detail();
    vec2 centered = coords * 2.0 - vec2(1.0);
    centered *= vec2(0.92, 1.08);

    float dome = pow(max(1.0 - dot(centered, centered), 0.0), 0.72);
    float rim = glass_rim(coords);
    vec2 p = coords * max(geo_size, vec2(1.0));
    float turbulence =
        value_noise(p * 0.030 + vec2(13.7, 5.1)) * 0.62 +
        value_noise(p * 0.072 + vec2(2.8, 29.4)) * 0.38 -
        0.5;

    return dome * (0.18 * detail) + rim * 0.42 + turbulence * (0.035 + detail * 0.045);
}

vec3 glass_normal(vec2 coords_geo) {
    vec2 texel = max(vec2(1.0) / max(geo_size, vec2(1.0)), vec2(0.0015));
    float h = glass_height(coords_geo);
    float hx = glass_height(coords_geo + vec2(texel.x, 0.0)) - h;
    float hy = glass_height(coords_geo + vec2(0.0, texel.y)) - h;
    vec2 gradient = vec2(hx, hy);

    return normalize(vec3(-gradient * 10.0, 1.0));
}

float glass_light_strength(vec2 coords_geo) {
    vec2 coords = clamp(coords_geo, vec2(0.0), vec2(1.0));
    vec2 size = max(geo_size, vec2(1.0));
    float detail = glass_surface_detail();
    vec3 normal = glass_normal(coords);
    vec3 light_dir = normalize(vec3(-0.55, -0.72, 0.86));
    vec3 half_dir = normalize(light_dir + vec3(0.0, 0.0, 1.0));

    float rim = glass_rim(coords);
    float diffuse = max(dot(normal, light_dir), 0.0);
    float specular = pow(max(dot(normal, half_dir), 0.0), 42.0);
    float top_light = 1.0 - smoothstep(0.0, min(size.y * 0.42, 150.0), coords.y * size.y);
    float left_light = 1.0 - smoothstep(0.0, min(size.x * 0.34, 140.0), coords.x * size.x);
    float caustic = smoothstep(
        0.48,
        1.0,
        value_noise(coords * size * 0.026 + vec2(8.3, 17.1))
    );

    return clamp(
        rim * 0.34 +
        diffuse * (0.16 + detail * 0.08) +
        specular * (0.42 * detail) +
        top_light * 0.08 +
        left_light * 0.04 +
        caustic * rim * (0.08 + detail * 0.04),
        0.0,
        1.0
    );
}

vec2 niri_refraction_offset(vec2 coords_geo) {
    float amount = clamp(refraction, 0.0, 0.12);
    if (amount <= 0.0) {
        return vec2(0.0);
    }

    vec2 coords = clamp(coords_geo, vec2(0.0), vec2(1.0));
    vec3 normal = glass_normal(coords);
    float rim = glass_rim(coords);
    vec2 p = coords * max(geo_size, vec2(1.0));
    vec2 turbulence = vec2(
        value_noise(p * 0.044 + vec2(3.1, 9.7)) - 0.5,
        value_noise(p * 0.044 + vec2(21.4, 6.2)) - 0.5
    );
    float detail = glass_surface_detail();
    float amount_scale = 0.35 + detail * 0.65;

    return (normal.xy * (0.55 + rim * 1.45) + turbulence * (0.18 + rim * 0.26)) * amount * amount_scale;
}

vec4 postprocess(vec4 color, vec2 coords_geo) {
    if (saturation != 1.0) {
        color.rgb = saturate(color.rgb, saturation);
    }

    if (noise > 0.0) {
        vec2 uv = gl_FragCoord.xy;
        color.rgb += (hash12(uv) - 0.5) * noise;
    }

    // Mix bg_color behind the texture (both premultiplied alpha).
    color = color + bg_color * (1.0 - color.a);

    float tint_mix = clamp(tint_amount * tint_color.a, 0.0, 1.0);
    if (tint_mix > 0.0) {
        color.rgb = mix(color.rgb, tint_color.rgb * color.a, tint_mix);
    }

    float highlight = glass_light_strength(coords_geo) * clamp(edge_highlight, 0.0, 2.0);
    if (highlight > 0.0) {
        color.rgb += vec3(highlight * color.a * 0.28);
    }

    return color;
}
