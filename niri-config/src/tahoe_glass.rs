use std::collections::BTreeMap;

use crate::appearance::{
    BackgroundEffect, BackgroundEffectRule, Color, Shadow, ShadowOffset, ShadowRule,
};
use crate::utils::{FloatOrInt, MergeWith, RegexEq};

#[derive(Debug, Clone, PartialEq)]
pub struct TahoeGlass {
    pub allow_namespaces: Vec<RegexEq>,
    pub materials: BTreeMap<String, TahoeGlassMaterial>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TahoeGlassMaterial {
    pub background_effect: BackgroundEffect,
    pub shadow: Shadow,
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

impl TahoeGlass {
    pub fn material(&self, name: &str) -> TahoeGlassMaterial {
        self.materials
            .get(name)
            .or_else(|| self.materials.get("panel"))
            .copied()
            .unwrap_or_default()
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
            material_profile(
                0.005,
                1.08,
                1.07,
                Color::new_unpremul(237. / 255., 242. / 255., 248. / 255., 1.),
                0.145,
                0.10,
                0.003,
                0.12,
                0.,
                0.,
            ),
        );
        materials.insert(
            "pill".to_owned(),
            material_profile(
                0.005,
                1.09,
                1.04,
                Color::new_unpremul(242. / 255., 246. / 255., 251. / 255., 1.),
                0.115,
                0.24,
                0.009,
                0.10,
                0.,
                0.006,
            ),
        );
        materials.insert(
            "launcher".to_owned(),
            material_profile(
                0.005,
                1.07,
                1.06,
                Color::new_unpremul(237. / 255., 242. / 255., 248. / 255., 1.),
                0.165,
                0.11,
                0.003,
                0.12,
                0.,
                0.001,
            ),
        );
        materials.insert(
            "dock".to_owned(),
            material_profile(
                0.005,
                1.08,
                1.06,
                Color::new_unpremul(237. / 255., 242. / 255., 248. / 255., 1.),
                0.145,
                0.13,
                0.005,
                0.12,
                0.,
                0.003,
            ),
        );
        materials.insert(
            "menu".to_owned(),
            material_profile(
                0.004,
                1.07,
                1.08,
                Color::new_unpremul(241. / 255., 244. / 255., 249. / 255., 1.),
                0.145,
                0.18,
                0.003,
                0.13,
                0.,
                0.,
            ),
        );
        materials.insert(
            "toast".to_owned(),
            material_profile(
                0.005,
                1.08,
                1.08,
                Color::new_unpremul(241. / 255., 244. / 255., 249. / 255., 1.),
                0.130,
                0.18,
                0.004,
                0.12,
                0.,
                0.,
            ),
        );

        let mut backdrop = material_profile(
            0.003,
            1.04,
            1.03,
            Color::new_unpremul(1., 1., 1., 1.),
            0.070,
            0.05,
            0.002,
            0.,
            0.,
            0.,
        );
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
    tint_color: Color,
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
    material.background_effect.tint_color = Some(tint_color);
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

#[cfg(test)]
mod tests {
    use crate::Config;

    use super::TahoeGlass;

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
            Some(0.003)
        );
        assert_eq!(
            config.material("menu").background_effect.chromatic,
            Some(0.)
        );
        assert!(!config.material("backdrop").shadow.on);
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
    }
}
