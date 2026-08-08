use std::collections::BTreeMap;

use crate::appearance::{
    BackgroundEffect, BackgroundEffectRule, Blur, Color, Shadow, ShadowOffset, ShadowRule,
};
use crate::utils::{FloatOrInt, MergeWith, RegexEq};

/// Reserved name of the kernel owned by the top-level `blur { }` block.
pub const DEFAULT_BLUR_KERNEL_NAME: &str = "default";

/// Built-in Tahoe glass material names (stable vocabulary for Shell and config).
pub const GLASS_MATERIAL_NAMES: &[&str] = &[
    "panel", "pill", "launcher", "dock", "menu", "toast", "backdrop",
];

/// Settings-tool editable material fields (KDL leaf names).
pub const GLASS_SETTINGS_FIELDS: &[&str] = &[
    "edge-highlight",
    "refraction",
    "inner-shadow",
    "chromatic",
    "lens-depth",
];

#[derive(Debug, Clone, PartialEq)]
pub struct TahoeGlass {
    pub allow_namespaces: Vec<RegexEq>,
    pub materials: BTreeMap<String, TahoeGlassMaterial>,
}

/// Config-time material with a fully resolved blur kernel after
/// [`Config::resolve_named_blur_kernels`](crate::Config::resolve_named_blur_kernels).
///
/// Inheritance (kernel name → kernel body, material field merges) happens only
/// during config parse. Render paths must not re-merge a global blur onto this.
#[derive(Debug, Clone, PartialEq)]
pub struct TahoeGlassMaterial {
    pub background_effect: BackgroundEffect,
    pub shadow: Shadow,
    /// Named kernel reference from KDL (`blur-kernel "name"`). `None` or
    /// `"default"` selects the top-level `blur { }` block.
    pub kernel_name: Option<String>,
    /// Fully resolved kernel body (parse-time only).
    pub kernel: Blur,
}

/// Immutable resolved glass material for render / golden tests.
///
/// Built only from parse-time resolution; not a second schema.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedGlassMaterial {
    pub kernel: Blur,
    pub effect: BackgroundEffect,
    pub shadow: Shadow,
}

impl From<&TahoeGlassMaterial> for ResolvedGlassMaterial {
    fn from(material: &TahoeGlassMaterial) -> Self {
        Self {
            kernel: material.kernel,
            effect: material.background_effect,
            shadow: material.shadow,
        }
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct TahoeGlassPart {
    #[knuffel(children(name = "allow-namespace"))]
    pub allow_namespaces: Vec<TahoeGlassAllowNamespace>,
    #[knuffel(children(name = "material"))]
    pub materials: Vec<TahoeGlassMaterialRule>,
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub struct TahoeGlassAllowNamespace(#[knuffel(argument, str)] pub RegexEq);

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct TahoeGlassMaterialRule {
    #[knuffel(argument, str)]
    pub name: String,
    /// Optional named blur kernel (`blur-kernel "soft"`). Omitted → default.
    #[knuffel(child, unwrap(argument, str))]
    pub blur_kernel: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub xray: Option<bool>,
    #[knuffel(child, unwrap(argument))]
    pub blur: Option<bool>,
    #[knuffel(child, unwrap(argument))]
    pub noise: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub saturation: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub contrast: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child)]
    pub tint_color: Option<Color>,
    #[knuffel(child, unwrap(argument))]
    pub tint_amount: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub edge_highlight: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub refraction: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub inner_shadow: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub chromatic: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub lens_depth: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, default)]
    pub shadow: ShadowRule,
}

/// Top-level `blur-kernel "name" { ... }` part (body matches `blur { }`).
#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct BlurKernelPart {
    #[knuffel(argument, str)]
    pub name: String,
    #[knuffel(child)]
    pub off: bool,
    #[knuffel(child)]
    pub on: bool,
    #[knuffel(child, unwrap(argument))]
    pub passes: Option<u8>,
    #[knuffel(child, unwrap(argument))]
    pub offset: Option<FloatOrInt<0, 100>>,
    #[knuffel(child, unwrap(argument))]
    pub noise: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub saturation: Option<FloatOrInt<0, 1000>>,
}

