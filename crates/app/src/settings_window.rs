//! The settings window (⌘,): a UI over the same `settings.toml` the file watcher already follows.
//!
//! The file stays the source of truth. Every control writes through [`config::edit`], which
//! changes just that key in the file (comments and unknown keys stay) and refuses to write
//! anything that would leave the file invalid. The controls read the settings store, which the
//! watcher keeps current, so editing the file by hand updates the open window; and a file that
//! does not load is shown in a banner (and left alone) while the controls keep the last good
//! values.
//!
//! One window: opening it again focuses the existing one.

mod fields;

use std::rc::Rc;

use gpui_chat_settings::{
    DeletedMessages, EMOTE_SCALE_RANGE, EditValue, FONT_SIZE_RANGE, LINE_HEIGHT_RANGE,
    SCROLLBACK_RANGE, SettingEdit, Settings, SettingsError, ThemeMode,
};
use gpui_chat_twitch::auth::AuthState;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsPanes,
};
use gpui_kit::component::{Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, FocusHandle, Focusable, FontWeight, Global,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, WindowBounds, WindowOptions, div, px, size,
};

use self::fields::{Commit, NumberSpec, Reset};
use crate::account::{self, AccountHub};
use crate::actions::{CloseWindow, LogIn, LogOut, MinimizeWindow, OpenSettings};
use crate::fonts::FontConfig;
use crate::theme::{self, Tokens};
use crate::{config, sign_in};

/// Opening size; the window can be resized down to [`MIN_SIZE`].
const DEFAULT_SIZE: (f32, f32) = (880.0, 640.0);
const MIN_SIZE: (f32, f32) = (640.0, 440.0);

/// Key context of the window, for key bindings that apply only inside it.
pub const CONTEXT: &str = "SettingsWindow";

/// The open settings window, kept so a second ⌘, focuses it.
struct OpenWindow(AnyWindowHandle);

impl Global for OpenWindow {}

/// The settings view of the open window, so a write that has no handle to it (a page's reset
/// button) can still put a refusal in the banner.
struct ActiveView(WeakEntity<SettingsView>);

impl Global for ActiveView {}

/// Writes `edits` for the open settings window (see [`SettingsView::write`]).
pub fn write(edits: Vec<SettingEdit>, cx: &mut App) {
    if let Some(view) = cx.try_global::<ActiveView>().map(|view| view.0.clone()) {
        SettingsView::write(&view, edits, cx);
    }
}

/// Opens the settings window, or brings the open one to the front.
pub fn open(cx: &mut App) {
    if let Some(handle) = cx.try_global::<OpenWindow>().map(|open| open.0) {
        // A closed window is gone from the list; then a new one is opened below. (Updating
        // fails for a window that is being updated right now, e.g. when ⌘, is pressed inside
        // it; the window's own handler for the action covers that case.)
        if cx.windows().contains(&handle) {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            return;
        }
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
            cx,
        )),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        ..TitleBar::window_options()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| SettingsView::new(window, cx))
    }) {
        Ok((handle, _)) => cx.set_global(OpenWindow(handle)),
        Err(error) => tracing::warn!(%error, "could not open the settings window"),
    }
}

/// Whether the settings window is the only window left (or there is none): the app quits when
/// the last *chat* window closes, not when only settings remain.
pub fn only_settings_left(cx: &App) -> bool {
    let settings = cx.try_global::<OpenWindow>().map(|open| open.0);
    cx.windows()
        .into_iter()
        .all(|window| Some(window) == settings)
}

