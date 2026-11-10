# macOS app icons

Run `bash scripts/generate-macos-icons.sh` on macOS with Xcode. The fixed master
`mac-original.png` and measured rectangles in `layout.json` are the inputs.
The preparation tool preserves the existing design and aspect ratio. Larger
representations put the background within the central 80% (10% on each edge).
16pt and 32pt @1x use a full-bleed background, with the glyph fitted into the
central 80%. For tall glyphs the horizontal padding is therefore greater than
10%; for wide glyphs the vertical padding is greater. These are artwork margins,
not a guarantee about margins after macOS normalizes the displayed icon.

`AppIcon.iconset` keeps 16pt @2x distinct from 32pt @1x even though both are 32px.
`scripts/macos-icon.swift` emits largest-first ICNS with ARGB small entries and
Assets.car, then verifies 12 IconServices renders and 7 app-catalog renders, plus 3 native background-edge assertions.
Run `swift scripts/macos-icon.swift verify resources/app-icons/AppIcon.iconset src-tauri/icons Pullkit`
without regeneration to verify the committed output. Rendering uses native APIs;
it is not an automated Dock or System Settings UI test.

The helper is vendored from app-icon-sizing; it requires no other project checkout.
Do not replace its output with `tauri icon`, `iconutil`, or electron-builder's
PNG conversion: this loses the small-size representations or their encoding.
The macOS-only artwork does not replace Windows, web or menu-bar assets.

The outermost small-image pixels retain an antialiased alpha up to 239/255,
without an empty outer band. Fully opaque edges triggered a light native frame
on macOS 27. Verification compares native edge colors with the actual source,
because two equally framed app fixtures can otherwise agree and both be wrong.
