//! Single editable schema/default source for Tahoe glass materials and blur.
//!
//! Rust `niri-config` owns defaults. Shell settings consume the generated
//! JSON artifact under `generated/glass_schema_defaults.json` — that file is
//! not a second editable schema (CI / unit test enforces no drift).

use crate::appearance::Blur;
use crate::tahoe_glass::{
    TahoeGlass, DEFAULT_BLUR_KERNEL_NAME, GLASS_MATERIAL_NAMES, GLASS_SETTINGS_FIELDS,
};

/// KDL field name → settings/QML key (`edge-highlight` → `edge_highlight`).
fn field_to_key(field: &str) -> String {
    field.replace('-', "_")
}

fn format_f64(value: f64) -> String {
    // Stable decimal formatting for golden/artifact equality.
    if value == 0.0 {
        return "0.0".to_owned();
    }
    let s = format!("{value:.6}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_owned()
    }
}

/// JSON document of compositor-owned defaults (pretty-printed, trailing newline).
///
/// Hand-formatted (no serde in niri-config) so the crate stays lightweight.
pub fn defaults_json() -> String {
    let glass = TahoeGlass::default();
    let blur = Blur::default();

    let mut out = String::from("{\n");
    out.push_str("  \"schema_version\": 1,\n");
    out.push_str(&format!(
        "  \"default_blur_kernel_name\": \"{DEFAULT_BLUR_KERNEL_NAME}\",\n"
    ));

    // material names
    out.push_str("  \"material_names\": [");
    for (i, name) in GLASS_MATERIAL_NAMES.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("\"{name}\""));
    }
    out.push_str("],\n");

    // settings fields (KDL names)
    out.push_str("  \"settings_fields\": [");
    for (i, field) in GLASS_SETTINGS_FIELDS.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("\"{field}\""));
    }
    out.push_str("],\n");

    // blur default kernel
    out.push_str("  \"blur\": {\n");
    out.push_str(&format!(
        "    \"off\": {},\n",
        if blur.off { "true" } else { "false" }
    ));
    out.push_str(&format!("    \"passes\": {},\n", blur.passes));
    out.push_str(&format!("    \"offset\": {},\n", format_f64(blur.offset)));
    out.push_str(&format!("    \"noise\": {},\n", format_f64(blur.noise)));
    out.push_str(&format!(
        "    \"saturation\": {}\n",
        format_f64(blur.saturation)
    ));
    out.push_str("  },\n");

    // materials (settings fields only + full profile for golden)
    out.push_str("  \"materials\": {\n");
    for (mi, name) in GLASS_MATERIAL_NAMES.iter().enumerate() {
        let material = glass.material(name);
        let effect = material.background_effect;
        out.push_str(&format!("    \"{name}\": {{\n"));
        let pairs: [(&str, f64); 5] = [
            ("edge-highlight", effect.edge_highlight.unwrap_or(0.)),
            ("refraction", effect.refraction.unwrap_or(0.)),
            ("inner-shadow", effect.inner_shadow.unwrap_or(0.)),
            ("chromatic", effect.chromatic.unwrap_or(0.)),
            ("lens-depth", effect.lens_depth.unwrap_or(0.)),
        ];
        for (i, (field, value)) in pairs.iter().enumerate() {
            let key = field_to_key(field);
            let comma = if i + 1 < pairs.len() { "," } else { "" };
            out.push_str(&format!("      \"{key}\": {}{comma}\n", format_f64(*value)));
        }
        let trailing = if mi + 1 < GLASS_MATERIAL_NAMES.len() {
            ","
        } else {
            ""
        };
        out.push_str(&format!("    }}{trailing}\n"));
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// Absolute path to the committed artifact (inside this crate).
pub fn artifact_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("generated")
        .join("glass_schema_defaults.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn glass_schema_defaults_artifact_matches_rust_source() {
        let expected = defaults_json();
        let path = artifact_path();

        if std::env::var_os("UPDATE_GLASS_SCHEMA").is_some() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, &expected).unwrap();
            return;
        }

        let on_disk = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "missing glass schema artifact at {}: {err}\n\
                 regenerate with: UPDATE_GLASS_SCHEMA=1 cargo test -p niri-config glass_schema_defaults_artifact",
                path.display()
            )
        });
        assert_eq!(
            on_disk, expected,
            "glass schema artifact drifted from Rust defaults.\n\
             regenerate with: UPDATE_GLASS_SCHEMA=1 cargo test -p niri-config glass_schema_defaults_artifact"
        );
    }

    #[test]
    fn defaults_json_lists_all_materials_and_fields() {
        let json = defaults_json();
        for name in GLASS_MATERIAL_NAMES {
            assert!(json.contains(&format!("\"{name}\"")), "{name}");
        }
        for field in GLASS_SETTINGS_FIELDS {
            let key = field.replace('-', "_");
            assert!(json.contains(&format!("\"{key}\"")), "{key}");
        }
        assert!(json.contains("\"default_blur_kernel_name\": \"default\""));
    }
}
