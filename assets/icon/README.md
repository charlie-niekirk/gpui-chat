# App icon

`icon-1024.png` is the committed source image (1024×1024, 8-bit RGBA, transparent corners, drawn on
Apple's icon grid: an 824 px rounded square centred on the canvas). `scripts/bundle-macos.sh` turns it
into `AppIcon.icns` (`sips` + `iconutil`); no other tool is needed.

To change the icon, replace the PNG and keep it 1024×1024 with transparency.