pub struct SettingsView {
    focus_handle: FocusHandle,
    /// Why the last change from a toggle or menu was not saved. Cleared when the settings
    /// change (the file loaded, or a write worked).
    edit_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("Settings");
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let this = cx.entity().downgrade();
        cx.set_global(ActiveView(this));
        // Text typed into a field but not yet committed is committed when the window closes
        // (the close button) or the app quits; ⌘W does the same in its action handler.
        window.on_window_should_close(cx, |_, cx| {
            fields::flush_all(cx);
            true
        });
        let mut subscriptions = vec![
            cx.observe(&config::store(cx), |this, _, cx| {
                this.edit_error = None;
                cx.notify();
            }),
            // The theme and font follow the settings: redraw in the new colours. A theme file
            // added or removed re-applies the theme too, which refreshes the theme list.
            cx.observe_global::<Tokens>(|_, cx| cx.notify()),
            cx.observe_global::<FontConfig>(|_, cx| cx.notify()),
            cx.on_app_quit(|_, cx| {
                fields::flush_all(cx);
                async {}
            }),
        ];
        if let Some(hub) = account::current(cx) {
            subscriptions.push(cx.observe(&hub, |_, _, cx| cx.notify()));
        }
        Self {
            focus_handle,
            edit_error: None,
            _subscriptions: subscriptions,
        }
    }

    /// Writes `edits` for a control that has no room to show an error (a switch, a menu): the
    /// reason goes in the banner instead.
    fn write(this: &WeakEntity<Self>, edits: Vec<SettingEdit>, cx: &mut App) {
        let result = config::edit(&edits, cx);
        this.update(cx, |view, cx| {
            view.edit_error = result
                .err()
                .map(|error| SharedString::from(format!("Not saved: {}", friendly(&error))));
            cx.notify();
        })
        .ok();
    }

    fn banner(&self, tokens: &Tokens, cx: &App) -> Option<impl IntoElement + use<>> {
        let load_error = config::store(cx).read(cx).error().map(ToString::to_string);
        if load_error.is_none() && self.edit_error.is_none() {
            return None;
        }
        let danger = tokens.danger;
        let (headline, detail) = match (&load_error, &self.edit_error) {
            (Some(error), edit_error) => (
                "settings.toml has a problem. It has not been changed.",
                format!(
                    "{error}\nThe values below are the last good ones. Fix the file, or change \
                     the setting it names here.{}",
                    if edit_error.is_some() {
                        "\nYour last change was not saved."
                    } else {
                        ""
                    }
                ),
            ),
            (None, Some(error)) => ("The change was not saved.", error.to_string()),
            (None, None) => unreachable!("checked above"),
        };
        Some(
            v_flex()
                .id("settings-banner")
                .w_full()
                .flex_none()
                .px_3()
                .py_2()
                .gap_1()
                .bg(danger.opacity(0.14))
                .border_b_1()
                .border_color(danger)
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(tokens.danger_text)
                        .child(headline),
                )
                .child(div().text_sm().child(detail)),
        )
    }

    fn pages(&self, cx: &mut Context<Self>) -> Vec<SettingPage> {
        let this = cx.entity().downgrade();
        vec![
            appearance_page(&this, cx),
            chat_page(&this),
            emotes_page(&this),
            streamlink_page(),
            account_page(),
        ]
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let font = cx.global::<FontConfig>().clone();
        let banner = self.banner(&tokens, cx);
        let location = config::file(cx).map(|path| path.display().to_string());
        let pages = self.pages(cx);
        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &CloseWindow, window, cx| {
                fields::flush_all(cx);
                window.remove_window();
            }))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            // ⌘, inside the settings window: it is already open, so just keep it in front.
            .on_action(cx.listener(|_, _: &OpenSettings, window, _| window.activate_window()))
            // The same actions as the main window's menu and title bar.
            .on_action(cx.listener(|_, _: &LogIn, window, cx| {
                if let Some(hub) = account::current(cx) {
                    sign_in::open(hub, window, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &LogOut, _, cx| {
                if let Some(hub) = account::current(cx) {
                    hub.update(cx, |hub, cx| hub.log_out(cx));
                }
            }))
            .size_full()
            .bg(tokens.window_bg)
            .text_color(tokens.text)
            .font_family(font.family)
            .text_size(px(14.0))
            .child(TitleBar::new().child(div().text_sm().child("Settings")))
            .children(banner)
            .child(
                div().flex_1().min_h_0().w_full().child(
                    SettingsPanes::new("settings")
                        .sidebar_width(px(210.0))
                        .pages(pages),
                ),
            )
            .children(location.map(|path| {
                div()
                    .flex_none()
                    .w_full()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(tokens.border)
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .truncate()
                    .child(format!("Saved to {path}. You can also edit it by hand."))
            }))
    }
}

// ---- pages ----

/// What a refused write says next to the control: the setting and the reason, without the path
/// (the footer and the banner already name the file).
fn friendly(error: &SettingsError) -> String {
    match error {
        SettingsError::Invalid { field, reason, .. } => format!("`{field}`: {reason}"),
        SettingsError::Parse {
            message,
            line: Some(line),
            ..
        } => format!("line {line}: {message}"),
        SettingsError::Parse { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

fn current(cx: &App) -> &Settings {
    config::current(cx)
}

/// A field's page-reset hook: dirty when the setting differs from its default.
fn reset<T: PartialEq + 'static>(field: &'static str, get: fn(&Settings) -> T) -> Reset {
    Reset {
        field,
        is_dirty: Rc::new(move |cx| get(current(cx)) != get(&Settings::default())),
    }
}

/// A float setting as the user types it (`1.4`, not the `f64` widening of the `f32`).
fn float_text(value: f32) -> String {
    value.to_string()
}

/// A commit for an optional text setting: empty removes the key.
fn optional_text(field: &'static str) -> Commit<String> {
    Rc::new(move |text: String, cx: &mut App| {
        let edit = if text.is_empty() {
            SettingEdit::remove(field)
        } else {
            SettingEdit::set(field, EditValue::Str(text))
        };
        config::edit(&[edit], cx).map_err(|error| friendly(&error))
    })
}

fn appearance_page(this: &WeakEntity<SettingsView>, cx: &App) -> SettingPage {
    let themes = theme::choices(cx);
    let theme_options: Vec<(SharedString, SharedString)> = themes
        .iter()
        .map(|theme| {
            let label = if theme.user {
                format!("{} (custom)", theme.name)
            } else {
                theme.name.clone()
            };
            (
                SharedString::from(theme.id.clone()),
                SharedString::from(label),
            )
        })
        .collect();
    let mut light_options = vec![(SharedString::default(), SharedString::from("Same as Theme"))];
    light_options.extend(theme_options.iter().cloned());

    let font_in_use = cx.global::<FontConfig>().family.to_string();
    let font = SettingGroup::new().title("Font").items([
        SettingItem::new(
            "Font family",
            fields::text(
                "font-family",
                "Font family",
                "JetBrainsMono Nerd Font",
                |cx| current(cx).appearance.font_family.clone(),
                Rc::new(|text: String, cx: &mut App| {
                    if text.is_empty() {
                        return Err("A font family is required.".to_owned());
                    }
                    config::edit(
                        &[SettingEdit::set(
                            "appearance.font_family",
                            EditValue::Str(text),
                        )],
                        cx,
                    )
                    .map_err(|error| friendly(&error))
                }),
                reset("appearance.font_family", |s| {
                    s.appearance.font_family.clone()
                }),
            ),
        )
        .description(format!(
            "Tried first; if it is not installed the fallbacks in settings.toml apply. \
             Showing “{font_in_use}” now."
        )),
        SettingItem::new(
            "Font size",
            fields::number(
                "font-size",
                "Font size",
                NumberSpec {
                    min: f64::from(FONT_SIZE_RANGE.0),
                    max: f64::from(FONT_SIZE_RANGE.1),
                    step: 1.0,
                    decimals: 1,
                },
                |cx| float_text(current(cx).appearance.font_size),
                float_commit("appearance.font_size"),
                reset("appearance.font_size", |s| s.appearance.font_size),
            ),
        )
        .description("In points."),
        SettingItem::new(
            "Line height",
            fields::number(
                "line-height",
                "Line height",
                NumberSpec {
                    min: f64::from(LINE_HEIGHT_RANGE.0),
                    max: f64::from(LINE_HEIGHT_RANGE.1),
                    step: 0.1,
                    decimals: 2,
                },
                |cx| float_text(current(cx).appearance.line_height),
                float_commit("appearance.line_height"),
                reset("appearance.line_height", |s| s.appearance.line_height),
            ),
        )
        .description("A multiple of the font size."),
    ]);

    let theme_group = SettingGroup::new().title("Theme").items([
        SettingItem::new(
            "Appearance",
            SettingField::dropdown(
                vec![
                    ("system".into(), "Follow the system".into()),
                    ("light".into(), "Light".into()),
                    ("dark".into(), "Dark".into()),
                ],
                |cx| {
                    match current(cx).appearance.theme {
                        ThemeMode::System => "system",
                        ThemeMode::Light => "light",
                        ThemeMode::Dark => "dark",
                    }
                    .into()
                },
                {
                    let this = this.clone();
                    move |value: SharedString, cx| {
                        SettingsView::write(
                            &this,
                            vec![SettingEdit::set(
                                "appearance.theme",
                                EditValue::Str(value.to_string()),
                            )],
                            cx,
                        );
                    }
                },
            )
            .default_value("system"),
        )
        .description("Light or dark. “Follow the system” switches with macOS."),
        SettingItem::new(
            "Theme",
            SettingField::scrollable_dropdown(
                theme_options.clone(),
                |cx| theme_value(&current(cx).appearance.theme_name, cx),
                {
                    let this = this.clone();
                    move |value: SharedString, cx| {
                        SettingsView::write(
                            &this,
                            vec![SettingEdit::set(
                                "appearance.theme_name",
                                EditValue::Str(value.to_string()),
                            )],
                            cx,
                        );
                    }
                },
            )
            .default_value("default"),
        )
        .description("Bundled themes, plus any file in the themes folder next to settings.toml."),
        SettingItem::new(
            "Theme in light mode",
            SettingField::scrollable_dropdown(
                light_options,
                |cx| {
                    current(cx)
                        .appearance
                        .theme_name_light
                        .as_deref()
                        .map_or_else(SharedString::default, |name| theme_value(name, cx))
                },
                {
                    let this = this.clone();
                    move |value: SharedString, cx| {
                        let edit = if value.is_empty() {
                            SettingEdit::remove("appearance.theme_name_light")
                        } else {
                            SettingEdit::set(
                                "appearance.theme_name_light",
                                EditValue::Str(value.to_string()),
                            )
                        };
                        SettingsView::write(&this, vec![edit], cx);
                    }
                },
            )
            .default_value(""),
        )
        .description("Use a different theme whenever the light variant is wanted."),
    ]);

    let messages = SettingGroup::new().title("Messages").items([
        SettingItem::new(
            "Timestamps",
            SettingField::switch(
                |cx| current(cx).chat.timestamps,
                bool_writer(this, "chat.timestamps"),
            )
            .default_value(true),
        )
        .description("Show the time before each message."),
        SettingItem::new(
            "Emote size",
            fields::number(
                "emote-scale",
                "Emote size",
                NumberSpec {
                    min: f64::from(EMOTE_SCALE_RANGE.0),
                    max: f64::from(EMOTE_SCALE_RANGE.1),
                    step: 0.1,
                    decimals: 2,
                },
                |cx| float_text(current(cx).emotes.scale),
                float_commit("emotes.scale"),
                reset("emotes.scale", |s| s.emotes.scale),
            ),
        )
        .description("1 is 28 px for a Twitch emote."),
        SettingItem::new(
            "Animated emotes",
            SettingField::switch(
                |cx| current(cx).emotes.animate,
                bool_writer(this, "emotes.animate"),
            )
            .default_value(true),
        )
        .description("Off shows the first frame of an animated emote."),
    ]);

    SettingPage::new("Appearance")
        .icon(IconName::Palette)
        .default_open(true)
        .groups([font, theme_group, messages])
}

/// The dropdown value for a theme name in the file: the id of the theme it selects, or the name
/// itself when it matches nothing (shown as is, and the theme banner explains).
fn theme_value(name: &str, cx: &App) -> SharedString {
    theme::choice_id(name, cx).map_or_else(|| SharedString::from(name.to_owned()), Into::into)
}

fn float_commit(field: &'static str) -> Commit<f64> {
    Rc::new(move |value: f64, cx: &mut App| {
        config::edit(
            &[SettingEdit::set(field, EditValue::float(value as f32))],
            cx,
        )
        .map_err(|error| friendly(&error))
    })
}

fn bool_writer(
    this: &WeakEntity<SettingsView>,
    field: &'static str,
) -> impl Fn(bool, &mut App) + 'static {
    let this = this.clone();
    move |value, cx| {
        SettingsView::write(
            &this,
            vec![SettingEdit::set(field, EditValue::Bool(value))],
            cx,
        );
    }
}

fn chat_page(this: &WeakEntity<SettingsView>) -> SettingPage {
    let removed = SettingGroup::new()
        .title("Moderation")
        .items([SettingItem::new(
            "Removed messages",
            SettingField::dropdown(
                vec![
                    ("strikethrough".into(), "Struck through".into()),
                    ("collapse".into(), "Collapsed".into()),
                ],
                |cx| {
                    match current(cx).chat.deleted_messages {
                        DeletedMessages::Strikethrough => "strikethrough",
                        DeletedMessages::Collapse => "collapse",
                    }
                    .into()
                },
                {
                    let this = this.clone();
                    move |value: SharedString, cx| {
                        SettingsView::write(
                            &this,
                            vec![SettingEdit::set(
                                "chat.deleted_messages",
                                EditValue::Str(value.to_string()),
                            )],
                            cx,
                        );
                    }
                },
            )
            .default_value("strikethrough"),
        )
        .description(
            "How a message a moderator deleted (or whose author was timed out or banned) is shown: \
         dimmed and struck through where it is, or collapsed to “<message deleted>” until you \
         click it.",
        )]);
    SettingPage::new("Chat").icon(IconName::Inbox).groups([
        SettingGroup::new().title("History").items([
            SettingItem::new(
                "Load recent messages",
                SettingField::switch(
                    |cx| current(cx).chat.history,
                    bool_writer(this, "chat.history"),
                )
                .default_value(true),
            )
            .description(
                "Show a channel's recent messages when you join it and fill in what you missed \
                 after a reconnect, dimmed. Privacy: this sends the channel's name (and your IP \
                 address) to the third-party recent-messages service, recent-messages.robotty.de, \
                 and if it was not recording that channel yet, asking makes it start. Off: \
                 nothing is sent. Applies the next time a channel is joined.",
            ),
            SettingItem::new(
                "Scrollback",
                fields::number(
                    "scrollback",
                    "Scrollback",
                    NumberSpec {
                        min: SCROLLBACK_RANGE.0 as f64,
                        max: SCROLLBACK_RANGE.1 as f64,
                        step: 100.0,
                        decimals: 0,
                    },
                    |cx| current(cx).chat.scrollback.to_string(),
                    Rc::new(|value: f64, cx: &mut App| {
                        config::edit(
                            &[SettingEdit::set(
                                "chat.scrollback",
                                EditValue::Int(value as i64),
                            )],
                            cx,
                        )
                        .map_err(|error| friendly(&error))
                    }),
                    reset("chat.scrollback", |s| s.chat.scrollback),
                ),
            )
            .description("Messages kept per channel. Takes effect the next time the app starts."),
        ]),
        removed,
    ])
}

fn emotes_page(this: &WeakEntity<SettingsView>) -> SettingPage {
    let provider = |title: &'static str, description: &'static str, field: &'static str| {
        let read: fn(&Settings) -> bool = match field {
            "emotes.seventv" => |s| s.emotes.seventv,
            "emotes.bttv" => |s| s.emotes.bttv,
            _ => |s| s.emotes.ffz,
        };
        SettingItem::new(
            title,
            SettingField::switch(move |cx| read(current(cx)), bool_writer(this, field))
                .default_value(true),
        )
        .description(description)
    };
    SettingPage::new("Emote providers")
        .icon(IconName::Star)
        .group(
            SettingGroup::new()
                .title("Third-party emotes")
                .description(
                    "Twitch's own emotes are always on. A provider that is off is not requested; \
                     messages already on screen keep their emotes.",
                )
                .items([
                    provider(
                        "7TV",
                        "Channel and global sets, including zero-width emotes.",
                        "emotes.seventv",
                    ),
                    provider("BetterTTV", "Channel and global sets.", "emotes.bttv"),
                    provider("FrankerFaceZ", "Channel and global sets.", "emotes.ffz"),
                ]),
        )
}

fn streamlink_page() -> SettingPage {
    SettingPage::new("Streamlink").icon(IconName::Play).group(
        SettingGroup::new()
            .title("Watching a stream")
            .description("Changes apply the next time play is pressed.")
            .items([
                SettingItem::new(
                    "Streamlink program",
                    fields::text(
                        "streamlink-path",
                        "Streamlink program",
                        "Found automatically",
                        |cx| current(cx).streamlink.path.clone().unwrap_or_default(),
                        optional_text("streamlink.path"),
                        reset("streamlink.path", |s| s.streamlink.path.clone()),
                    ),
                )
                .description(
                    "Leave empty to find it automatically. A path (~/ is fine) that is not an \
                     executable is an error, not a reason to look elsewhere.",
                ),
                SettingItem::new(
                    "Player",
                    fields::text(
                        "streamlink-player",
                        "Player",
                        "Streamlink's default",
                        |cx| current(cx).streamlink.player.clone().unwrap_or_default(),
                        optional_text("streamlink.player"),
                        reset("streamlink.player", |s| s.streamlink.player.clone()),
                    ),
                )
                .description("A program such as mpv, or a full path. Leave empty for the default."),
                SettingItem::new(
                    "Default quality",
                    fields::text(
                        "streamlink-quality",
                        "Default quality",
                        "best",
                        |cx| current(cx).streamlink.default_quality.clone(),
                        Rc::new(|text: String, cx: &mut App| {
                            if text.is_empty() {
                                return Err("A quality is required, such as best.".to_owned());
                            }
                            config::edit(
                                &[SettingEdit::set(
                                    "streamlink.default_quality",
                                    EditValue::Str(text),
                                )],
                                cx,
                            )
                            .map_err(|error| friendly(&error))
                        }),
                        reset("streamlink.default_quality", |s| {
                            s.streamlink.default_quality.clone()
                        }),
                    ),
                )
                .description(
                    "best, 1080p60, 720p, audio_only, or a list such as 720p60,720p,best.",
                ),
                SettingItem::new(
                    "Extra arguments",
                    fields::lines(
                        "streamlink-args",
                        "Extra arguments",
                        "--twitch-low-latency",
                        |cx| current(cx).streamlink.extra_args.clone(),
                        Rc::new(|entries: Vec<String>, cx: &mut App| {
                            config::edit(
                                &[SettingEdit::set(
                                    "streamlink.extra_args",
                                    EditValue::StrList(entries),
                                )],
                                cx,
                            )
                            .map_err(|error| friendly(&error))
                        }),
                        reset("streamlink.extra_args", |s| s.streamlink.extra_args.clone()),
                    ),
                )
                .description("One argument per line, placed before the channel."),
            ]),
    )
}

fn account_page() -> SettingPage {
    SettingPage::new("Account")
        .icon(IconName::CircleUser)
        .group(
            SettingGroup::new()
                .title("Twitch")
                .item(SettingItem::render(|_, _, cx: &mut App| account_row(cx))),
        )
}

/// Who is logged in, with Log in / Log out. The buttons dispatch the same actions as the
/// application menu, which this window handles.
fn account_row(cx: &App) -> impl IntoElement + use<> {
    let tokens = *cx.global::<Tokens>();
    let state = account::current(cx).map(|hub| hub_state(&hub, cx));
    let (status, detail, logged_in) = match state {
        None => (
            "Not available".to_owned(),
            Some("There is no Twitch account in this run (offline modes never use it).".to_owned()),
            None,
        ),
        Some(AccountStatus::Restoring) => ("Checking the saved login…".to_owned(), None, None),
        Some(AccountStatus::LoggedOut(notice)) => ("Not logged in".to_owned(), notice, Some(false)),
        Some(AccountStatus::LoggedIn { login, relogin }) => (
            format!("Logged in as {login}"),
            relogin.then(|| {
                "This login lacks permissions the app now asks for. Log in again.".to_owned()
            }),
            Some(true),
        ),
    };
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(IconName::CircleUser))
                        .child(div().font_weight(FontWeight::MEDIUM).child(status)),
                )
                .children(
                    detail
                        .map(|detail| div().text_sm().text_color(tokens.text_muted).child(detail)),
                ),
        )
        .children(logged_in.map(|logged_in| {
            if logged_in {
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        Button::new("settings-log-in-again")
                            .small()
                            .label("Log in again…")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(LogIn), cx)),
                    )
                    .child(
                        Button::new("settings-log-out")
                            .small()
                            .label("Log out")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(LogOut), cx)),
                    )
            } else {
                h_flex().flex_none().child(
                    Button::new("settings-log-in")
                        .small()
                        .primary()
                        .label("Log in…")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(LogIn), cx)),
                )
            }
        }))
}

