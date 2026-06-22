use knuffel::errors::DecodeError;
use knuffel::Decode as _;

use crate::utils::{expect_only_children, parse_arg_node, MergeWith};
use crate::FloatOrInt;

#[derive(Debug, Clone, PartialEq)]
pub struct Animations {
    pub off: bool,
    pub slowdown: f64,
    pub workspace_switch: WorkspaceSwitchAnim,
    pub window_open: WindowOpenAnim,
    pub window_close: WindowCloseAnim,
    pub horizontal_view_movement: HorizontalViewMovementAnim,
    pub window_movement: WindowMovementAnim,
    pub window_resize: WindowResizeAnim,
    pub config_notification_open_close: ConfigNotificationOpenCloseAnim,
    pub exit_confirmation_open_close: ExitConfirmationOpenCloseAnim,
    pub screenshot_ui_open: ScreenshotUiOpenAnim,
    pub overview_open_close: OverviewOpenCloseAnim,
    pub recent_windows_close: RecentWindowsCloseAnim,
}

impl Default for Animations {
    fn default() -> Self {
        Self {
            off: false,
            slowdown: 1.,
            workspace_switch: Default::default(),
            horizontal_view_movement: Default::default(),
            window_movement: Default::default(),
            window_open: Default::default(),
            window_close: Default::default(),
            window_resize: Default::default(),
            config_notification_open_close: Default::default(),
            exit_confirmation_open_close: Default::default(),
            screenshot_ui_open: Default::default(),
            overview_open_close: Default::default(),
            recent_windows_close: Default::default(),
        }
    }
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub struct AnimationsPart {
    #[knuffel(child)]
    pub off: bool,
    #[knuffel(child)]
    pub on: bool,
    #[knuffel(child, unwrap(argument))]
    pub slowdown: Option<FloatOrInt<0, { i32::MAX }>>,
    #[knuffel(child)]
    pub workspace_switch: Option<WorkspaceSwitchAnim>,
    #[knuffel(child)]
    pub window_open: Option<WindowOpenAnim>,
    #[knuffel(child)]
    pub window_close: Option<WindowCloseAnim>,
    #[knuffel(child)]
    pub horizontal_view_movement: Option<HorizontalViewMovementAnim>,
    #[knuffel(child)]
    pub window_movement: Option<WindowMovementAnim>,
    #[knuffel(child)]
    pub window_resize: Option<WindowResizeAnim>,
    #[knuffel(child)]
    pub config_notification_open_close: Option<ConfigNotificationOpenCloseAnim>,
    #[knuffel(child)]
    pub exit_confirmation_open_close: Option<ExitConfirmationOpenCloseAnim>,
    #[knuffel(child)]
    pub screenshot_ui_open: Option<ScreenshotUiOpenAnim>,
    #[knuffel(child)]
    pub overview_open_close: Option<OverviewOpenCloseAnim>,
    #[knuffel(child)]
    pub recent_windows_close: Option<RecentWindowsCloseAnim>,
}

impl MergeWith<AnimationsPart> for Animations {
    fn merge_with(&mut self, part: &AnimationsPart) {
        self.off |= part.off;
        if part.on {
            self.off = false;
        }

        merge!((self, part), slowdown);

        // Animation properties are fairly tied together, except maybe `off`. So let's just save
        // ourselves the work and not merge within individual animations.
        merge_clone!(
            (self, part),
            workspace_switch,
            window_open,
            window_close,
            horizontal_view_movement,
            window_movement,
            window_resize,
            config_notification_open_close,
            exit_confirmation_open_close,
            screenshot_ui_open,
            overview_open_close,
            recent_windows_close,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animation {
    pub off: bool,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Easing(EasingParams),
    Spring(SpringParams),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EasingParams {
    pub duration_ms: u32,
    pub curve: Curve,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    CubicBezier(f64, f64, f64, f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringParams {
    pub damping_ratio: f64,
    pub stiffness: u32,
    pub epsilon: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceSwitchAnim(pub Animation);

impl Default for WorkspaceSwitchAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 1.,
                stiffness: 1000,
                epsilon: 0.0001,
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowOpenAnim {
    pub anim: Animation,
    pub custom_shader: Option<String>,
}

impl Default for WindowOpenAnim {
    fn default() -> Self {
        Self {
            anim: Animation {
                off: false,
                kind: Kind::Easing(EasingParams {
                    duration_ms: 150,
                    curve: Curve::EaseOutExpo,
                }),
            },
            custom_shader: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowCloseAnim {
    pub anim: Animation,
    pub custom_shader: Option<String>,
}

impl Default for WindowCloseAnim {
    fn default() -> Self {
        Self {
            anim: Animation {
                off: false,
                kind: Kind::Easing(EasingParams {
                    duration_ms: 150,
                    curve: Curve::EaseOutQuad,
                }),
            },
            custom_shader: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerOpenAnim {
    pub anim: Animation,
    pub style: LayerOpenAnimationStyle,
    pub scale_from: f64,
    pub opacity_from: f32,
    pub origin: LayerAnimationOrigin,
    pub edge: LayerAnimationEdge,
    pub distance: f64,
}

impl Default for LayerOpenAnim {
    fn default() -> Self {
        Self {
            anim: Animation {
                off: false,
                kind: Kind::Easing(EasingParams {
                    duration_ms: 150,
                    curve: Curve::EaseOutExpo,
                }),
            },
            style: LayerOpenAnimationStyle::Popin,
            scale_from: 0.96,
            opacity_from: 0.,
            origin: LayerAnimationOrigin::Center,
            edge: LayerAnimationEdge::Right,
            distance: 32.,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerCloseAnim {
    pub anim: Animation,
    pub style: LayerCloseAnimationStyle,
    pub scale_to: f64,
    pub opacity_to: f32,
    pub origin: LayerAnimationOrigin,
    pub edge: LayerAnimationEdge,
    pub distance: f64,
}

impl Default for LayerCloseAnim {
    fn default() -> Self {
        Self {
            anim: Animation {
                off: false,
                kind: Kind::Easing(EasingParams {
                    duration_ms: 150,
                    curve: Curve::EaseOutQuad,
                }),
            },
            style: LayerCloseAnimationStyle::Popout,
            scale_to: 0.97,
            opacity_to: 0.,
            origin: LayerAnimationOrigin::Center,
            edge: LayerAnimationEdge::Right,
            distance: 32.,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerOpenAnimationStyle {
    Fade,
    Popin,
    Slide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCloseAnimationStyle {
    Fade,
    Popout,
    Slide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerAnimationOrigin {
    Center,
    Anchor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerAnimationEdge {
    Top,
    Right,
    Bottom,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizontalViewMovementAnim(pub Animation);

impl Default for HorizontalViewMovementAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 1.,
                stiffness: 800,
                epsilon: 0.0001,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowMovementAnim(pub Animation);

impl Default for WindowMovementAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 1.,
                stiffness: 800,
                epsilon: 0.0001,
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowResizeAnim {
    pub anim: Animation,
    pub custom_shader: Option<String>,
}

impl Default for WindowResizeAnim {
    fn default() -> Self {
        Self {
            anim: Animation {
                off: false,
                kind: Kind::Spring(SpringParams {
                    damping_ratio: 1.,
                    stiffness: 800,
                    epsilon: 0.0001,
                }),
            },
            custom_shader: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfigNotificationOpenCloseAnim(pub Animation);

impl Default for ConfigNotificationOpenCloseAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 0.6,
                stiffness: 1000,
                epsilon: 0.001,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExitConfirmationOpenCloseAnim(pub Animation);

impl Default for ExitConfirmationOpenCloseAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 0.6,
                stiffness: 500,
                epsilon: 0.01,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenshotUiOpenAnim(pub Animation);

impl Default for ScreenshotUiOpenAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Easing(EasingParams {
                duration_ms: 200,
                curve: Curve::EaseOutQuad,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverviewOpenCloseAnim(pub Animation);

impl Default for OverviewOpenCloseAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 1.,
                stiffness: 800,
                epsilon: 0.0001,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecentWindowsCloseAnim(pub Animation);

impl Default for RecentWindowsCloseAnim {
    fn default() -> Self {
        Self(Animation {
            off: false,
            kind: Kind::Spring(SpringParams {
                damping_ratio: 1.,
                stiffness: 800,
                epsilon: 0.001,
            }),
        })
    }
}

impl<S> knuffel::Decode<S> for WorkspaceSwitchAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for HorizontalViewMovementAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for WindowMovementAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for WindowOpenAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().anim;
        let mut custom_shader = None;
        let anim = Animation::decode_node(node, ctx, default, |child, ctx| {
            if &**child.node_name == "custom-shader" {
                custom_shader = parse_arg_node("custom-shader", child, ctx)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })?;

        Ok(Self {
            anim,
            custom_shader,
        })
    }
}

impl<S> knuffel::Decode<S> for WindowCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().anim;
        let mut custom_shader = None;
        let anim = Animation::decode_node(node, ctx, default, |child, ctx| {
            if &**child.node_name == "custom-shader" {
                custom_shader = parse_arg_node("custom-shader", child, ctx)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })?;

        Ok(Self {
            anim,
            custom_shader,
        })
    }
}

impl<S> knuffel::Decode<S> for LayerOpenAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default();
        let mut style = None;
        let mut scale_from = None;
        let mut opacity_from = None;
        let mut origin = None;
        let mut edge = None;
        let mut distance = None;

        let anim = Animation::decode_node(node, ctx, default.anim, |child, ctx| {
            match &**child.node_name {
                "style" => {
                    check_duplicate_node(ctx, child, style.is_some(), "style");
                    let value: String = parse_arg_node("style", child, ctx)?;
                    style = Some(parse_layer_open_style(ctx, child, &value));
                    Ok(true)
                }
                "scale-from" => {
                    check_duplicate_node(ctx, child, scale_from.is_some(), "scale-from");
                    let value: FloatOrInt<0, 10> = parse_arg_node("scale-from", child, ctx)?;
                    scale_from = Some(value.0);
                    Ok(true)
                }
                "opacity-from" => {
                    check_duplicate_node(ctx, child, opacity_from.is_some(), "opacity-from");
                    let value: FloatOrInt<0, 1> = parse_arg_node("opacity-from", child, ctx)?;
                    opacity_from = Some(value.0 as f32);
                    Ok(true)
                }
                "origin" => {
                    check_duplicate_node(ctx, child, origin.is_some(), "origin");
                    let value: String = parse_arg_node("origin", child, ctx)?;
                    origin = Some(parse_layer_animation_origin(ctx, child, &value));
                    Ok(true)
                }
                "edge" => {
                    check_duplicate_node(ctx, child, edge.is_some(), "edge");
                    let value: String = parse_arg_node("edge", child, ctx)?;
                    edge = Some(parse_layer_animation_edge(ctx, child, &value));
                    Ok(true)
                }
                "distance" => {
                    check_duplicate_node(ctx, child, distance.is_some(), "distance");
                    let value: FloatOrInt<0, 65535> = parse_arg_node("distance", child, ctx)?;
                    distance = Some(value.0);
                    Ok(true)
                }
                _ => Ok(false),
            }
        })?;

        Ok(Self {
            anim,
            style: style.unwrap_or(default.style),
            scale_from: scale_from.unwrap_or(default.scale_from),
            opacity_from: opacity_from.unwrap_or(default.opacity_from),
            origin: origin.unwrap_or(default.origin),
            edge: edge.unwrap_or(default.edge),
            distance: distance.unwrap_or(default.distance),
        })
    }
}

impl<S> knuffel::Decode<S> for LayerCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default();
        let mut style = None;
        let mut scale_to = None;
        let mut opacity_to = None;
        let mut origin = None;
        let mut edge = None;
        let mut distance = None;

        let anim = Animation::decode_node(node, ctx, default.anim, |child, ctx| {
            match &**child.node_name {
                "style" => {
                    check_duplicate_node(ctx, child, style.is_some(), "style");
                    let value: String = parse_arg_node("style", child, ctx)?;
                    style = Some(parse_layer_close_style(ctx, child, &value));
                    Ok(true)
                }
                "scale-to" => {
                    check_duplicate_node(ctx, child, scale_to.is_some(), "scale-to");
                    let value: FloatOrInt<0, 10> = parse_arg_node("scale-to", child, ctx)?;
                    scale_to = Some(value.0);
                    Ok(true)
                }
                "opacity-to" => {
                    check_duplicate_node(ctx, child, opacity_to.is_some(), "opacity-to");
                    let value: FloatOrInt<0, 1> = parse_arg_node("opacity-to", child, ctx)?;
                    opacity_to = Some(value.0 as f32);
                    Ok(true)
                }
                "origin" => {
                    check_duplicate_node(ctx, child, origin.is_some(), "origin");
                    let value: String = parse_arg_node("origin", child, ctx)?;
                    origin = Some(parse_layer_animation_origin(ctx, child, &value));
                    Ok(true)
                }
                "edge" => {
                    check_duplicate_node(ctx, child, edge.is_some(), "edge");
                    let value: String = parse_arg_node("edge", child, ctx)?;
                    edge = Some(parse_layer_animation_edge(ctx, child, &value));
                    Ok(true)
                }
                "distance" => {
                    check_duplicate_node(ctx, child, distance.is_some(), "distance");
                    let value: FloatOrInt<0, 65535> = parse_arg_node("distance", child, ctx)?;
                    distance = Some(value.0);
                    Ok(true)
                }
                _ => Ok(false),
            }
        })?;

        Ok(Self {
            anim,
            style: style.unwrap_or(default.style),
            scale_to: scale_to.unwrap_or(default.scale_to),
            opacity_to: opacity_to.unwrap_or(default.opacity_to),
            origin: origin.unwrap_or(default.origin),
            edge: edge.unwrap_or(default.edge),
            distance: distance.unwrap_or(default.distance),
        })
    }
}

impl<S> knuffel::Decode<S> for WindowResizeAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().anim;
        let mut custom_shader = None;
        let anim = Animation::decode_node(node, ctx, default, |child, ctx| {
            if &**child.node_name == "custom-shader" {
                custom_shader = parse_arg_node("custom-shader", child, ctx)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })?;

        Ok(Self {
            anim,
            custom_shader,
        })
    }
}

impl<S> knuffel::Decode<S> for ConfigNotificationOpenCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for ExitConfirmationOpenCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for ScreenshotUiOpenAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for OverviewOpenCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

impl<S> knuffel::Decode<S> for RecentWindowsCloseAnim
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let default = Self::default().0;
        Ok(Self(Animation::decode_node(node, ctx, default, |_, _| {
            Ok(false)
        })?))
    }
}

fn check_duplicate_node<S: knuffel::traits::ErrorSpan>(
    ctx: &mut knuffel::decode::Context<S>,
    node: &knuffel::ast::SpannedNode<S>,
    duplicate: bool,
    name: &str,
) {
    if duplicate {
        ctx.emit_error(DecodeError::unexpected(
            &node.node_name,
            "node",
            format!("duplicate node `{name}`, single node expected"),
        ));
    }
}

fn parse_layer_open_style<S: knuffel::traits::ErrorSpan>(
    ctx: &mut knuffel::decode::Context<S>,
    node: &knuffel::ast::SpannedNode<S>,
    value: &str,
) -> LayerOpenAnimationStyle {
    match value {
        "fade" => LayerOpenAnimationStyle::Fade,
        "popin" => LayerOpenAnimationStyle::Popin,
        "slide" => LayerOpenAnimationStyle::Slide,
        unexpected => {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "node",
                format!(
                    "unexpected layer open animation style `{unexpected}`. \
                    Supported styles are `fade`, `popin` and `slide`."
                ),
            ));
            LayerOpenAnimationStyle::Fade
        }
    }
}

fn parse_layer_close_style<S: knuffel::traits::ErrorSpan>(
    ctx: &mut knuffel::decode::Context<S>,
    node: &knuffel::ast::SpannedNode<S>,
    value: &str,
) -> LayerCloseAnimationStyle {
    match value {
        "fade" => LayerCloseAnimationStyle::Fade,
        "popout" => LayerCloseAnimationStyle::Popout,
        "slide" => LayerCloseAnimationStyle::Slide,
        unexpected => {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "node",
                format!(
                    "unexpected layer close animation style `{unexpected}`. \
                    Supported styles are `fade`, `popout` and `slide`."
                ),
            ));
            LayerCloseAnimationStyle::Fade
        }
    }
}

fn parse_layer_animation_origin<S: knuffel::traits::ErrorSpan>(
    ctx: &mut knuffel::decode::Context<S>,
    node: &knuffel::ast::SpannedNode<S>,
    value: &str,
) -> LayerAnimationOrigin {
    match value {
        "center" => LayerAnimationOrigin::Center,
        "anchor" => LayerAnimationOrigin::Anchor,
        unexpected => {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "node",
                format!(
                    "unexpected layer animation origin `{unexpected}`. \
                    Supported origins are `center` and `anchor`."
                ),
            ));
            LayerAnimationOrigin::Center
        }
    }
}

fn parse_layer_animation_edge<S: knuffel::traits::ErrorSpan>(
    ctx: &mut knuffel::decode::Context<S>,
    node: &knuffel::ast::SpannedNode<S>,
    value: &str,
) -> LayerAnimationEdge {
    match value {
        "top" => LayerAnimationEdge::Top,
        "right" => LayerAnimationEdge::Right,
        "bottom" => LayerAnimationEdge::Bottom,
        "left" => LayerAnimationEdge::Left,
        unexpected => {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "node",
                format!(
                    "unexpected layer animation edge `{unexpected}`. \
                    Supported edges are `top`, `right`, `bottom` and `left`."
                ),
            ));
            LayerAnimationEdge::Right
        }
    }
}

impl Animation {
    pub fn new_off() -> Self {
        Self {
            off: true,
            kind: Kind::Easing(EasingParams {
                duration_ms: 0,
                curve: Curve::Linear,
            }),
        }
    }

    fn decode_node<S: knuffel::traits::ErrorSpan>(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
        default: Self,
        mut process_children: impl FnMut(
            &knuffel::ast::SpannedNode<S>,
            &mut knuffel::decode::Context<S>,
        ) -> Result<bool, DecodeError<S>>,
    ) -> Result<Self, DecodeError<S>> {
        #[derive(Default, PartialEq)]
        struct OptionalEasingParams {
            duration_ms: Option<u32>,
            curve: Option<Curve>,
        }

        expect_only_children(node, ctx);

        let mut off = false;
        let mut easing_params = OptionalEasingParams::default();
        let mut spring_params = None;

        for child in node.children() {
            match &**child.node_name {
                "off" => {
                    knuffel::decode::check_flag_node(child, ctx);
                    if off {
                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "node",
                            "duplicate node `off`, single node expected",
                        ));
                    } else {
                        off = true;
                    }
                }
                "spring" => {
                    if easing_params != OptionalEasingParams::default() {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            "cannot set both spring and easing parameters at once",
                        ));
                    }
                    if spring_params.is_some() {
                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "node",
                            "duplicate node `spring`, single node expected",
                        ));
                    }

                    spring_params = Some(SpringParams::decode_node(child, ctx)?);
                }
                "duration-ms" => {
                    if spring_params.is_some() {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            "cannot set both spring and easing parameters at once",
                        ));
                    }
                    if easing_params.duration_ms.is_some() {
                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "node",
                            "duplicate node `duration-ms`, single node expected",
                        ));
                    }

                    easing_params.duration_ms = Some(parse_arg_node("duration-ms", child, ctx)?);
                }
                "curve" => {
                    if spring_params.is_some() {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            "cannot set both spring and easing parameters at once",
                        ));
                    }
                    if easing_params.curve.is_some() {
                        ctx.emit_error(DecodeError::unexpected(
                            &child.node_name,
                            "node",
                            "duplicate node `curve`, single node expected",
                        ));
                    }

                    let mut iter_args = child.arguments.iter();
                    let val = iter_args.next().ok_or_else(|| {
                        DecodeError::missing(child, "additional argument `curve` is required")
                    })?;
                    let animation_curve_string: String =
                        knuffel::traits::DecodeScalar::decode(val, ctx)?;

                    let animation_curve = match animation_curve_string.as_str() {
                        "linear" => Some(Curve::Linear),
                        "ease-out-quad" => Some(Curve::EaseOutQuad),
                        "ease-out-cubic" => Some(Curve::EaseOutCubic),
                        "ease-out-expo" => Some(Curve::EaseOutExpo),
                        "emphasized-decel" => Some(Curve::CubicBezier(0.05, 0.7, 0.1, 1.)),
                        "emphasized-accel" => Some(Curve::CubicBezier(0.3, 0., 0.8, 0.15)),
                        "menu-decel" => Some(Curve::CubicBezier(0.1, 1., 0., 1.)),
                        "menu-accel" => Some(Curve::CubicBezier(0.52, 0.03, 0.72, 0.08)),
                        "stall" => Some(Curve::CubicBezier(1., -0.1, 0.7, 0.85)),
                        "cubic-bezier" => {
                            let val = iter_args.next().ok_or_else(|| {
                                DecodeError::missing(
                                    child,
                                    "missing x1 coordinate for cubic Bézier curve control point",
                                )
                            })?;
                            // the X axis represents time frame so it cannot be negative
                            // or larger than 1
                            let x1: FloatOrInt<0, 1> =
                                knuffel::traits::DecodeScalar::decode(val, ctx)?;
                            let val = iter_args.next().ok_or_else(|| {
                                DecodeError::missing(
                                    child,
                                    "missing y1 coordinate for cubic Bézier curve control point",
                                )
                            })?;
                            let y1: FloatOrInt<{ i32::MIN }, { i32::MAX }> =
                                knuffel::traits::DecodeScalar::decode(val, ctx)?;
                            let val = iter_args.next().ok_or_else(|| {
                                DecodeError::missing(
                                    child,
                                    "missing x2 coordinate for cubic Bézier curve control point",
                                )
                            })?;
                            let x2: FloatOrInt<0, 1> =
                                knuffel::traits::DecodeScalar::decode(val, ctx)?;
                            let val = iter_args.next().ok_or_else(|| {
                                DecodeError::missing(
                                    child,
                                    "missing y2 coordinate for cubic Bézier curve control point",
                                )
                            })?;
                            let y2: FloatOrInt<{ i32::MIN }, { i32::MAX }> =
                                knuffel::traits::DecodeScalar::decode(val, ctx)?;

                            Some(Curve::CubicBezier(x1.0, y1.0, x2.0, y2.0))
                        }
                        unexpected_curve => {
                            ctx.emit_error(DecodeError::unexpected(
                                &val.literal,
                                "argument",
                                format!(
                                    "unexpected animation curve `{unexpected_curve}`. \
                                    Niri only supports these animation curves: \
                                    `ease-out-quad`, `ease-out-cubic`, `ease-out-expo`, `linear`, \
                                    `emphasized-decel`, `emphasized-accel`, `menu-decel`, \
                                    `menu-accel`, `stall` and `cubic-bezier`."
                                ),
                            ));

                            None
                        }
                    };

                    if let Some(val) = iter_args.next() {
                        ctx.emit_error(DecodeError::unexpected(
                            &val.literal,
                            "argument",
                            "unexpected argument",
                        ));
                    }
                    for name in child.properties.keys() {
                        ctx.emit_error(DecodeError::unexpected(
                            name,
                            "property",
                            format!("unexpected property `{}`", name.escape_default()),
                        ));
                    }
                    for child in child.children() {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            format!("unexpected node `{}`", child.node_name.escape_default()),
                        ));
                    }

                    easing_params.curve = animation_curve;
                }
                name_str => {
                    if !process_children(child, ctx)? {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            format!("unexpected node `{}`", name_str.escape_default()),
                        ));
                    }
                }
            }
        }

        let kind = if let Some(spring_params) = spring_params {
            // Configured spring.
            Kind::Spring(spring_params)
        } else if easing_params == OptionalEasingParams::default() {
            // Did not configure anything.
            default.kind
        } else {
            // Configured easing.
            let default = if let Kind::Easing(easing) = default.kind {
                easing
            } else {
                // Generic fallback values for when the default animation is spring, but the user
                // configured an easing animation.
                EasingParams {
                    duration_ms: 250,
                    curve: Curve::EaseOutCubic,
                }
            };

            Kind::Easing(EasingParams {
                duration_ms: easing_params.duration_ms.unwrap_or(default.duration_ms),
                curve: easing_params.curve.unwrap_or(default.curve),
            })
        };

        Ok(Self { off, kind })
    }
}

