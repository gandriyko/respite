# Respite

## Agent skill

`skills/respite/SKILL.md` is the user-facing guide for AI agents. Whenever a change touches the
CLI (command, flag, output, exit code), `.http` syntax (directive, body form, variable,
`@assert` subject or operator) or environments, update the skill in the same change, then the
README. `cargo test -p respite-cli skill_covers_cli` fails when a command, long flag, directive
or assert operator is missing from it; semantics it can't check are on you.

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