enum AccountStatus {
    Restoring,
    LoggedOut(Option<String>),
    LoggedIn { login: String, relogin: bool },
}

fn hub_state(hub: &gpui_kit::Entity<AccountHub>, cx: &App) -> AccountStatus {
    let hub = hub.read(cx);
    match hub.state() {
        AuthState::Restoring => AccountStatus::Restoring,
        AuthState::LoggedOut { .. } => AccountStatus::LoggedOut(hub.notice().map(str::to_owned)),
        AuthState::LoggedIn(account) => AccountStatus::LoggedIn {
            login: account.login.clone(),
            relogin: account.needs_relogin(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_refusal_names_the_setting_and_the_reason_not_the_path() {
        let invalid = SettingsError::Invalid {
            path: PathBuf::from("/config/settings.toml"),
            field: "appearance.font_size",
            reason: "500 is outside 6 to 72".into(),
        };
        assert_eq!(
            friendly(&invalid),
            "`appearance.font_size`: 500 is outside 6 to 72"
        );
        let parse = SettingsError::Parse {
            path: PathBuf::from("/x/settings.toml"),
            message: "expected `=`".into(),
            line: Some(3),
            column: Some(1),
        };
        assert_eq!(friendly(&parse), "line 3: expected `=`");
    }

    #[test]
    fn floats_show_as_typed() {
        assert_eq!(float_text(1.4), "1.4");
        assert_eq!(float_text(14.0), "14");
        assert_eq!(float_text(0.75), "0.75");
    }
}
