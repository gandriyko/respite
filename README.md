<div align="center">

<img src="crates/app/assets/icon.svg" width="96" alt="Respite icon">

# Respite

**A fast, text-first REST client. Requests are plain `.http` files.**

Write a request, press <kbd>⌘</kbd> <kbd>Enter</kbd>, read the response. No forms, no cloud, no lock-in.

[![Rust](https://img.shields.io/badge/rust-2024_edition-orange?logo=rust)](https://www.rust-lang.org)
[![GPUI](https://img.shields.io/badge/UI-GPUI-blue)](https://www.gpui.rs)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey?logo=apple)](#install)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](Cargo.toml)

<img src="docs/screenshot.png" width="900" alt="Respite: project tree, .http editor and response panel">

</div>

---

## Why Respite

- 📄 **Files are the source of truth.** One `.http` file holds many requests separated by `###`. You can commit it, diff it and review it like any other code.
- 🔁 **Compatible.** Uses the same syntax as VS Code REST Client and JetBrains HTTP Client, so your existing `.http` / `.rest` files open unchanged.
- ⚡ **Native and GPU-rendered.** Built in Rust on [GPUI](https://www.gpui.rs), the UI framework behind Zed.
- 🧰 **GUI and CLI.** The same files run from the desktop app, a terminal or CI, and the CLI can print JSON for scripts and AI agents.
- 📥 **Bring your collections.** Import from Postman, Insomnia, OpenAPI / Swagger, Bruno, HAR and cURL.
- 🔐 **Works behind real auth.** OAuth 2.0 (including the browser and device flows), client certificates and corporate proxies, configured in the same env files.

## Features

| | |
|---|---|
| **Editor** | `.http` syntax highlighting (JSON, XML, GraphQL and JS bodies), completion, diagnostics with quick fixes, rename, live templates, folding; a syntax reference with runnable examples (<kbd>F1</kbd>) |
| **Response** | Body, headers, cookies, the request as sent, timeline (redirects, time to first byte, download); multi-MB bodies, images, JSONPath filter, run history ‹ › with diff, pin |
| **Variables** | File variables, environments in one central folder (`respite-dev.json`, secrets masked by name) picked per file with `# @env dev`, dynamic values (`{{$uuid}}`, `{{$timestamp}}`, …), values taken from earlier responses |
| **Requests** | Multipart, form-urlencoded, GraphQL, bodies loaded from files, per-request directives (`@timeout`, `@name`, `@insecure`, …); TLS checks can also be turned off for all requests in Settings |
| **Auth & network** | OAuth 2.0 via `{{$auth.token("id")}}` (Client Credentials, Password, Authorization Code + PKCE in the system browser, Device Authorization) with cached, refreshed tokens; client certificates (mTLS) and extra CAs per environment; system or custom proxy. Configured the JetBrains way, so files stay portable |
| **Tests** | `# @assert status == 201`, `# @assert body $.id exists`, `# @assert duration < 500ms`: httpyac's operators, written as comments so the file still runs in VS Code and JetBrains; results in the Tests tab, `respite run` exits 1 on a failure |
| **History** | Every run is stored: a sortable, filterable table with status, duration and an OK / failed summary; reopen or rerun any entry |
| **cURL** | Paste a `curl` command and get a request block; copy any request back as cURL |
| **Workspace** | Collections in `~/.config/respite/collections` (with subfolders) plus `.http` files added from anywhere, light and dark themes, settings, state restored on launch |

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

You need a recent stable Rust toolchain (edition 2024); see [Development setup](#development-setup-macos) for installing it with Homebrew.

## Desktop app

The sidebar has two parts:

- **Collections**: the central folder, `~/.config/respite/collections`. Create files and folders there from the sidebar (hover a folder for its buttons); the trash button moves a file or folder to the Trash (a folder asks first). A folder's import button (or File > Import…) takes several picks at once: `.http` / `.rest` files are copied in, and Postman, Insomnia, OpenAPI, Bruno and HAR exports each become a folder, keeping their subfolders.
- **Files**: `.http` / `.rest` files that live elsewhere, for example in a repository. **+** adds them (several at once), or drag them from Finder onto the window; **×** drops one from the list without touching the file.

Run `respite file.http` to open a file; it joins the Files list. Each file picks its environment with `# @env dev` on its first line, and the environment menu above the editor writes that line for you.

| Shortcut | Action |
|---|---|
| <kbd>⌘</kbd> <kbd>Enter</kbd> | Run the request under the cursor |
| <kbd>⌘</kbd> <kbd>V</kbd> | Smart paste: a cURL command becomes a request block |
| <kbd>⌘</kbd> <kbd>⇧</kbd> <kbd>C</kbd> | Copy request as cURL |
| <kbd>⌘</kbd> <kbd>⌥</kbd> <kbd>E</kbd> | Next environment |
| <kbd>⌘</kbd> <kbd>Y</kbd> | History |
| <kbd>⌘</kbd> <kbd>B</kbd> | Toggle sidebar |
| <kbd>⌘</kbd> <kbd>,</kbd> | Settings |
| <kbd>F1</kbd> | Syntax reference |
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
respite env          Environments in ~/.config/respite/env
respite auth         OAuth 2.0 tokens: print one (token ID), forget them (clear)
respite history      Past executions
```

Some useful flags:

```sh
respite run api.http --env prod --output json        # override the file's # @env; machine-readable output
respite run api.http --env-file ci/respite-ci.json   # env files from elsewhere (CI has no ~/.config/respite)
respite run api.http createUser --dry-run            # show the resolved request, send nothing
respite run api.http --fail-on-status --fail-fast    # use it as a smoke test in CI
respite run api.http --jsonpath '$.items[*].id'      # print only the matching values
```

## OAuth 2.0

Write the token into a request as in JetBrains HTTP Client, and the file runs there too:

```http
GET https://api.example.com/me
Authorization: Bearer {{$auth.token("api")}}
```

The config lives in an env file under `Security` → `Auth`, with JetBrains' field names; the secret sits next to it and is masked by its name:

```jsonc
// ~/.config/respite/env/respite-dev.json
{
  "Security": { "Auth": { "api": {
    "Type": "OAuth2",
    "Grant Type": "Client Credentials",
    "Token URL": "https://auth.example.com/oauth/token",
    "Client ID": "respite",
    "Client Secret": "{{clientSecret}}",
    "Scope": "read write"
  }}},
  "clientSecret": "…"
}
```

- **Grants.**
  - `Client Credentials` and `Password`.
  - `Authorization Code`: Respite opens the system browser and catches the redirect on a loopback address. Set `Redirect URL` to something like `http://127.0.0.1:8765/callback`, or leave it out and a free port is picked. Add `"PKCE": true` to use PKCE.
  - `Device Authorization`: Respite shows "Enter code … at …" while the request waits, opens the page and polls until you sign in.
  - `"Type": "Mock"` with a `"Token"` gives a fixed token for tests.
  - Implicit is not supported.
- **Other fields.** `Client Credentials` (`basic`, `in body` or `none`), `Custom Request Parameters`, `Custom Request Headers` and `Acquire Automatically`. `{{$auth.idToken("api")}}` gives the ID token.
- **When a token is fetched.** Only when a request first needs it. It is reused until shortly before it expires, renewed with the refresh token when there is one, and fetched again when the config changes.
- **Cache.** Tokens are kept per environment in `~/.respite/history/oauth.<env>.json`, a file only you can read. The app and the CLI share it, and history masks the tokens.
- **Commands.** `respite auth token api --env dev` prints a token, `respite auth clear --env dev` forgets them. In the app, use **Clear OAuth 2.0 Tokens** in the command palette.

To try it, use the public Duende demo server. Its token endpoint is `https://demo.duendesoftware.com/connect/token` and its API is `https://demo.duendesoftware.com/api/test`. Test clients:

- `m2m` with secret `secret` and scope `api`, for Client Credentials;
- `interactive.public` with `"PKCE": true`, for the browser. Set `Auth URL` to `https://demo.duendesoftware.com/connect/authorize` and sign in as bob / bob;
- `device`, for the device flow. Set `Device Auth URL` to `https://demo.duendesoftware.com/connect/deviceauthorization`.

## Certificates and proxy

Client certificates (mTLS) are set up the JetBrains way, in an env file:

```jsonc
// respite-dev.json
{
  "SSLConfiguration": {
    "clientCertificate": "certs/client.pem",
    "clientCertificateKey": "certs/client.key",
    "caCertificate": "certs/dev-ca.pem",
    "verifyHostCertificate": true
  }
}
```

- **Files.** Paths are relative to the env file. The certificate and key can be PEM or DER. Write `{"path": …, "format": "DER"}` when the extension doesn't tell. P12/PFX files and keys with a passphrase are not supported yet; the error shows the `openssl` command that converts them to PEM.
- **CAs.** `caCertificate` is Respite's addition: one CA file or a list, trusted on top of the system store. `"verifyHostCertificate": false` turns certificate checks off for the whole environment.
- **Scope.** The setup applies to every request in the environment, OAuth 2.0 token requests included.
- **Proxy.** By default the system proxy is used: macOS settings, `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`. To override it, use **Settings → Requests**, or `request.proxy` (`http://user:pass@proxy:3128`) and `request.no_proxy` (`["localhost", ".internal"]`) in settings.json.

## Tests

Write checks above the request line. They are plain comments to VS Code REST Client and JetBrains, so the file still runs there; Respite runs them after every response.

```http
### Create a user
# @name createUser
# @assert status == 200
# @assert header content-type contains json
# @assert body $.json.name == {{name}}
# @assert body json.id exists
# @assert duration < 2 s
POST https://httpbin.org/post
Content-Type: application/json

{"name": "{{name}}", "id": "{{$uuid}}"}
```

`# @assert <subject> <operator> [value]`, with httpyac's vocabulary:

| | |
|---|---|
| **Subjects** | `status` · `header <name>` · `body` (the whole body as text) · `body <JSONPath>` (`$.a[0].b`; a bare `a.b` means `$.a.b`) · `duration` (total ms; the value may use `ms` / `s` / `m`) |
| **Compare** | `==` `!=` `<` `<=` `>` `>=` (numbers as numbers, otherwise text) · `contains` `startsWith` `endsWith` · `matches` (regex) |
| **No value** | `exists` · `isTrue` · `isFalse` · `isNumber` · `isString` · `isBoolean` · `isArray` |

The value may hold `{{variables}}`. In the app, results show in the **Tests** tab, and a red dot marks the tab when one fails. `respite run` prints ✓ / ✗ for each check, lists them under `tests` in JSON output, and exits with `1` when any fails, which makes a `.http` file a CI smoke test:

```sh
respite run api.http --env staging --fail-fast
```

## AI agents

[`skills/respite/SKILL.md`](skills/respite/SKILL.md) teaches a coding agent (Claude Code, Codex and others that read Agent Skills) to send requests and test an API through the CLI. Copy the folder into the agent's skills directory, for example `~/.claude/skills/respite`.

For clients that talk MCP instead of running commands, `respite mcp` is a stdio MCP server with the same commands as tools (`list`, `run`, `check`, `show`, `env_show`, …). Each tool runs the CLI command with JSON output, so the results, masking and exit codes are the same. Nothing opens on screen.

```sh
claude mcp add respite -- respite mcp
```

The server works in one workspace: the directory it was started in, or `--root /path/to/project` for clients that start it elsewhere (Claude Desktop). Paths outside it, and files other than `.http` / `.rest` where a request file is expected, are refused, so env files can't be read through it. Nothing returns secrets in clear: `show` gives the block as written, `export_curl` keeps `{{variables}}`, and `run` masks secret env values, OAuth tokens and `$processEnv` / `$dotenv` values. The requests themselves still carry the secrets, to whatever host the `.http` file names.

## Environments

All environments live in one folder, `~/.config/respite/env/`, and serve every `.http` file wherever it is. One flat file per environment, secrets included; `respite.json` holds values shared by all of them. Values whose key contains `pass`, `pwd`, `secret`, `token`, `key` or `credential` are masked in history and CLI output:

```jsonc
// respite-dev.json
{ "host": "http://localhost:8080" }

// respite-prod.json
{ "host": "https://api.example.com", "token": "…" }
```

A file picks its environment on its first line; the app's environment menu writes it, and `respite run --env` overrides it:

```http
# @env prod
GET {{host}}/me
Authorization: Bearer {{token}}
```

Env files next to `.http` files are not read. In CI, where there is no `~/.config/respite`, pass them with `--env-file`. 

## Where files live

Two folders, kept apart on purpose: one for what you write, one for what the app writes.

```text
~/.config/respite/          what you write; fine for a dotfiles repo, unless env/ holds real secrets
  settings.json             settings (the Settings page edits it)
  keymap.json               your key bindings over the defaults (optional)
  collections/              the sidebar's Collections: .http / .rest files, any subfolders
  env/                      environments, one flat JSON file each, for every .http file anywhere
    respite.json            values shared by all environments
    respite-dev.json        …

~/.respite/                 what the app writes; holds tokens and response bodies, keep it private
  files.json                the sidebar's Files: paths of .http files added from elsewhere
  state.json                the last open file and cursor, reopened on launch
  history/                  every run: <time>-<request>.json (request, response, timings)
                            and <time>-<request>.body (raw response body)
    cookies.<env>.json      cookie jar per environment (cookies._.json without one)
    oauth.<env>.json        OAuth tokens per environment
```

- **Collections** are plain files and folders, so Finder, an editor or git work on them too. The trash button moves them to the Trash.
- **Files** stay where they are, for example in a repository; Respite keeps only their paths. **×** drops one from the list, the file is untouched.
- **History** names a file relative to `collections/` when it lives there, by full path otherwise, so moving the whole `collections/` folder keeps it.

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
  settings/     ~/.config/respite/settings.json
```

## Development setup (macOS)

Everything comes from [Homebrew](https://brew.sh) plus Apple's Command Line Tools; full Xcode is not needed.

```sh
# Compiler, linker and the macOS SDK (iconutil for the app icon is included)
xcode-select --install

# Rust toolchain; cmake builds aws-lc (TLS), librsvg is only for regenerating the icon
brew install rust cmake librsvg

git clone git@github.com:gandriyko/respite.git
cd respite
cargo run -p respite                      # the app, opening ~/.config/respite/collections
cargo run -p respite -- path/to/api.http  # the app on a file or folder
cargo run -p respite -- run api.http      # the CLI
```

Before committing:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets   # expected to be warning-free
```

Other tasks:

```sh
scripts/bundle-macos.sh       # target/release/Respite.app
scripts/make-icon.sh          # crates/app/assets/icon.svg → Respite.icns
```

`scripts/bundle-macos.sh --universal` builds for both Apple Silicon and Intel and needs rustup instead of Homebrew's `rust`:

```sh
brew uninstall rust && brew install rustup
export PATH="$(brew --prefix rustup)/bin:$PATH"   # keg-only; add this line to ~/.zshrc
rustup default stable
rustup target add x86_64-apple-darwin aarch64-apple-darwin
```

## Roadmap

- [x] `.http` parser, variables, environments, executor, history
- [x] CLI with JSON output, cURL import and export
- [x] Import from Postman, Insomnia, OpenAPI / Swagger, Bruno and HAR
- [x] Navigation: <kbd>⌘</kbd> <kbd>P</kbd>, go to request, command palette, breadcrumbs
- [x] Autocompletion and inline diagnostics in the editor
- [ ] Timeline: DNS, TCP and TLS phases
- [x] MCP server for AI agents (`respite mcp`)
- [x] Declarative assertions (`# @assert`)
- [ ] Pre-request and test scripts, JUnit output
- [x] OAuth 2.0: Client Credentials, Password, Authorization Code + PKCE, Device Authorization, refresh tokens (`$auth.token`)
- [x] Client certificates (mTLS), extra trusted CAs, proxy settings
- [ ] P12/PFX certificates and passphrase-protected keys
- [ ] WebSocket, SSE, gRPC; AWS SigV4 auth

## License

[MIT](Cargo.toml)
