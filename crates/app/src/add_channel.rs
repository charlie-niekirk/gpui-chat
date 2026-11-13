//! The "Add channel" dialog (⌘N): one text field, validated with
//! [`crate::login::normalise_login`], with the reason shown under the field when it is rejected.

use std::rc::Rc;

use gpui_chat_core::ChannelId;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div,
};

use crate::login::normalise_login;

/// The dialog body: the input and the validation message.
struct AddChannelForm {
    input: Entity<InputState>,
    error: Option<SharedString>,
}

impl Render for AddChannelForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        v_flex()
            .gap_2()
            .child(div().text_sm().child("Channel name or twitch.tv link"))
            .child(Input::new(&self.input))
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_sm().text_color(danger).child(error)),
            )
    }
}

/// Opens the dialog. `on_add` runs with the channel once the user confirms a valid name.
pub fn open(
    on_add: impl Fn(ChannelId, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let on_add = Rc::new(on_add);
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("forsen"));
    let form = cx.new(|_| AddChannelForm {
        input: input.clone(),
        error: None,
    });
    // The builder runs again whenever the dialog layer rebuilds; focus only on the first run.
    let focus_pending = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_pending.replace(false) {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        let (input, form_for_ok) = (input.clone(), form.clone());
        let on_add = on_add.clone();
        dialog
            .title("Add channel")
            .child(form.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                    .child(DialogAction::new().child(Button::new("add").primary().label("Add"))),
            )
            .on_ok(move |_, window, cx| {
                let value = input.read(cx).value();
                match normalise_login(&value) {
                    Ok(channel) => {
                        on_add(channel, window, cx);
                        true
                    }
                    Err(error) => {
                        form_for_ok.update(cx, |form, cx| {
                            form.error = Some(error.to_string().into());
                            cx.notify();
                        });
                        false
                    }
                }
            })
    });
}
