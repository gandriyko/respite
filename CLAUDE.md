# Respite

## Dependencies

Never modify third-party crates: no `[patch]` overrides, vendored copies or forks of
dependencies (e.g. `gpui-base`, `gpui-component`). Work within the project's own code using
the crates' public API. If a feature needs something the public API doesn't expose, drop or
simplify the feature, or raise it with the user — don't patch the library.
