//! Key bindings in effect: the defaults (`platform::default_bindings`) with the user's
//! `keymap.toml` applied on top, hot-reloaded (PR 4.3).
//!
//! Parsing and checking the file is `gpui_chat_settings::keymap` (no GPUI); this module turns its
//! entries into GPUI [`KeyBinding`]s. An entry **replaces** every default shortcut of its action
//! (`[]` unbinds), and removes any other action's default on the very same keystroke and context,
//! so what the palette and the menus show is what the keys do. The keymap is rebuilt from scratch
//! on every change: `clear_key_bindings`, then what Kit registered at startup (`base`, taken
//! before ours), then ours, then the menu bar (macOS reads the shortcuts a menu shows when it
//! is set).
//!
//! **This module is the only place that registers key bindings.** The rebuild drops anything bound
//! with `cx.bind_keys` outside it (after `init`, nothing but Kit's own startup bindings survives),
//! so a feature that wants keys exposes them through `platform::default_bindings` (as
//! `completion::bindings` and `emote_picker::bindings` do) and gets an entry in [`catalog`]. `apply`
//! checks the count: the keymap must hold exactly Kit's bindings plus the effective ones, and a
//! rebuild that finds extra bindings logs an error (a `debug_assert` in debug builds).
//!
//! [`catalog`] is the list of actions `keymap.toml` and the command palette know by name. A test
//! keeps it in step with the actions the app defines.
//!
//! Problems (bad entries, a file that does not parse, two actions on one key) are kept in a
//! global; the shell shows them in a strip ([`notices`]).

use std::rc::Rc;

use gpui_chat_settings::{
    KeymapEntry, KeymapReport, KeymapWatcher, SettingsPaths, WatchOptions, load_keymap,
    watch_keymap,
};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::{
    Action, AnyWindowHandle, App, AppContext as _, Entity, Global, KeyBinding,
    KeyBindingContextPredicate,
};

use crate::actions::*;
use crate::bridge::{self, BridgeReceiver};
use crate::chat::selection_layer::{ClearSelection, CopySelection};
use crate::completion::{
    ComposerComplete, ComposerCompletePrevious, ComposerCompletionDismiss,
    ComposerToggleEmotePicker,
};
use crate::emote_picker::{
    EmotePickerClose, EmotePickerDown, EmotePickerLeft, EmotePickerNextTab, EmotePickerPreviousTab,
    EmotePickerRight, EmotePickerUp,
};
use crate::platform;

/// An action the user can bind by name and (when `palette`) run from the command palette.
#[derive(Clone, Copy)]
pub struct ActionInfo {
    /// The name in `keymap.toml`: the action's type name.
    pub id: &'static str,
    /// What the palette and the docs call it.
    pub label: &'static str,
    /// The palette's heading for it.
    pub group: &'static str,
    /// Listed in the palette (the input box's own keys are not).
    pub palette: bool,
    build: fn() -> Box<dyn Action>,
}

impl ActionInfo {
    pub fn build(&self) -> Box<dyn Action> {
        (self.build)()
    }
}

macro_rules! catalog {
    ($( $palette:literal $group:literal $action:ident $label:literal ),+ $(,)?) => {
        /// Every action that can be bound, in the order the palette lists them.
        static CATALOG: &[ActionInfo] = &[$(ActionInfo {
            id: stringify!($action),
            label: $label,
            group: $group,
            palette: $palette,
            build: || Box::new($action),
        }),+];
    };
}

