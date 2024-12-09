use crate::{
    prelude::*,
    utils::{component_preview, component_preview_group},
    Indicator,
};

use component_system::ComponentPreview;
use gpui::{img, AnyElement, Hsla, ImageSource, Img, IntoElement, Styled};
use ui_macros::IntoComponent;

/// An element that renders a user avatar with customizable appearance options.
///
/// # Examples
///
/// ```
/// use ui::{Avatar, AvatarShape};
///
/// Avatar::new("path/to/image.png")
///     .shape(AvatarShape::Circle)
///     .grayscale(true)
///     .border_color(gpui::red());
/// ```
#[derive(IntoElement, IntoComponent)]
#[component(scope = "user")]
pub struct Avatar {
    image: Img,
    size: Option<AbsoluteLength>,
    border_color: Option<Hsla>,
    indicator: Option<AnyElement>,
}

impl Avatar {
    /// Creates a new avatar element with the specified image source.
    pub fn new(src: impl Into<ImageSource>) -> Self {
        Avatar {
            image: img(src),
            size: None,
            border_color: None,
            indicator: None,
        }
    }

    /// Applies a grayscale filter to the avatar image.
    ///
    /// # Examples
    ///
    /// ```
    /// use ui::{Avatar, AvatarShape};
    ///
    /// let avatar = Avatar::new("path/to/image.png").grayscale(true);
    /// ```
    pub fn grayscale(mut self, grayscale: bool) -> Self {
        self.image = self.image.grayscale(grayscale);
        self
    }

    /// Sets the border color of the avatar.
    ///
    /// This might be used to match the border to the background color of
    /// the parent element to create the illusion of cropping another
    /// shape underneath (for example in face piles.)
    pub fn border_color(mut self, color: impl Into<Hsla>) -> Self {
        self.border_color = Some(color.into());
        self
    }

    /// Size overrides the avatar size. By default they are 1rem.
    pub fn size<L: Into<AbsoluteLength>>(mut self, size: impl Into<Option<L>>) -> Self {
        self.size = size.into().map(Into::into);
        self
    }

    /// Sets the current indicator to be displayed on the avatar, if any.
    pub fn indicator<E: IntoElement>(mut self, indicator: impl Into<Option<E>>) -> Self {
        self.indicator = indicator.into().map(IntoElement::into_any_element);
        self
    }
}

impl RenderOnce for Avatar {
    fn render(self, cx: &mut WindowContext) -> impl IntoElement {
        let border_width = if self.border_color.is_some() {
            px(2.)
        } else {
            px(0.)
        };

        let image_size = self.size.unwrap_or_else(|| rems(1.).into());
        let container_size = image_size.to_pixels(cx.rem_size()) + border_width * 2.;

        div()
            .size(container_size)
            .rounded_full()
            .when_some(self.border_color, |this, color| {
                this.border(border_width).border_color(color)
            })
            .child(
                self.image
                    .size(image_size)
                    .rounded_full()
                    .bg(cx.theme().colors().ghost_element_background),
            )
            .children(self.indicator.map(|indicator| div().child(indicator)))
    }
}

impl ComponentPreview for Avatar {
    fn preview(cx: &WindowContext) -> AnyElement {
        let avatar_1 = "https://avatars.githubusercontent.com/u/1789?s=70&v=4";
        let avatar_2 = "https://avatars.githubusercontent.com/u/482957?s=70&v=4";
        let avatar_3 = "https://avatars.githubusercontent.com/u/326587?s=70&v=4";

        v_flex()
            .gap_4()
            .child(
                component_preview_group()
                    .child(component_preview("Default").child(Avatar::new(avatar_1)))
                    .child(
                        component_preview("Custom Size").child(Avatar::new(avatar_2).size(px(48.))),
                    )
                    .child(
                        component_preview("Grayscale").child(Avatar::new(avatar_3).grayscale(true)),
                    ),
            )
            .child(
                component_preview_group()
                    .child(
                        component_preview("With Border")
                            .child(Avatar::new(avatar_1).border_color(cx.theme().colors().border)),
                    )
                    .child(component_preview("With Indicator").child(
                        Avatar::new(avatar_2).indicator(Indicator::dot().color(Color::Success)),
                    )),
            )
            .into_any_element()
    }
}
