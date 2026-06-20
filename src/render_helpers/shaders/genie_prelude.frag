precision highp float;

#if defined(DEBUG_FLAGS)
uniform float niri_tint;
#endif

varying vec2 niri_v_coords;
uniform vec2 niri_size;

uniform sampler2D niri_tex;
uniform mat3 niri_geo_to_tex;

uniform vec4 niri_area_rect;
uniform vec4 niri_window_rect;
uniform vec4 niri_target_rect;
uniform float niri_progress;
uniform float niri_clamped_progress;
uniform float niri_direction;

uniform float niri_alpha;
uniform float niri_scale;