catalog![
    false "Go" OpenPalette "Command Palette…",
        true "Go" OpenChannelSwitcher "Go to Channel…",
        true "Channel" AddChannel "Add Channel…",
        true "Channel" RemoveChannel "Remove Channel",
        true "Channel" MoveChannelUp "Move Channel Up",
        true "Channel" MoveChannelDown "Move Channel Down",
        true "Channel" SelectNextChannel "Next Channel",
        true "Channel" SelectPreviousChannel "Previous Channel",
        true "Channel" ReloadEmotes "Reload Emotes",
        true "Pane" SplitRight "Split Right",
        true "Pane" SplitDown "Split Down",
        true "Pane" ClosePane "Close Pane",
        true "Pane" FocusPaneLeft "Focus Pane Left",
        true "Pane" FocusPaneRight "Focus Pane Right",
        true "Pane" FocusPaneUp "Focus Pane Above",
        true "Pane" FocusPaneDown "Focus Pane Below",
        true "Chat" CopySelection "Copy Selection",
        false "Chat" ClearSelection "Clear Selection",
        true "Chat" OpenUserCard "Open User Card for Selected Message",
        true "Chat" Find "Find in Chat…",
        true "Chat" FindNext "Find Next",
        true "Chat" FindPrevious "Find Previous",
        false "Chat" FindClose "Close Find Bar",
        true "Chat" ClearFilter "Clear Chat Filter",
        true "Chat" ToggleMentionsFilter "Show Only Mentions and Highlights",
        true "Chat" ToggleLinksFilter "Show Only Messages with Links",
        true "Chat" ToggleBotsFilter "Hide Bots",
        true "Chat" IgnoreUser "Ignore Author of Selected Message",
        true "Chat" UnignoreUser "Unignore Author of Selected Message",
        true "Chat" BlockOnTwitch "Block Author of Selected Message on Twitch…",
        true "Account" EnableBlockListSync "Enable Block List Sync…",
        true "View" ToggleSidebar "Toggle Sidebar",
        true "View" ShowMentions "Show Mentions",
        true "View" ToggleDebugHud "Toggle Debug HUD",
        true "Account" LogIn "Log In to Twitch…",
        true "Account" LogOut "Log Out",
        true "App" OpenSettings "Settings…",
        true "App" About "About",
        true "App" MinimizeWindow "Minimize Window",
        true "App" CloseWindow "Close Window",
        true "App" Quit "Quit",
        false "Input" ComposerHistoryPrevious "Previous Sent Message",
        false "Input" ComposerHistoryNext "Next Sent Message",
        false "Input" ComposerComplete "Complete",
        false "Input" ComposerCompletePrevious "Complete Backwards",
        false "Input" ComposerCompletionDismiss "Dismiss Completion",
        false "Input" ComposerToggleEmotePicker "Emote Picker",
        false "Emote Picker" EmotePickerLeft "Emote Left",
        false "Emote Picker" EmotePickerRight "Emote Right",
        false "Emote Picker" EmotePickerUp "Emote Up",
        false "Emote Picker" EmotePickerDown "Emote Down",
        false "Emote Picker" EmotePickerNextTab "Next Emote Tab",
        false "Emote Picker" EmotePickerPreviousTab "Previous Emote Tab",
        false "Emote Picker" EmotePickerClose "Close Emote Picker",
];

/// Every action that can be bound, in the order the palette lists them.
pub fn catalog() -> &'static [ActionInfo] {
    CATALOG
}

/// What is in force: the layers the keymap is rebuilt from, and what was wrong with the file.
#[derive(Clone)]
pub struct KeymapState {
    /// What Kit (inputs, menus, dialogs, lists) registered before the app's own bindings.
    base: Vec<KeyBinding>,
    /// The app's default bindings for this platform.
    defaults: Vec<KeyBinding>,
    /// The user's entries.
    entries: Vec<KeymapEntry>,
    /// Problems with the file itself (one line each).
    file_problems: Vec<String>,
    /// Problems found turning the entries into bindings.
    apply_problems: Vec<String>,
    /// How many bindings the keymap held after the last [`apply`]; a different count at the next
    /// one means something bound keys behind this module's back (see the module docs).
    applied: usize,
}

impl Global for KeymapState {}

/// What to tell the user about their keymap: one line each, empty when all is well.
pub fn notices(cx: &App) -> Vec<String> {
    cx.try_global::<KeymapState>().map_or_else(Vec::new, |s| {
        s.file_problems
            .iter()
            .chain(&s.apply_problems)
            .cloned()
            .collect()
    })
}

/// The names `keymap.toml` may use.
fn known_ids() -> Vec<&'static str> {
    catalog().iter().map(|info| info.id).collect()
}

/// Events about the keymap file are rare and small.
const EVENT_QUEUE: usize = 8;

