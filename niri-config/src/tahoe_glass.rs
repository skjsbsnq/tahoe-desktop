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
    #[knuffel(child)]
    pub tint_color: Option<Color>,
    #[knuffel(child, unwrap(argument))]
    pub tint_amount: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub edge_highlight: Option<FloatOrInt<0, 1000>>,
    #[knuffel(child, unwrap(argument))]
    pub refraction: Option<FloatOrInt<0, 1000>>,
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
        let panel = TahoeGlassMaterial::default();
        materials.insert("panel".to_owned(), panel);
        materials.insert("pill".to_owned(), panel);
        materials.insert("dock".to_owned(), panel);
        materials.insert("menu".to_owned(), panel);
        materials.insert("toast".to_owned(), panel);

        let mut backdrop = panel;
        backdrop.shadow.on = false;
        backdrop.background_effect.tint_amount = Some(0.02);
        materials.insert("backdrop".to_owned(), backdrop);

        Self {
            allow_namespaces,
            materials,
        }
    }
}

impl Default for TahoeGlassMaterial {
    fn default() -> Self {
        Self {
            background_effect: BackgroundEffect {
                xray: Some(false),
                blur: Some(true),
                noise: Some(0.006),
                saturation: Some(1.16),
                tint_color: Some(Color::new_unpremul(1., 1., 1., 1.)),
                tint_amount: Some(0.04),
                edge_highlight: Some(0.),
                refraction: Some(0.),
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
            tint_color: part.tint_color,
            tint_amount: part.tint_amount,
            edge_highlight: part.edge_highlight,
            refraction: part.refraction,
        });
        self.shadow.merge_with(&part.shadow);
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

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
                    tint-color "#ffffff"
                    tint-amount 0.04
                    edge-highlight 0.01
                    refraction 0.002

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
        assert_eq!(material.background_effect.edge_highlight, Some(0.01));
        assert_eq!(material.background_effect.refraction, Some(0.002));
        assert!(material.shadow.on);
        assert_eq!(material.shadow.softness, 28.);
    }
}