impl BlurKernelPart {
    pub fn to_blur_part(&self) -> crate::appearance::BlurPart {
        crate::appearance::BlurPart {
            off: self.off,
            on: self.on,
            passes: self.passes,
            offset: self.offset,
            noise: self.noise,
            saturation: self.saturation,
        }
    }
}

impl TahoeGlass {
    pub fn material(&self, name: &str) -> TahoeGlassMaterial {
        self.materials
            .get(name)
            .or_else(|| self.materials.get("panel"))
            .cloned()
            .unwrap_or_default()
    }

    /// Fully resolved material for render/golden (kernel already bound).
    pub fn resolved(&self, name: &str) -> ResolvedGlassMaterial {
        ResolvedGlassMaterial::from(&self.material(name))
    }

    pub fn namespace_allowed(&self, namespace: &str) -> bool {
        self.allow_namespaces
            .iter()
            .any(|regex| regex.0.is_match(namespace))
    }
}

impl Default for TahoeGlass {
    fn default() -> Self {
        let allow_namespaces = vec!["^tahoe-".parse().unwrap()];

        let mut materials = BTreeMap::new();
        materials.insert(
            "panel".to_owned(),
            material_profile(0.005, 1.35, 1.0, 0.12, 0.45, 0.012, 0.06, 0., 0.),
        );
        materials.insert(
            "pill".to_owned(),
            material_profile(0.005, 1.45, 1.0, 0.10, 0.25, 0.012, 0.07, 0., 0.010),
        );
        materials.insert(
            "launcher".to_owned(),
            material_profile(0.005, 1.35, 1.0, 0.12, 0.45, 0.012, 0.055, 0., 0.003),
        );
        materials.insert(
            "dock".to_owned(),
            material_profile(0.005, 1.40, 1.0, 0.14, 0.35, 0.016, 0.07, 0., 0.006),
        );
        materials.insert(
            "menu".to_owned(),
            material_profile(0.004, 1.40, 1.0, 0.10, 0.55, 0.014, 0.10, 0., 0.),
        );
        materials.insert(
            "toast".to_owned(),
            material_profile(0.005, 1.40, 1.0, 0.10, 0.50, 0.014, 0.09, 0., 0.),
        );

        let mut backdrop = material_profile(0.003, 1.25, 1.0, 0.10, 0.25, 0.006, 0., 0., 0.);
        backdrop.shadow.on = false;
        materials.insert("backdrop".to_owned(), backdrop);

        Self {
            allow_namespaces,
            materials,
        }
    }
}

fn material_profile(
    noise: f64,
    saturation: f64,
    contrast: f64,
    tint_amount: f64,
    edge_highlight: f64,
    refraction: f64,
    inner_shadow: f64,
    chromatic: f64,
    lens_depth: f64,
) -> TahoeGlassMaterial {
    let mut material = TahoeGlassMaterial::default();
    material.background_effect.noise = Some(noise);
    material.background_effect.saturation = Some(saturation);
    material.background_effect.contrast = Some(contrast);
    material.background_effect.tint_amount = Some(tint_amount);
    material.background_effect.edge_highlight = Some(edge_highlight);
    material.background_effect.refraction = Some(refraction);
    material.background_effect.inner_shadow = Some(inner_shadow);
    material.background_effect.chromatic = Some(chromatic);
    material.background_effect.lens_depth = Some(lens_depth);
    material
}