/// Keeps the keymap watcher alive; dropped on quit.
struct KeymapWatchGuard(Option<KeymapWatcher>);

impl Global for KeymapWatchGuard {}

/// Installs the key bindings (Kit's, then the app's defaults, then the user's `keymap.toml`) and
/// the menu bar, and starts watching the file. Call once at startup right after `gpui_kit::init`
/// and **before anything else binds keys**: whatever is in the keymap now is taken as Kit's
/// (`base`), and every later change rebuilds from `base` + the defaults + the file, so a binding
/// added elsewhere with `cx.bind_keys` would be wiped by the first edit of `keymap.toml`
/// (see the module docs).
/// Returns the receiver to hand to [`attach_reloads`] once a window exists, or `None` when the
/// file is not being watched (no home directory, or the watcher could not start; logged, and
/// the keymap read at startup still applies).
pub fn init(cx: &mut App) -> Option<BridgeReceiver<KeymapReport>> {
    let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
    let paths = SettingsPaths::from_env().ok();
    let known = known_ids();
    let report = paths.as_ref().map_or_else(KeymapReport::default, |paths| {
        load_keymap(&paths.keymap_file(), &known, &[])
    });
    for problem in &report.problems {
        tracing::warn!(%problem, "keymap.toml");
    }
    cx.set_global(KeymapState {
        base,
        defaults: platform::default_bindings(),
        entries: report.entries.clone(),
        file_problems: report.problems.iter().map(ToString::to_string).collect(),
        apply_problems: Vec::new(),
        applied: 0,
    });
    apply(cx);

    let paths = paths?;
    let (tx, rx) = bridge::channel(EVENT_QUEUE, EVENT_QUEUE);
    let watcher = watch_keymap(
        paths.keymap_file(),
        known.iter().map(|id| (*id).to_owned()).collect(),
        report,
        WatchOptions::default(),
        move |report| {
            // Runs on the watcher's own thread; a closed bridge means the app is quitting.
            let _ = tx.blocking_send(report);
        },
    );
    match watcher {
        Ok(watcher) => {
            cx.set_global(KeymapWatchGuard(Some(watcher)));
            cx.on_app_quit(|cx| {
                cx.global_mut::<KeymapWatchGuard>().0.take();
                async {}
            })
            .detach();
            Some(rx)
        }
        Err(error) => {
            tracing::warn!(%error, "keymap.toml changes will not be picked up live");
            crate::problems::report_watch_failure("keymap.toml", &error.to_string());
            None
        }
    }
}

/// Entity that receives keymap reloads from the watcher thread (through the bridge) and applies
/// them on the UI thread. The bridge holds it weakly, so a global keeps it alive.
struct KeymapReloads;

