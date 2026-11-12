<div align="center">

<img src="crates/app/assets/icon.svg" width="96" alt="Respite icon">

# Respite

**A fast, text-first REST client. Requests are plain `.http` files.**

Write a request, press <kbd>⌘</kbd> <kbd>Enter</kbd>, read the response. No forms, no cloud, no lock-in.

[![Rust](https://img.shields.io/badge/rust-2024_edition-orange?logo=rust)](https://www.rust-lang.org)
[![GPUI](https://img.shields.io/badge/UI-GPUI-blue)](https://www.gpui.rs)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey?logo=apple)](#install)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](Cargo.toml)

</div>

---

## Why Respite

- 📄 **Files are the source of truth.** One `.http` file holds many requests separated by `###`. You can commit it, diff it and review it like any other code.
- 🔁 **Compatible.** Uses the same syntax as VS Code REST Client and JetBrains HTTP Client, so your existing `.http` / `.rest` files open unchanged.
- ⚡ **Native and GPU-rendered.** Built in Rust on [GPUI](https://www.gpui.rs), the UI framework behind Zed.
- 🧰 **GUI and CLI.** The same files run from the desktop app, a terminal or CI, and the CLI can print JSON for scripts and AI agents.
- 📥 **Bring your collections.** Import from Postman, Insomnia, OpenAPI / Swagger, Bruno, HAR and cURL.

## Features

| | |
|---|---|
| **Editor** | `.http` syntax highlighting, a ▶ run button in the gutter, per-request status inline, folding |
| **Variables** | File variables, `http-client.env.json` environments with private overrides, dynamic values (`{{$uuid}}`, `{{$timestamp}}`, …), values taken from earlier responses |
| **Requests** | Multipart, form-urlencoded, GraphQL, bodies loaded from files, per-request directives (`@timeout`, `@name`, …) |
| **Responses** | Body with highlighting and search, headers, the request as it was sent, timings |
| **History** | Every run is stored per project and can be browsed, filtered and reopened |
| **cURL** | Paste a `curl` command and get a request block; copy any request back as cURL |
| **Workspace** | Collections in `~/.config/respite` plus any project folders, light and dark themes, settings, state restored on launch |

## Quick start

```http
@host = https://httpbin.org

### Get my IP
GET {{host}}/ip
Accept: application/json

### Create a user
# @name createUser
POST {{host}}/post
Content-Type: application/json

{
  "name": "Ada",
  "id": "{{$uuid}}"
}
```

Save this as `api.http`, open it in Respite and press <kbd>⌘</kbd> <kbd>Enter</kbd> on either request. To run it from a terminal instead:

```sh
respite run api.http "Get my IP"
```

## Install

Respite currently targets **macOS**. Linux should work but has not been tested yet.

```sh
git clone git@github.com:gandriyko/respite.git
cd respite

# Desktop app → target/release/Respite.app (--install also copies it to /Applications)
scripts/bundle-macos.sh --install

# Or just the binary (GUI + CLI in one)
cargo install --path crates/app
```

You need a recent stable Rust toolchain (edition 2024).

## Desktop app

Run `respite` with no arguments to open your collections in `~/.config/respite`. Run `respite path/to/dir` or `respite file.http` to open a specific project. Each folder in `~/.config/respite` shows up as a collection at the top of the sidebar, and imported collections are saved there too.

| Shortcut | Action |
|---|---|
| <kbd>⌘</kbd> <kbd>Enter</kbd> | Run the request under the cursor |
| <kbd>⌘</kbd> <kbd>V</kbd> | Smart paste: a cURL command becomes a request block |
| <kbd>⌘</kbd> <kbd>⇧</kbd> <kbd>C</kbd> | Copy request as cURL |
| <kbd>⌘</kbd> <kbd>⌥</kbd> <kbd>E</kbd> | Next environment |
| <kbd>⌘</kbd> <kbd>Y</kbd> | History |
| <kbd>⌘</kbd> <kbd>B</kbd> | Toggle sidebar |
| <kbd>⌘</kbd> <kbd>,</kbd> | Settings |
| <kbd>⌘</kbd> <kbd>S</kbd> / <kbd>⌘</kbd> <kbd>W</kbd> | Save / close tab |

## CLI

The desktop app and the CLI ship as one binary.

```text
respite list         List requests: file, line, name, method, URL
respite show         Print a request block; --resolved substitutes variables
respite run          Run requests (all of them in file order when none are named)
respite check        Diagnostics without sending anything (pre-commit / CI)
respite import       Postman / Insomnia / OpenAPI / Swagger / Bruno / HAR → .http files
respite import-curl  cURL from stdin or the clipboard → request block(s)
respite export-curl  Request → cURL command
respite env          Environments from http-client(.private).env.json
respite history      Past executions of this workspace
```

Some useful flags:

```sh
respite run api.http --env prod --output json        # machine-readable output
respite run api.http createUser --dry-run            # show the resolved request, send nothing
respite run api.http --fail-on-status --fail-fast    # use it as a smoke test in CI
respite run api.http --jsonpath '$.items[*].id'      # print only the matching values
```

## Environments

Respite reads the same environment files as JetBrains HTTP Client. Keep secrets in the `.private` file and git-ignore it:

```jsonc
// http-client.env.json
{
  "dev":  { "host": "http://localhost:8080" },
  "prod": { "host": "https://api.example.com" }
}

// http-client.private.env.json  (git-ignored)
{
  "prod": { "token": "…" }
}
```

## Project layout

```text
crates/
  app/          GPUI desktop app (sidebar, editor, response panel, history, settings)
  cli/          respite CLI commands
  http-syntax/  .http parser
  http-env/     environments and variable resolution
  http-exec/    executor on reqwest: cookies, timings, history store
  http-curl/    cURL ⇄ request conversion
  http-import/  Postman, Insomnia, OpenAPI, Bruno and HAR importers
  settings/     ~/.config/respite/settings.json plus per-project overrides
```

Run `cargo test --workspace` to test everything.

## Roadmap

- [x] `.http` parser, variables, environments, executor, history
- [x] CLI with JSON output, cURL import and export
- [x] Import from Postman, Insomnia, OpenAPI / Swagger, Bruno and HAR
- [ ] Navigation: <kbd>⌘</kbd> <kbd>P</kbd>, go to request, breadcrumbs
- [ ] Autocompletion and inline diagnostics in the editor
- [ ] MCP server for AI agents
- [ ] Pre-request and test scripts, JUnit output
- [ ] WebSocket, SSE, gRPC; OAuth 2.0 and AWS SigV4 auth

## License

[MIT](Cargo.toml)
