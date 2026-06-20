
void main() {
    vec2 output_pos = niri_area_rect.xy + niri_v_coords * niri_area_rect.zw;
    vec4 color = genie_color(output_pos);

    color = color * niri_alpha;

#if defined(DEBUG_FLAGS)
    if (niri_tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