struct KeymapReloadsHandle(#[allow(dead_code)] Entity<KeymapReloads>);

impl Global for KeymapReloadsHandle {}

/// Connects the watcher to the UI: an edit of `keymap.toml` rebuilds the key bindings and the
/// menu bar at once.
pub fn attach_reloads(
    receiver: BridgeReceiver<KeymapReport>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let entity: Entity<KeymapReloads> = cx.new(|_| KeymapReloads);
    bridge::attach(&entity, window, receiver, cx, |_, reports, cx| {
        let Some(report) = reports.into_iter().last() else {
            return false;
        };
        for problem in &report.problems {
            tracing::warn!(%problem, "keymap.toml");
        }
        update(cx, |state| {
            state.entries = report.entries;
            state.file_problems = report.problems.iter().map(ToString::to_string).collect();
        });
        false
    })
    .detach();
    cx.set_global(KeymapReloadsHandle(entity));
}

/// Changes the state, then rebuilds the bindings from it.
fn update(cx: &mut App, change: impl FnOnce(&mut KeymapState)) {
    let Some(mut state) = cx.try_global::<KeymapState>().cloned() else {
        return;
    };
    change(&mut state);
    cx.set_global(state);
    apply(cx);
}

/// Rebuilds the keymap and the menu bar from the state.
fn apply(cx: &mut App) {
    let Some(mut state) = cx.try_global::<KeymapState>().cloned() else {
        return;
    };
    let (bindings, problems) = effective(&state.defaults, &state.entries, cx);
    let held = cx.key_bindings().borrow().bindings().count();
    if state.applied != 0 && held != state.applied {
        tracing::error!(
            held,
            expected = state.applied,
            "key bindings were registered outside `keymap` and are dropped by this rebuild: \
             hand them to `platform::default_bindings` instead"
        );
        debug_assert_eq!(
            held, state.applied,
            "something called `cx.bind_keys` after `keymap::init`; keymap is the only place that may"
        );
    }
    let expected = state.base.len() + bindings.len();
    cx.clear_key_bindings();
    cx.bind_keys(state.base.iter().cloned());
    cx.bind_keys(bindings);
    state.applied = cx.key_bindings().borrow().bindings().count();
    debug_assert_eq!(
        state.applied, expected,
        "the keymap holds exactly Kit's bindings plus the effective app bindings"
    );
    // The menus read the shortcuts they show from the keymap when they are set.
    cx.set_menus(platform::menus());
    state.apply_problems = problems;
    cx.set_global(state);
}

/// The app's bindings once `entries` are applied to `defaults`, and what could not be applied.
fn effective(
    defaults: &[KeyBinding],
    entries: &[KeymapEntry],
    cx: &App,
) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings: Vec<KeyBinding> = defaults.to_vec();
    let mut problems = Vec::new();
    // Bindings the user's entries added so far, to catch one key given to two actions.
    let mut claimed: Vec<(KeyBinding, &str)> = Vec::new();
    for entry in entries {
        let Some(info) = catalog().iter().find(|info| info.id == entry.action) else {
            problems.push(format!("{}: unknown action", entry.action));
            continue;
        };
        let name = info.build().name();
        let predicate = match &entry.context {
            Some(source) => match KeyBindingContextPredicate::parse(source) {
                Ok(predicate) => Some(Rc::new(predicate)),
                Err(error) => {
                    problems.push(format!(
                        "{}: context \"{source}\": {error}; keeping the default shortcut",
                        entry.action
                    ));
                    continue;
                }
            },
            // Where the action's first default shortcut applies.
            None => defaults
                .iter()
                .find(|binding| binding.action().name() == name)
                .and_then(KeyBinding::predicate),
        };
        let mut added = Vec::new();
        for keys in &entry.keys {
            match KeyBinding::load(keys, info.build(), predicate.clone(), false, None, cx) {
                Ok(binding) => added.push(binding),
                Err(error) => problems.push(format!("{}: {error}", entry.action)),
            }
        }
        if !entry.keys.is_empty() && added.is_empty() {
            // Nothing usable was given: the defaults stay rather than the action losing its keys.
            problems.push(format!(
                "{}: no usable shortcut; keeping the default",
                entry.action
            ));
            continue;
        }
        added.retain(|binding| {
            let taken = claimed.iter().find(|(other, _)| {
                other.keystrokes() == binding.keystrokes()
                    && other.predicate() == binding.predicate()
            });
            if let Some((_, owner)) = taken {
                problems.push(format!(
                    "{}: {} is already bound to {owner}; keeping that",
                    entry.action,
                    keys_label(binding)
                ));
            }
            taken.is_none()
        });
        if !entry.keys.is_empty() && added.is_empty() {
            // Every key was another entry's: the defaults stay, the problem is already listed.
            continue;
        }
        bindings.retain(|binding| {
            binding.action().name() != name
                && !added.iter().any(|new| {
                    new.keystrokes() == binding.keystrokes()
                        && new.predicate() == binding.predicate()
                })
        });
        claimed.extend(added.iter().cloned().map(|binding| (binding, info.id)));
        bindings.extend(added);
    }
    // A user shortcut that shares its keystroke with another action in a *different* context
    // (so the displacement above did not take it) now answers to whichever context is narrower:
    // say so. Restating one of the action's own defaults is not news, and neither is a default
    // that was displaced (it is gone from `bindings`).
    for (binding, id) in &claimed {
        let restates_default = defaults.iter().any(|default| {
            default.action().partial_eq(binding.action())
                && default.keystrokes() == binding.keystrokes()
                && default.predicate() == binding.predicate()
        });
        if restates_default {
            continue;
        }
        for other in &bindings {
            if other.action().partial_eq(binding.action())
                || other.keystrokes() != binding.keystrokes()
            {
                continue;
            }
            let other_name = other.action().name();
            let other_id = catalog()
                .iter()
                .find(|info| info.build().name() == other_name)
                .map_or(other_name, |info| info.id);
            let context = other
                .predicate()
                .map_or_else(|| "everywhere".to_owned(), |p| format!("in {p}"));
            problems.push(format!(
                "{id}: {} is also bound to {other_id} ({context}); the narrower context wins",
                keys_label(binding)
            ));
        }
    }
    (bindings, problems)
}