impl Default for TahoeGlassMaterial {
    fn default() -> Self {
        Self {
            background_effect: BackgroundEffect {
                xray: Some(false),
                blur: Some(true),
                noise: Some(0.006),
                saturation: Some(1.16),
                contrast: Some(1.0),
                tint_color: Some(Color::new_unpremul(1., 1., 1., 1.)),
                tint_amount: Some(0.04),
                edge_highlight: Some(0.),
                refraction: Some(0.),
                inner_shadow: Some(0.),
                chromatic: Some(0.),
                lens_depth: Some(0.),
                ..Default::default()
            },
            shadow: Shadow {
                on: true,
                offset: ShadowOffset {
                    x: FloatOrInt(0.),
                    y: FloatOrInt(8.),
                },
                softness: 28.,
                spread: 2.,
                color: Color::new_unpremul(0., 0., 0., 0.27),
                ..Default::default()
            },
            kernel_name: None,
            kernel: Blur::default(),
        }
    }
}

impl MergeWith<TahoeGlassPart> for TahoeGlass {
    fn merge_with(&mut self, part: &TahoeGlassPart) {
        if !part.allow_namespaces.is_empty() {
            self.allow_namespaces = part
                .allow_namespaces
                .iter()
                .map(|namespace| namespace.0.clone())
                .collect();
        }

        for material in &part.materials {
            self.materials
                .entry(material.name.clone())
                .or_default()
                .merge_with(material);
        }
    }
}

impl MergeWith<TahoeGlassMaterialRule> for TahoeGlassMaterial {
    fn merge_with(&mut self, part: &TahoeGlassMaterialRule) {
        if let Some(name) = &part.blur_kernel {
            self.kernel_name = Some(name.clone());
        }
        self.background_effect.merge_with(&BackgroundEffectRule {
            xray: part.xray,
            blur: part.blur,
            noise: part.noise,
            saturation: part.saturation,
            contrast: part.contrast,
            tint_color: part.tint_color,
            tint_amount: part.tint_amount,
            edge_highlight: part.edge_highlight,
            refraction: part.refraction,
            inner_shadow: part.inner_shadow,
            chromatic: part.chromatic,
            lens_depth: part.lens_depth,
        });
        self.shadow.merge_with(&part.shadow);
    }
}

