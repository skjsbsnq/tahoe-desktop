vec4 genie_color(vec2 output_pos) {
    vec2 window_pos = niri_window_rect.xy;
    vec2 window_size = max(niri_window_rect.zw, vec2(1.0));
    vec2 target_pos = niri_target_rect.xy;
    vec2 target_size = max(niri_target_rect.zw, vec2(1.0));

    float progress = clamp(niri_clamped_progress, 0.0, 1.0);
    float morph = niri_direction > 0.0 ? progress : 1.0 - progress;

    float top_progress = pow(morph, 1.45);
    float bottom_progress = 1.0 - pow(1.0 - morph, 2.05);
    float settle = smoothstep(0.62, 1.0, morph);

    float window_top = window_pos.y;
    float window_bottom = window_pos.y + window_size.y;
    float target_squash_y = target_size.y * 0.075 * settle;
    float target_top = target_pos.y + target_squash_y;
    float target_bottom = target_pos.y + target_size.y - target_squash_y;

    float top = mix(window_top, target_top, top_progress);
    float bottom = mix(window_bottom, target_bottom, bottom_progress);
    float shape_top = min(top, bottom);
    float shape_bottom = max(top, bottom);
    float shape_height = max(shape_bottom - shape_top, 1.0);

    if (output_pos.y < shape_top || output_pos.y > shape_bottom) {
        return vec4(0.0);
    }

    float v = clamp((output_pos.y - top) / max(bottom - top, 1.0), 0.0, 1.0);
    float row = smoothstep(0.0, 1.0, v);
    float row_progress = mix(top_progress, bottom_progress, row);

    float window_center = window_pos.x + window_size.x * 0.5;
    float target_center = target_pos.x + target_size.x * 0.5;
    float center = mix(window_center, target_center, row_progress);

    float bend = sin(v * 3.14159265) * (target_center - window_center) * 0.16;
    center += bend * morph * (1.0 - morph);

    float half_width = mix(window_size.x, target_size.x, row_progress) * 0.5;
    float waist_profile = pow(max(sin(v * 3.14159265), 0.0), 0.75);
    float waist = 1.0 - waist_profile * (morph * 0.07 + settle * 0.075);
    float icon_squash_x = 1.0 - settle * 0.045;
    half_width = max(half_width * waist * icon_squash_x, 0.5);

    float left = center - half_width;
    float right = center + half_width;
    float shape_width = max(right - left, 1.0);

    if (output_pos.x < left || output_pos.x > right) {
        return vec4(0.0);
    }

    float u = clamp((output_pos.x - left) / shape_width, 0.0, 1.0);
    vec2 source_pos = window_pos + vec2(u, v) * window_size;
    vec2 tex_coords = (niri_geo_to_tex * vec3(source_pos, 1.0)).xy;

    if (tex_coords.x < 0.0 || tex_coords.x > 1.0
            || tex_coords.y < 0.0 || tex_coords.y > 1.0) {
        return vec4(0.0);
    }

    vec4 color = texture2D(niri_tex, tex_coords);
    float end_fade = 1.0 - smoothstep(0.82, 1.0, morph);
    return color * end_fade;
}