/// A keystroke sequence as the keyboard shows it: `⌘⇧D`, `Ctrl+K Ctrl+S`.
fn keys_label(binding: &KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|stroke| Kbd::format(stroke.inner()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The shortcut `action` has now (the first one, like the menus), as it is shown to the user, or
/// `None` when it has none.
pub fn shortcut(action: &dyn Action, cx: &App) -> Option<String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap.bindings_for_action(action).next().map(keys_label)
}

/// `shortcut` of an action known by type: for hints written into the UI ("Press ⌘N to …").
pub fn shortcut_of<A: Action>(action: A, cx: &App) -> Option<String> {
    shortcut(&action, cx)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use gpui_kit::TestAppContext;

    use super::*;

    fn entry(action: &str, keys: &[&str], context: Option<&str>) -> KeymapEntry {
        KeymapEntry {
            action: action.to_owned(),
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            context: context.map(str::to_owned),
        }
    }

    /// Initialises Kit (its own bindings) and the app's defaults, like `main` before the window.
    /// Returns how many bindings Kit registered.
    fn setup(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
            let kit = base.len();
            cx.set_global(KeymapState {
                base,
                defaults: platform::default_bindings(),
                entries: Vec::new(),
                file_problems: Vec::new(),
                apply_problems: Vec::new(),
                applied: 0,
            });
            apply(cx);
            kit
        })
    }

    fn keys_of<A: Action>(action: A, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            keymap
                .bindings_for_action(&action)
                .map(keys_label)
                .collect()
        })
    }

    #[gpui_kit::test]
    fn the_catalog_covers_every_action_the_app_defines(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let defined: BTreeSet<&str> = cx
                .all_action_names()
                .iter()
                .copied()
                .filter(|name| name.starts_with("gpui_chat::"))
                .collect();
            let listed: BTreeSet<&str> = catalog().iter().map(|info| info.build().name()).collect();
            assert_eq!(
                defined.difference(&listed).collect::<Vec<_>>(),
                Vec::<&&str>::new(),
                "actions missing from keymap::catalog"
            );
            assert_eq!(listed.len(), catalog().len(), "an action is listed twice");
            for info in catalog() {
                assert_eq!(
                    info.build().name().rsplit("::").next(),
                    Some(info.id),
                    "{} is not the action's name",
                    info.id
                );
            }
        });
    }

    #[gpui_kit::test]
    fn without_a_keymap_file_the_defaults_apply_and_kit_keeps_its_bindings(
        cx: &mut TestAppContext,
    ) {
        let kit_bindings = setup(cx);
        assert!(kit_bindings > 0);
        assert_eq!(keys_of(SplitRight, cx).len(), 1);
        cx.update(|cx| {
            let total = cx.key_bindings().borrow().bindings().count();
            assert_eq!(
                total,
                kit_bindings + platform::default_bindings().len(),
                "Kit's bindings survive the rebuild"
            );
        });
    }

    #[gpui_kit::test]
    fn an_entry_replaces_the_defaults_of_its_action_and_unbind_removes_them(
        cx: &mut TestAppContext,
    ) {
        setup(cx);
        let before = keys_of(ToggleSidebar, cx);
        assert_eq!(before.len(), 2, "{before:?}");
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![
                    entry("ToggleSidebar", &["cmd-j"], None),
                    entry("ClosePane", &[], None),
                ];
            });
            assert_eq!(notices(cx), Vec::<String>::new());
        });
        assert_eq!(keys_of(ToggleSidebar, cx), [keys_label_of("cmd-j", cx)]);
        assert!(keys_of(ClosePane, cx).is_empty());
        // Unrelated actions are untouched.
        assert_eq!(keys_of(SplitRight, cx).len(), 1);
        // Back to no entries: the defaults return.
        cx.update(|cx| update(cx, |state| state.entries.clear()));
        assert_eq!(keys_of(ToggleSidebar, cx), before);
        assert_eq!(keys_of(ClosePane, cx).len(), 1);
    }

    fn keys_label_of(keys: &str, cx: &mut TestAppContext) -> String {
        cx.update(|cx| {
            let binding = KeyBinding::load(keys, Box::new(Quit), None, false, None, cx).unwrap();
            keys_label(&binding)
        })
    }

    #[gpui_kit::test]
    fn a_new_shortcut_takes_the_key_from_the_action_that_had_it(cx: &mut TestAppContext) {
        setup(cx);
        let split_right = keys_of(SplitRight, cx);
        cx.update(|cx| {
            // The default of SplitRight is cmd-d in the shell context, which ClosePane now takes
            // there (the context defaults to the one ClosePane's own shortcut has).
            update(cx, |state| {
                state.entries = vec![entry("ClosePane", &["cmd-d"], None)];
            });
        });
        assert!(keys_of(SplitRight, cx).is_empty(), "was {split_right:?}");
        assert_eq!(keys_of(ClosePane, cx), [keys_label_of("cmd-d", cx)]);
    }

    #[gpui_kit::test]
    fn problems_are_reported_and_the_defaults_stay(cx: &mut TestAppContext) {
        setup(cx);
        let palette_default = keys_of(OpenPalette, cx);
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![
                    // Fine: `-` is the key (GPUI reads `cmd-ctrl--` the same way).
                    entry("SplitDown", &["cmd-ctrl--"], None),
                    // GPUI's context grammar rejects the dangling `&&`, which the file check
                    // (which only balances parentheses) does not.
                    entry("OpenPalette", &["cmd-p"], Some("AppShell &&")),
                    entry("FocusPaneUp", &["cmd-u"], None),
                    entry("FocusPaneDown", &["cmd-u"], None),
                ];
            });
            let shown = notices(cx);
            assert_eq!(shown.len(), 2, "{shown:?}");
            assert!(
                shown[0].starts_with("OpenPalette: context \"AppShell &&\""),
                "{shown:?}"
            );
            assert_eq!(
                shown[1],
                format!(
                    "FocusPaneDown: {} is already bound to FocusPaneUp; keeping that",
                    keys_label_in("cmd-u", cx)
                )
            );
        });
        // The bad context left the action's defaults; the conflict left FocusPaneDown's.
        assert_eq!(keys_of(OpenPalette, cx), palette_default);
        assert_eq!(
            keys_of(FocusPaneDown, cx),
            keys_of_default(FocusPaneDown, cx)
        );
        assert_eq!(keys_of(FocusPaneUp, cx), [keys_label_of("cmd-u", cx)]);
        // The valid entry applied.
        assert_eq!(keys_of(SplitDown, cx), [keys_label_of("cmd-ctrl--", cx)]);
    }

    fn keys_label_in(keys: &str, cx: &mut App) -> String {
        let binding = KeyBinding::load(keys, Box::new(Quit), None, false, None, cx).unwrap();
        keys_label(&binding)
    }

    #[gpui_kit::test]
    fn a_key_another_action_has_in_another_context_is_reported(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            // ⌘Q is Quit's everywhere; SplitRight's shortcut lives in the shell context, so
            // the two coexist and the narrower one wins inside the shell.
            update(cx, |state| {
                state.entries = vec![entry("SplitRight", &["cmd-q"], None)]
            });
            let shown = notices(cx);
            assert_eq!(shown.len(), 1, "{shown:?}");
            assert!(
                shown[0].starts_with("SplitRight: ")
                    && shown[0].contains("is also bound to Quit (everywhere)"),
                "{shown:?}"
            );
        });
        assert_eq!(keys_of(Quit, cx).len(), 1, "Quit keeps its key");
    }

    #[gpui_kit::test]
    fn swapping_two_defaults_and_restating_one_are_not_problems(cx: &mut TestAppContext) {
        setup(cx);
        let (right, down) = (keys_of(SplitRight, cx), keys_of(SplitDown, cx));
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![
                    entry("SplitDown", &["cmd-d"], None),
                    entry("SplitRight", &["cmd-alt-shift-d"], None),
                    // The default again, spelled out.
                    entry("ClosePane", &["cmd-shift-w"], None),
                    entry("ComposerHistoryPrevious", &["up"], None),
                ];
            });
            assert_eq!(notices(cx), Vec::<String>::new());
        });
        assert_eq!(keys_of(SplitDown, cx), right);
        assert_eq!(keys_of(SplitRight, cx), down);
    }

    #[gpui_kit::test]
    fn an_entry_whose_keys_all_lose_a_conflict_keeps_the_defaults(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![
                    entry("FocusPaneDown", &["cmd-u"], None),
                    entry("FocusPaneUp", &["cmd-u"], None),
                ];
            });
        });
        // FocusPaneDown (first by name) won ⌘U; FocusPaneUp lost its only key and kept its default.
        assert_eq!(keys_of(FocusPaneDown, cx), [keys_label_of("cmd-u", cx)]);
        assert_eq!(keys_of(FocusPaneUp, cx), keys_of_default(FocusPaneUp, cx));
    }

    #[gpui_kit::test]
    fn a_rebuild_holds_exactly_kits_bindings_plus_the_effective_ones(cx: &mut TestAppContext) {
        let kit = setup(cx);
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![entry("ClosePane", &[], None)]
            });
            let held = cx.key_bindings().borrow().bindings().count();
            assert_eq!(held, kit + platform::default_bindings().len() - 1);
        });
    }

    /// A composer-like box: the input inside the composer's key context, as in a pane.
    struct ComposerBox {
        input: Entity<gpui_kit::component::input::InputState>,
    }

    impl gpui_kit::Render for ComposerBox {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            _: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{InteractiveElement as _, ParentElement as _};
            gpui_kit::div()
                .key_context(crate::actions::COMPOSER_CONTEXT)
                .child(gpui_kit::component::input::Input::new(&self.input))
        }
    }

    #[gpui_kit::test]
    fn copy_in_the_input_box_copies_its_own_text_not_the_chat_selection(cx: &mut TestAppContext) {
        setup(cx);
        let chat_copies = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = chat_copies.clone();
        cx.update(|cx| {
            crate::theme::apply_settings(
                &gpui_chat_settings::AppearanceSettings::default(),
                None,
                cx,
            );
            cx.on_action(move |_: &CopySelection, _| counter.set(counter.get() + 1));
        });
        let (view, cx) = cx.add_window_view(|window, cx| ComposerBox {
            input: cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).default_value("typed text")
            }),
        });
        cx.update(|window, cx| {
            let input = view.read(cx).input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.run_until_parked();
        let (all, copy) = if cfg!(target_os = "macos") {
            ("cmd-a", "cmd-c")
        } else {
            ("ctrl-a", "ctrl-c")
        };
        cx.simulate_keystrokes(all);
        cx.simulate_keystrokes(copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("typed text")
        );
        assert_eq!(chat_copies.get(), 0, "the chat's Copy is not what ⌘C ran");
    }

    fn keys_of_default<A: Action>(action: A, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let defaults = platform::default_bindings();
            let _ = &cx;
            defaults
                .iter()
                .filter(|binding| binding.action().partial_eq(&action))
                .map(keys_label)
                .collect()
        })
    }

    #[gpui_kit::test]
    fn a_context_can_be_chosen(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            update(cx, |state| {
                state.entries = vec![entry(
                    "ComposerHistoryPrevious",
                    &["ctrl-p"],
                    Some("ChatComposer > Input"),
                )];
            });
            assert!(notices(cx).is_empty(), "{:?}", notices(cx));
        });
        assert_eq!(
            keys_of(ComposerHistoryPrevious, cx),
            [keys_label_of("ctrl-p", cx)]
        );
    }
}
