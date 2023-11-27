use gpui::{Div, Render};
use story::Story;

use crate::prelude::*;
use crate::Avatar;

pub struct AvatarStory;

impl Render for AvatarStory {
    type Element = Div;

    fn render(&mut self, cx: &mut ViewContext<Self>) -> Self::Element {
        Story::container()
            .child(Story::title_for::<Avatar>())
            .child(Story::label("Default"))
            .child(Avatar::uri(
                "https://avatars.githubusercontent.com/u/1714999?v=4",
            ))
            .child(Avatar::uri(
                "https://avatars.githubusercontent.com/u/326587?v=4",
            ))
    }
}