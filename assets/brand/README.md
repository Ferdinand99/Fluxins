# Inlet brand assets

The Inlet logo is a speech bubble with a bay cut into its side: many servers flow into one client. The logo and the wordmark are Inlet's own work, under the repository licence (AGPL-3.0-or-later).

The Inlet logo is not Fluxer's logo, and Fluxer's logo is not Inlet's. Do not combine the two, do not use Fluxer's name, logo or colours as Inlet's branding, and do not use the Inlet logo in a way that suggests Fluxer made or endorses Inlet. "Fluxer" is a trademark of Fluxer Platform AB. Inlet only says that it works with Fluxer.

## Colours

| Name | Hex | Use |
|---|---|---|
| Inlet teal | `#2BD4BD` | Main brand colour. The mark on dark backgrounds, the top of the icon gradient. |
| Inlet deep teal | `#0E8C82` | The mark on light backgrounds. |
| Icon gradient end | `#0A5F68` | Bottom-right of the icon tile. |
| Foam | `#F6F3EC` | The bubble on the icon, the wordmark on dark backgrounds. |
| Night | `#16151B` | The wordmark on light backgrounds, mono icons on light backgrounds. |

The icon carries its own colours. It does not change with the app's accent colour (indigo `#413CDD` by default, or an instance's own accent such as orange `#FF7E00`). Only the mono mark may be tinted with an accent colour.

## Files

| File | What it is |
|---|---|
| `inlet-icon.svg` | App icon, full-bleed rounded square. Use from 64 px up. |
| `inlet-icon-small.svg` | App icon simplified for 48 px and below: rounder bubble, shorter bay with a wider mouth, larger tail. |
| `inlet-icon-macos.svg`, `inlet-icon-macos-small.svg` | The same on the macOS icon grid (824 px body in a 1024 px canvas). |
| `inlet-mark.svg` | The bubble alone, in teal. |
| `inlet-mark-mono.svg` | The small-size bubble in `currentColor`, for tray icons and inline use. |
| `inlet-wordmark.svg` | The word "Inlet" as filled paths (no font needed). |
| `inlet-lockup-dark.svg`, `inlet-lockup-light.svg` | Mark and wordmark, for dark and light backgrounds. `readme/` has 2x PNGs of both. |
| `windows/inlet.ico` | 16, 20, 24, 32, 40, 48, 64, 128 and 256 px. Built into `inlet.exe` by `crates/inlet-app/build.rs`. |
| `macos/inlet.icns`, `macos/Inlet.iconset/` | 16 to 512 px at @1x and @2x, for the app bundle. |
| `linux/hicolor/` | 16 to 512 px PNGs and a scalable SVG, named after the app id `no.opland.inlet`. Copy into `/usr/share/icons/hicolor/` (or `~/.local/share/icons/hicolor/`) next to a `no.opland.inlet.desktop` file. The 128 px PNG is also the X11 window icon. |
| `tray/` | Mono tray icons: `on-light` (dark glyph) and `on-dark` (light glyph) at 16, 20, 24, 32 and 48 px, plus a macOS template image (`inlet-trayTemplate.png`, @2x). |
| `favicon/` | `favicon.ico` (16, 32, 48), `favicon.svg`, 16 and 32 px PNGs, `apple-touch-icon.png` (180), `icon-192.png` and `icon-512.png`. |

## Use

- Keep clear space around the mark of at least the height of the bay.
- Do not stretch, rotate, recolour (except the mono mark), outline or add effects.
- Use the small icon at 48 px and below, and the full icon above that.
- On a busy or mid-tone background, use the icon tile, not the bare mark.

## Rebuilding the PNG, ICO and ICNS files

The SVG files are the source. The other files were rendered from them with librsvg (`rsvg-convert -w <size>`). The ICO and ICNS files hold the PNG renders as PNG-compressed frames. Render again after any change to an SVG.
