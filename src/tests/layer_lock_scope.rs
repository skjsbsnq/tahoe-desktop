//! T03 static guard: the layer-shell commit/destroy paths must never hold the
//! layer-map guard across renderer, rules, foreign-rect, pointer, IPC and
//! construction work (STAB-04).
//!
//! These are source-contract tests (same precedent as R17's structural
//! `queue_redraw_all` checks): they scan the production file for the guard's
//! brace scope and assert that the forbidden phase-2 work happens only after
//! the guard's enclosing block closes. The braces are real scope markers: the
//! `MutexGuard` is block-scoped, so a token between `layer_map_for_output`
//! and the closing brace provably executes while the guard is alive.

fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                out.push('"');
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    out.push('"');
                    i += 1;
                }
            }
            b'\'' => {
                out.push('\'');
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    out.push('\'');
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    out.push(' ');
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                }
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

/// Extract the body (without braces) of a fn starting at `marker`.
fn fn_body<'a>(stripped: &'a str, marker: &str) -> &'a str {
    let pos = stripped
        .find(marker)
        .unwrap_or_else(|| panic!("missing fn marker {marker:?}"));
    let open = stripped[pos + marker.len()..]
        .find('{')
        .unwrap_or_else(|| panic!("{marker}: no body brace"))
        + pos
        + marker.len();
    let mut depth = 0i32;
    for (i, byte) in stripped.as_bytes().iter().enumerate().skip(open + 1) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                if depth == 0 {
                    return &stripped[open + 1..i];
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    panic!("{marker}: unterminated body");
}

/// For every `layer_map_for_output` call, the range during which the returned
/// guard is provably alive: from the call site to the closing brace of the
/// block that contains it (the guard is block-scoped).
fn guard_alive_ranges(body: &str) -> Vec<(usize, usize)> {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut calls: Vec<(usize, i32)> = Vec::new();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                while let Some((_, call_depth)) = calls.last() {
                    if *call_depth != depth {
                        break;
                    }
                    let (start, _) = calls.pop().unwrap();
                    ranges.push((start, i));
                }
                depth -= 1;
            }
            _ => {}
        }
        if body[i..].starts_with("layer_map_for_output") {
            calls.push((i, depth));
            i += "layer_map_for_output".len();
            continue;
        }
        i += 1;
    }
    for (start, _) in calls {
        ranges.push((start, body.len()));
    }
    ranges.sort_unstable();
    ranges
}

/// Tokens that must never execute while the layer-map guard is alive in the
/// layer-shell lifecycle paths: renderer/close-animation work, rule
/// resolution, foreign-toplevel rect cleanup, pointer queries (PointerInternal
/// mutex), config mutex, and protocol IPC (STAB-04 phase separation).
const GUARD_FORBIDDEN_TOKENS: &[&str] = &[
    "LayerSurface::new",
    "MappedLayer::new",
    "add_mapped_layer_pre_commit_hook",
    "start_close_animation_for_layer",
    "pointer_location_on_output",
    "clear_foreign_toplevel_rects_for_source",
    "send_scale_transform",
    "send_configure",
    "config.borrow",
];

/// Violations within one fn body: forbidden tokens whose position falls inside
/// a guard-alive range (positions are body-relative).
fn guard_violations(body: &str) -> Vec<(usize, String)> {
    let ranges = guard_alive_ranges(body);
    let mut violations = Vec::new();
    for token in GUARD_FORBIDDEN_TOKENS {
        let mut search_from = 0;
        while let Some(pos) = body[search_from..].find(token) {
            let abs = search_from + pos;
            let inside = ranges
                .iter()
                .any(|(start, end)| abs >= *start && abs < *end);
            if inside {
                violations.push((abs, (*token).to_string()));
            }
            search_from = abs + token.len();
        }
    }
    violations.sort_unstable();
    violations
}

fn assert_guard_discipline(fn_marker: &str) {
    let stripped = strip_comments_and_strings(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/handlers/layer_shell.rs"
    )));
    let body = fn_body(&stripped, fn_marker);
    assert!(
        body.contains("layer_map_for_output"),
        "{fn_marker}: scan must observe the layer-map guard"
    );
    let violations = guard_violations(body);
    let total_forbidden: usize = GUARD_FORBIDDEN_TOKENS
        .iter()
        .map(|token| {
            let mut count = 0;
            let mut search_from = 0;
            while let Some(pos) = body[search_from..].find(token) {
                let abs = search_from + pos;
                let inside = guard_alive_ranges(body)
                    .iter()
                    .any(|(start, end)| abs >= *start && abs < *end);
                if !inside {
                    count += 1;
                }
                search_from = abs + token.len();
            }
            count
        })
        .sum();
    assert!(
        violations.is_empty(),
        "{fn_marker}: layer-map guard crosses forbidden work: {violations:?}"
    );
    assert!(
        total_forbidden > 0,
        "{fn_marker}: scan must observe phase-2 work outside the guard"
    );
}

/// Commit path: MappedLayer construction, rules, pointer query, foreign-rect
/// cleanup, close animation and initial-configure IPC must all run after the
/// guard block closes.
#[test]
fn layer_shell_commit_guard_does_not_cross_phase_two_work() {
    assert_guard_discipline("fn layer_shell_handle_commit");
}

/// Destroy path: mapped-state teardown, Tahoe directive cleanup and the close
/// animation (renderer snapshot) must run outside the guard; only `unmap_layer`
/// stays inside a short map-write phase.
#[test]
fn layer_shell_destroy_guard_does_not_cross_phase_two_work() {
    assert_guard_discipline("fn layer_destroyed");
}

/// New-surface path: LayerSurface construction must not happen while the map
/// guard is held; the guard only wraps the map_layer write.
#[test]
fn new_layer_surface_guard_only_wraps_map_write() {
    assert_guard_discipline("fn new_layer_surface");
}