impl<S> knuffel::Decode<S> for SpringParams
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        if let Some(type_name) = &node.type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for this node",
            ));
        }
        if let Some(val) = node.arguments.first() {
            ctx.emit_error(DecodeError::unexpected(
                &val.literal,
                "argument",
                "unexpected argument",
            ));
        }
        for child in node.children() {
            ctx.emit_error(DecodeError::unexpected(
                child,
                "node",
                format!("unexpected node `{}`", child.node_name.escape_default()),
            ));
        }

        let mut damping_ratio = None;
        let mut stiffness = None;
        let mut epsilon = None;
        for (name, val) in &node.properties {
            match &***name {
                "damping-ratio" => {
                    damping_ratio = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);
                }
                "stiffness" => {
                    stiffness = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);
                }
                "epsilon" => {
                    epsilon = Some(knuffel::traits::DecodeScalar::decode(val, ctx)?);
                }
                name_str => {
                    ctx.emit_error(DecodeError::unexpected(
                        name,
                        "property",
                        format!("unexpected property `{}`", name_str.escape_default()),
                    ));
                }
            }
        }
        let damping_ratio = damping_ratio
            .ok_or_else(|| DecodeError::missing(node, "property `damping-ratio` is required"))?;
        let stiffness = stiffness
            .ok_or_else(|| DecodeError::missing(node, "property `stiffness` is required"))?;
        let epsilon =
            epsilon.ok_or_else(|| DecodeError::missing(node, "property `epsilon` is required"))?;

        if !(0.1..=10.).contains(&damping_ratio) {
            ctx.emit_error(DecodeError::conversion(
                node,
                "damping-ratio must be between 0.1 and 10.0",
            ));
        }
        if stiffness < 1 {
            ctx.emit_error(DecodeError::conversion(node, "stiffness must be >= 1"));
        }
        if !(0.00001..=0.1).contains(&epsilon) {
            ctx.emit_error(DecodeError::conversion(
                node,
                "epsilon must be between 0.00001 and 0.1",
            ));
        }

        Ok(SpringParams {
            damping_ratio,
            stiffness,
            epsilon,
        })
    }
}
