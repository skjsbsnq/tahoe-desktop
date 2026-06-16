// Rounded-rectangle signed distance field, used so edge highlight, refraction and
// inner shadow line up with the actual rounded glass shape instead of the
// axis-aligned bounding rectangle.
//
// `coords` is geometry-space pixels in [0, size]; `size` is geo_size;
// `corner_radius` is vec4(top_left, top_right, bottom_right, bottom_left).

// Select the per-corner radius the same way as niri_rounding_alpha(), so the SDF
// agrees with the rounding alpha mask everywhere.
float niri_corner_radius_at(vec2 coords, vec2 size, vec4 corner_radius) {
    if (coords.x < corner_radius.x && coords.y < corner_radius.x) {
        return corner_radius.x;
    } else if (size.x - corner_radius.y < coords.x && coords.y < corner_radius.y) {
        return corner_radius.y;
    } else if (size.x - corner_radius.z < coords.x && size.y - corner_radius.z < coords.y) {
        return corner_radius.z;
    } else if (coords.x < corner_radius.w && size.y - corner_radius.w < coords.y) {
        return corner_radius.w;
    }
    return 0.0;
}

// iquilezles rounded-box SDF. Negative inside the shape, positive outside.
float niri_sd_rounded_rect(vec2 coords, vec2 size, vec4 corner_radius) {
    vec2 half_size = size * 0.5;
    float radius = niri_corner_radius_at(coords, size, corner_radius);
    vec2 q = abs(coords - half_size) - (half_size - vec2(radius));
    float outside = length(max(q, 0.0)) - radius;
    float inside = min(max(q.x, q.y), 0.0);
    return outside + inside;
}

// Outward-pointing unit gradient of the rounded-rect SDF. Outside the shape this
// is the normalized corner vector; inside the flat region it picks the dominant
// axis, matching the reference liquid-glass lens shader. `grad_radius` (a clipped
// multiple of the corner radius / half-size) keeps the gradient well-conditioned
// on large panels where the real corner is far from the pixel.
vec2 niri_sd_rounded_rect_grad(vec2 coords, vec2 size, vec4 corner_radius) {
    vec2 half_size = size * 0.5;
    float radius = niri_corner_radius_at(coords, size, corner_radius);
    float grad_radius = min(radius * 1.5, min(half_size.x, half_size.y));
    vec2 q = abs(coords - half_size) - (half_size - vec2(grad_radius));
    if (q.x >= 0.0 || q.y >= 0.0) {
        vec2 g = sign(coords - half_size) * normalize(max(q, 0.0) + 0.00001);
        // normalize() of a near-zero vector is undefined; fall back to a unit axis.
        if (dot(g, g) < 0.25) {
            return sign(coords - half_size) * normalize(vec2(q.x, q.y) + 0.00001);
        }
        return g;
    }
    float grad_x = step(q.y, q.x);
    return sign(coords - half_size) * vec2(grad_x, 1.0 - grad_x);
}
