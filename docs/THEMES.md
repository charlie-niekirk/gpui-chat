# Themes

Colour themes live in `crates/settings` (`theme`, `themes`, `theme_watch`: parsing, validation, the
registry, the directory watch; no GPUI) and `crates/app/src/theme.rs` (turns a palette into GPUI /
GPUI Kit colours). The bundled pack is `assets/themes/*.toml`, compiled in with `include_str!`.

## Choosing a theme

```toml
[appearance]
theme = "system"              # system | light | dark: which *variant* to use
theme_name = "tokyo-night"    # which theme (bundled id or a file in <config>/themes/)
theme_name_light = "rose-pine"  # optional: use this theme whenever the light variant is wanted
```

* `theme_name` ignores case, spaces and `_` vs `-` (`Tokyo Night` = `tokyo-night`). An unknown
  name falls back to `default`, and a banner in the window says so. An unknown `theme_name_light`
  falls back to `theme_name`'s light variant first, then to `default`.
* A theme file has a `[dark]` section, a `[light]` section, or both. `theme = "dark"` / `"light"`
  pins the variant; `"system"` follows the OS appearance live. A theme that lacks the wanted variant
  still wins: its only palette is used (Kit's light/dark mode follows the palette actually used).
* **Follow-system**: `theme = "system"` with a theme that has both variants (`default`,
  `catppuccin`, `rose-pine`, or your own) flips between them with the OS. To flip between two
  different themes set `theme_name` (dark) and `theme_name_light` (light).
* Everything hot-reloads: edit `settings.toml` to switch themes, or edit/add/remove a file in
  `themes/` and the app re-resolves the active theme (a file that fails to parse keeps its last
  good version).
* **Problems are shown in the window** (a "Theme problem" strip above the content): a theme file
  that does not parse or lacks a required colour, two files with the same name once normalised
  (`Tokyo Night.toml` and `tokyo_night.toml`; the later file wins), a theme whose text is below
  3:1 contrast on its background, and a `theme_name` that matches nothing. The strip goes when
  the problem is fixed.

The settings window (⌘, → Appearance → Theme) lists the bundled pack and the files in `themes/`
(marked "custom") and writes `theme_name` / `theme_name_light` for you; see `docs/SETTINGS.md`.

## File format (`<config>/themes/<name>.toml`)

```toml
name = "My Theme"           # optional display name; the file name is the id

[dark]                      # and/or [light]
background = "#1a1b26"      # required: the chat area
text       = "#a9b1d6"      # required
accent     = "#7aa2f7"      # required: links, mentions, selection, primary buttons
# Optional; derived from the three above when missing:
window      = "#13141c"     # behind everything (default: sidebar)
sidebar     = "#13141c"     # (default: background nudged towards text)
title_bar   = "#24283b"
border      = "#292e42"
surface     = "#24283b"     # menus, popovers, dialogs, inputs, emote picker, autocomplete
text_muted  = "#787c99"     # (default: text faded towards background, keeping 4.5:1; a value under 4.5:1 on any surface is moved towards `text` until it reaches it)
accent_text = "#ffffff"     # text on an accent fill (default: black or white by contrast; moved to 4.5:1 when it falls short)
danger  = "#f7768e"         # errors, the live dot
success = "#9ece6a"         # connected
warning = "#e0af68"         # connecting / reconnecting
```

Colours are `#rgb` or `#rrggbb`. Unknown keys are ignored. A user file with the id of a bundled
theme replaces it, but keeps the variant it does not define: a `catppuccin.toml` with only
`[dark]` still follows the OS to the bundled Latte. `danger_text` and `accent_link` (danger / accent drawn as *text*) are derived, never written:
each is nudged towards `text` until it has 4.5:1 on every surface it is drawn on, plain and under
a tint. Chat usernames keep their Twitch colours (adjusted for readability against the
theme's `background`).

## Bundled pack

The pack is the stock [Omarchy](https://github.com/basecamp/omarchy) set (`themes/*/colors.toml`,
MIT) plus the app's own `default`. Each file's values are my own transcription of that palette
into the roles above (Omarchy's `dark_background` -> `window`/`sidebar`, `lighter_background` ->
`title_bar`/`surface`, `selection` -> `border`; `text_muted` is derived). Status colours are taken
from a palette's red/green/yellow only where it has recognisable ones; otherwise the variant's
defaults apply. Omarchy's `catppuccin` + `catppuccin-latte` are one two-variant theme here. The dark
half of `rose-pine` is the published Rosé Pine palette (Omarchy's `rose-pine` is Dawn, the light one).

| Id | Variants | Palette from |
|---|---|---|
| `default` | dark, light | this app |
| `catppuccin` | dark (Mocha), light (Latte) | Catppuccin |
| `catppuccin-latte` | light | alias of `catppuccin`'s light variant (Omarchy's name) |
| `everforest` | dark | Everforest |
| `flexoki-light` | light | Flexoki |
| `gruvbox` | dark | Gruvbox Material, as in Omarchy |
| `kanagawa` | dark | Kanagawa |
| `nord` | dark | Nord |
| `rose-pine` | dark, light (Dawn) | Rosé Pine |
| `tokyo-night` | dark | Tokyo Night |
| `ristretto` | dark | Omarchy's Ristretto (see the licence note) |
| `ethereal`, `hackerman`, `jade-horse`, `last-horizon`, `lumon`, `lupine`, `matte-black`, `miasma`, `retro-82`, `solitude`, `starship`, `vantablack`, `white` | one each | Omarchy's own themes |

**Licences:** `assets/themes/NOTICES.md` holds the MIT notices (Omarchy, Catppuccin, Rosé Pine)
and the upstream licence of every other palette. The Omarchy values were verified against
`basecamp/omarchy@d76f364b41ccf01909b1d84ffcc6bd4fbb8cd91d`. Ristretto's upstream (Monokai Pro)
is the one licence that is **not** verified; only Omarchy's colour values are used.

## What the theme drives

`Tokens` (`crates/app/src/theme.rs`) is the app's token set: every view reads colours from it, so
chat, sidebar, header, composer, autocomplete list, emote picker, reply strip, thread popover,
tooltips, the debug HUD and banners switch together. The same palette is pushed into GPUI Kit's
`Theme` colours (buttons, inputs, menus, dialogs, title bar, sidebar, scrollbars, ...). Kit's own
JSON theme system is not used: its schema has ~170 keys and ours has 13.