/// Look up a named kernel: reserved `default` → top-level blur; else named map.
pub fn lookup_blur_kernel<'a>(
    name: &str,
    default_kernel: Blur,
    named: &'a BTreeMap<String, Blur>,
) -> Result<Blur, String> {
    if name == DEFAULT_BLUR_KERNEL_NAME {
        return Ok(default_kernel);
    }
    named.get(name).copied().ok_or_else(|| {
        format!(
            "unknown blur-kernel `{name}` (define `blur-kernel \"{name}\" {{ ... }}` or use `{DEFAULT_BLUR_KERNEL_NAME}`)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn default_materials_match_shell_vocabulary() {
        let config = TahoeGlass::default();
        let names: Vec<_> = config.materials.keys().map(String::as_str).collect();

        assert_eq!(
            names,
            vec!["backdrop", "dock", "launcher", "menu", "panel", "pill", "toast"]
        );
        assert_eq!(
            config.material("launcher").background_effect.refraction,
            Some(0.004)
        );
        assert_eq!(
            config.material("menu").background_effect.chromatic,
            Some(0.)
        );
        assert!(!config.material("backdrop").shadow.on);

        for material in [
            "panel", "pill", "launcher", "dock", "menu", "toast", "backdrop",
        ] {
            assert_eq!(
                config.material(material).background_effect.xray,
                Some(false),
                "{material} should sample the live composed framebuffer"
            );
            assert_eq!(
                config.material(material).kernel,
                Blur::default(),
                "{material} default kernel matches Blur::default before resolve"
            );
        }
    }

    #[test]
    fn parse_tahoe_glass_material() {
        let config = Config::parse_mem(
            r##"
            tahoe-glass {
                allow-namespace "^tahoe-"

                material "panel" {
                    blur true
                    noise 0.006
                    saturation 1.16
                    contrast 1.08
                    tint-color "#ffffff"
                    tint-amount 0.04
                    edge-highlight 0.01
                    refraction 0.002
                    inner-shadow 0.12
                    chromatic 0.006
                    lens-depth 0.04

                    shadow {
                        on
                        softness 28
                        spread 2
                        offset x=0 y=8
                        color "#0004"
                    }
                }
            }
            "##,
        )
        .unwrap();

        let material = config.tahoe_glass.material("panel");
        assert_eq!(material.background_effect.xray, Some(false));
        assert_eq!(material.background_effect.blur, Some(true));
        assert_eq!(material.background_effect.noise, Some(0.006));
        assert_eq!(material.background_effect.contrast, Some(1.08));
        assert_eq!(material.background_effect.edge_highlight, Some(0.01));
        assert_eq!(material.background_effect.refraction, Some(0.002));
        assert_eq!(material.background_effect.inner_shadow, Some(0.12));
        assert_eq!(material.background_effect.chromatic, Some(0.006));
        assert_eq!(material.background_effect.lens_depth, Some(0.04));
        assert!(material.shadow.on);
        assert_eq!(material.shadow.softness, 28.);
        // Unreferenced materials use default kernel = top-level blur (defaults).
        assert_eq!(material.kernel, config.blur);
    }

    #[test]
    fn named_kernel_isolates_dock_from_panel() {
        let config = Config::parse_mem(
            r##"
            blur {
                passes 4
                offset 4
                noise 0.004
                saturation 1.22
            }

            blur-kernel "dock-soft" {
                passes 1
                offset 1
                noise 0.001
                saturation 1.0
            }

            tahoe-glass {
                material "dock" {
                    blur-kernel "dock-soft"
                    edge-highlight 0.18
                }
                material "panel" {
                    edge-highlight 0.14
                }
            }
            "##,
        )
        .unwrap();

        let dock = config.tahoe_glass.resolved("dock");
        let panel = config.tahoe_glass.resolved("panel");
        let toast = config.tahoe_glass.resolved("toast");

        assert_eq!(dock.kernel.passes, 1);
        assert_eq!(dock.kernel.offset, 1.0);
        assert_eq!(panel.kernel.passes, 4);
        assert_eq!(panel.kernel.offset, 4.0);
        assert_eq!(toast.kernel, panel.kernel);
        assert_ne!(dock.kernel, panel.kernel);
    }

    #[test]
    fn old_blur_only_config_binds_all_materials_to_default_kernel() {
        let config = Config::parse_mem(
            r##"
            blur {
                passes 7
                offset 9
                noise 0.01
                saturation 1.3
            }
            "##,
        )
        .unwrap();

        for name in GLASS_MATERIAL_NAMES {
            let resolved = config.tahoe_glass.resolved(name);
            assert_eq!(resolved.kernel, config.blur, "{name}");
            assert_eq!(resolved.kernel.passes, 7);
        }
    }

    #[test]
    fn unknown_kernel_reference_is_config_error() {
        let err = Config::parse_mem(
            r##"
            tahoe-glass {
                material "dock" {
                    blur-kernel "does-not-exist"
                }
            }
            "##,
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("does-not-exist") || format!("{err}").contains("does-not-exist"),
            "error should mention unknown kernel, got {err}"
        );
    }

    #[test]
    fn reserved_default_kernel_name_cannot_be_redefined() {
        let err = Config::parse_mem(
            r##"
            blur-kernel "default" {
                passes 2
            }
            "##,
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("default") || format!("{err:?}").contains("default"),
            "error should reject reserved name, got {err}"
        );
    }

    #[test]
    fn explicit_default_kernel_name_on_material_uses_top_level_blur() {
        let config = Config::parse_mem(
            r##"
            blur {
                passes 5
            }
            tahoe-glass {
                material "panel" {
                    blur-kernel "default"
                }
            }
            "##,
        )
        .unwrap();
        assert_eq!(config.tahoe_glass.resolved("panel").kernel.passes, 5);
    }
}
