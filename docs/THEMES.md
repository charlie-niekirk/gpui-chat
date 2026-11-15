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
  name falls back to `default` (logged).
* A theme file has a `[dark]` section, a `[light]` section, or both. `theme = "dark"` / `"light"`
  pins the variant; `"system"` follows the OS appearance live. A theme that lacks the wanted variant
  still wins: its only palette is used (Kit's light/dark mode follows the palette actually used).
* **Follow-system**: `theme = "system"` with a theme that has both variants (`default`,
  `catppuccin`, `rose-pine`, or your own) flips between them with the OS. To flip between two
  different themes set `theme_name` (dark) and `theme_name_light` (light).
* Everything hot-reloads: edit `settings.toml` to switch themes, or edit/add/remove a file in
  `themes/` and the app re-resolves the active theme (a file that fails to parse keeps its last
  good version and is logged).

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
text_muted  = "#787c99"     # (default: text faded towards background, keeping 4.5:1)
accent_text = "#ffffff"     # text on an accent fill (default: black or white by contrast)
danger  = "#f7768e"         # errors, the live dot
success = "#9ece6a"         # connected
warning = "#e0af68"         # connecting / reconnecting
```

Colours are `#rgb` or `#rrggbb`. Unknown keys are ignored. A user file with the id of a bundled
theme replaces it. Chat usernames keep their Twitch colours (adjusted for readability against the
theme's `background`).

## Bundled pack

The pack is the stock [Omarchy](https://github.com/basecamp/omarchy) set (`themes/*/colors.toml`,
MIT) plus the app's own `default`. Each file's values are my own transcription of that palette
into the roles above (Omarchy's `dark_background` -> `window`/`sidebar`, `lighter_background` ->
`title_bar`/`surface`, `selection` -> `border`; `text_muted` is derived). Status colours are taken
from a palette's red/green/yellow only where it has recognisable ones; otherwise the variant's
defaults apply. Omarchy's `catppuccin` + `catppuccin-latte` are one two-variant theme here. The dark
half of `rose-pine` is the published Rosé Pine palette (Omarchy's `rose-pine` is Dawn, the light one).

| Id | Variants | Palette from (licence) |
|---|---|---|
| `default` | dark, light | this app |
| `catppuccin` | dark (Mocha), light (Latte) | Catppuccin (MIT) |
| `everforest` | dark | Everforest (MIT) |
| `flexoki-light` | light | Flexoki (MIT) |
| `gruvbox` | dark | Gruvbox Material-style dark, as in Omarchy (MIT) |
| `kanagawa` | dark | Kanagawa (MIT) |
| `nord` | dark | Nord (MIT) |
| `rose-pine` | dark, light (Dawn) | Rosé Pine (MIT) |
| `tokyo-night` | dark | Tokyo Night (MIT) |
| `ristretto` | dark | Omarchy's Ristretto (Monokai Pro filter colours; values only) |
| `ethereal`, `hackerman`, `jade-horse`, `last-horizon`, `lumon`, `lupine`, `matte-black`, `miasma`, `retro-82`, `solitude`, `starship`, `vantablack`, `white` | one each | Omarchy's own bundled themes (MIT, same repository) |

Each bundled file's first line names its source. Upstream licences are as far as I could tell from
the projects; the colour values are facts, no code or artwork is copied.

## What the theme drives

`Tokens` (`crates/app/src/theme.rs`) is the app's token set: every view reads colours from it, so
chat, sidebar, header, composer, autocomplete list, emote picker, reply strip, thread popover,
tooltips, the debug HUD and banners switch together. The same palette is pushed into GPUI Kit's
`Theme` colours (buttons, inputs, menus, dialogs, title bar, sidebar, scrollbars, ...). Kit's own
JSON theme system is not used: its schema has ~170 keys and ours has 13.
