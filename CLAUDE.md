# Respite

## Dependencies

Never modify third-party crates: no `[patch]` overrides, vendored copies or forks of
dependencies (e.g. `gpui-base`, `gpui-component`). Work within the project's own code using
the crates' public API. If a feature needs something the public API doesn't expose, drop or
simplify the feature, or raise it with the user — don't patch the library.

## Icons

The app's asset source (`gpui_kit_assets::Assets`) bundles only the icons named in
gpui-component's `IconName`. `Icon::default().path("icons/<name>.svg")` for any other icon
compiles but draws nothing, with no error.

- Use `IconName` when it has the icon.
- Otherwise copy the SVG from the Lucide catalog in the registry
  (`gpui-kit-assets-0.9.1/assets/icons/<name>.svg`) to `crates/app/assets/<name>.svg` and use
  `Icon::default().data(include_bytes!("../assets/<name>.svg"))`. Don't switch to `AllAssets`
  (it embeds the whole catalog).
- Size icons from the interface font (`cx.theme().font_size`), not fixed pixels: the UI font
  size is configurable, and fixed 12–14 px icons look tiny next to a large font.
