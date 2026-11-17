---
name: respite
description: Send HTTP requests and test an API with the Respite CLI and plain `.http` files (VS Code REST Client / JetBrains syntax), including declarative `# @assert` checks, environments, request chaining, OAuth 2.0 tokens (`$auth.token`), client certificates (mTLS), custom CAs and proxies, imports from Postman / Insomnia / OpenAPI / Bruno / HAR and cURL conversion. Use when the user asks to call, try, smoke-test or regression-test an HTTP endpoint (also one behind OAuth, mTLS or a corporate proxy), write or fix `.http` / `.rest` files or their env files, import an API collection, or check an API in CI, and `respite` is installed or the project has `.http` files.
---

# Respite: HTTP requests and API tests from `.http` files

`respite` is one binary: the desktop app with no arguments (or a path), a CLI with a subcommand.
From an agent, use the CLI only; never launch the GUI.

## Rules for agents

- Pass `--non-interactive` to `run`: otherwise a `@prompt` or `@note` waits for input.
- Pass `--output json` (or `jsonl`) when you read the result; without a TTY JSON is the default.
- Before sending anything with side effects, check it with `--dry-run` or `show --resolved`.
- Never write secrets into `.http` files; use environment files (below, in
  `~/.config/respite/env/`). Name secret keys so they read as secrets (`password`, `apiKey`,
  `clientSecret`, `token`): only those are masked in output and history.
- After editing a `.http` file, run `respite check` on it.
- A network error carries its cause, for example "connection refused" or "certificate
  required". Read it before changing the request: a TLS or proxy problem is fixed in the env
  file or the settings, not in the `.http` file.

## Commands

Requests are addressed by `@name`, `###` title, `:LINE` (any line of the block) or `#N` (1-based).

| Command | What it does |
|---|---|
| `respite list [PATH]` | Requests in a file or directory: file, line, name, method, URL |
| `respite show FILE REQUEST [--resolved]` | The request block; `--resolved` substitutes variables |
| `respite run FILE [REQUESTS]...` | Send requests (none named = all, in file order) |
| `respite check [PATHS]...` | Diagnostics only, sends nothing (pre-commit / CI) |
| `respite import PATH [--to DIR]` | Postman / Insomnia / OpenAPI / Swagger / Bruno folder / HAR → `.http` files in a new folder (default under `~/.config/respite`), its environments into the env folder |
| `respite import-curl` | cURL command(s) from stdin → request block(s) |
| `respite export-curl FILE REQUEST [--raw]` | Request → cURL; `--raw` keeps `{{variables}}` |
| `respite env list` | Environment names |
| `respite env show NAME` | Merged variables of one environment, secrets masked |
| `respite auth token ID [--force]` | OAuth 2.0 token of a config: cached while fresh, else refreshed or acquired; `--force` runs the full grant |
| `respite auth clear [--env NAME]` | Forget the cached OAuth 2.0 tokens of one environment |
| `respite history [--last N]` | Past executions, newest first (default 20) |
| `respite mcp [--root DIR]` | MCP server on stdio (see below) |

`list`, `check`, `import`, `env`, `history` and `run` take `--output human|json|jsonl`.

Variables (`run`, `show`, `export-curl`, `auth token`):

- `--env NAME` — environment (`dev` reads `respite-dev.json`); without it, the file's `# @env`.
- `--var KEY=VALUE` — override any variable, highest priority, repeatable.
- `--env-file PATH` — extra env file over the env folder's, named `respite-<env>.json`,
  repeatable (CI: env files from the repo or secrets).

`run` options:

- `--dry-run` — resolve and print the request, send nothing.
- `--fail-fast` — stop at the first failure.
- `--fail-on-status` — exit 1 on a 4xx/5xx response.
- `--non-interactive` — never ask; a missing `@prompt` value or a `@note` becomes an error.
- `--include headers,body,timing` — parts to print (human: body; JSON: all).
- `--max-body 64KB` — cap the printed body.
- `--jsonpath '$.items[*].id'` — print only the matches from a JSON body.

`import-curl` options:

- `--clipboard` — read from the clipboard instead of stdin.
- `--to FILE` — append to a `.http` file instead of printing.
- `--env NAME` — replace that environment's values with `{{name}}`.
- `--no-original` — don't keep the command as a comment.

Exit codes:

| Code | Meaning |
|---|---|
| `0` | OK |
| `1` | An `@assert` failed, or a 4xx/5xx with `--fail-on-status` |
| `2` | Network error or timeout |
| `3` | Parse error, undefined variable or bad input, an OAuth 2.0 config or token request failing (`auth_failed`); also `check` finding errors |
| `4` | Refused: a `@note` needs confirmation |

`run --output json` gives one object per request (an array when there are several):
`request` (method, url, headers), `response` (status, reason, http_version, headers, body,
size_bytes), `timing_ms`, `tests`, `dependencies`, `history_id`, `warnings`, or `error`.
Values of secret variables are masked.

### MCP server

`respite mcp` serves the same commands as MCP tools over stdio, for clients without a shell:
`list`, `check`, `show`, `run`, `export_curl`, `import`, `import_curl`, `env_list`,
`env_show`, `history`. Arguments mirror the flags (`dry_run`, `fail_on_status`, …); `vars` is
an object instead of repeated `--var`. Each tool returns the command's JSON output; a non-zero
exit code sets `isError` and is appended to the text. History marks these runs `mcp`.

- `--root DIR` is the workspace (default: the directory the client starts the server in).
  Relative paths start there, and every path must stay inside it; where a request file is
  expected it must be `.http` / `.rest`. Env files can't be read through the tools.
- Nothing returns secrets in clear: `show` has no `resolved` (use `run` with `dry_run`, which
  masks), `export_curl` keeps `{{variables}}`, and `run` masks secret env values, OAuth
  tokens and the `$processEnv` / `$dotenv` values the file uses.
- `run` is always non-interactive: `@note` requests are refused, `@prompt` values come in `vars`.
- The tools still send requests with the secrets in them, to whatever host the file names.

Register it, for example in Claude Code: `claude mcp add respite -- respite mcp`. Clients that
start servers elsewhere (Claude Desktop) need `respite mcp --root /path/to/project`.

## File syntax

```http
# @env dev
@host = {{baseUrl}}

### Log in
# @name login
POST {{host}}/auth
Content-Type: application/json

{"user": "ann", "password": "{{password}}"}

### Who am I
# @assert status == 200
# @assert body $.name == ann
GET {{host}}/me
Authorization: Bearer {{login.response.body.$.token}}
```

- `###` starts a block (the rest of the line is its title). Comments start with `#` or `//`.
- Request line: `[METHOD] URL [HTTP/x]`; no method means `GET`. A long query can continue on
  indented lines starting with `?` / `&`.
- Headers follow the request line; a blank line, then the body.
- Directives, file variables and comments go **above** the request line.
- `> {% … %}`, `< {% … %}` and `> handler.js` scripts are recognised but **not executed**; use
  `# @assert`.

### Directives

| Directive | Effect |
|---|---|
| `# @env dev` | **First line only**: the environment the whole file runs with (`--env` overrides it); elsewhere it's a `misplaced_env` warning |
| `# @name login` | Name the request (for addressing and chaining); must be unique in the file |
| `# @timeout 10 s` | Request timeout; `ms` / `s` / `m`, seconds by default |
| `# @connection-timeout 2 s` | Connect timeout |
| `# @no-redirect` | Don't follow redirects |
| `# @no-cookie-jar` | Don't send or store cookies |
| `# @no-log` | Don't write this run to history |
| `# @insecure` | Skip TLS certificate checks |
| `# @prompt otp One-time code` | Ask for `{{otp}}` before sending; with `--non-interactive` pass `--var otp=…` |
| `# @note Deletes for real` | Ask for confirmation before sending; the CLI refuses with exit 4 unless confirmed in a terminal |
| `# @assert …` | Check the response (see Assertions) |
| `# @no-auto-encoding` | Accepted for compatibility; no effect |

### Variables

- File variables: `@name = value` (may reference other variables).
- Environments live in one folder, `~/.config/respite/env/`, for every `.http` file wherever it
  is. `respite-<env>.json` is a flat `{"key": "value"}` object; shared values go in
  `respite.json`. Secrets live in the same file: values whose key contains `pass`, `pwd`,
  `secret`, `token`, `key` or `credential` are masked in output and history (there are no
  `.private.json` files; `check` reports leftover ones). A file picks its
  environment with `# @env NAME` on its first line; without it only `respite.json` applies.
  Env files next to `.http` files are not read; in CI pass them with `--env-file`.
- Dynamic:
  - `{{$uuid}}`, `{{$randomInt 1 100}}`
  - `{{$timestamp}}`, `{{$isoTimestamp}}`, `{{$localDatetime}}`
  - `{{$datetime "YYYY-MM-DD"}}`, `{{$datetime iso8601 7 d}}` — offsets in `y M w d h m s ms`
  - `{{$processEnv HOME}}`, `{{$dotenv KEY}}`
- From another request: `{{login.response.body.$.token}}` (JSONPath),
  `{{login.response.body.*}}` (whole body), `{{login.response.headers.X-Token}}`,
  `{{login.request.body.*}}`. The named request runs first when its response isn't cached;
  dependency cycles are errors.

### OAuth 2.0

Same syntax and config as JetBrains HTTP Client, so the file runs there too:

```http
GET {{host}}/me
Authorization: Bearer {{$auth.token("api")}}
```

```jsonc
// ~/.config/respite/env/respite-dev.json
{ "Security": { "Auth": { "api": {
  "Type": "OAuth2",
  "Grant Type": "Client Credentials",
  "Token URL": "https://auth.example.com/oauth/token",
  "Client ID": "respite",
  "Client Secret": "{{clientSecret}}",
  "Scope": "read write"
}}},
  "clientSecret": "…" }
```

- `{{$auth.token("id")}}` is the access token, `{{$auth.idToken("id")}}` the ID token.
- `Grant Type`:
  - `Client Credentials`;
  - `Password` — with `Username` and `Password`;
  - `Authorization Code` — with `Auth URL`. It opens the system browser and waits up to 5
    minutes for the redirect. `Redirect URL` must be a loopback address like
    `http://127.0.0.1:8765/callback`; without it a free port is used.
    `"PKCE": true` uses S256, or pass an object with `Code Challenge Method` (`SHA-256` /
    `Plain`) and `Code Verifier`;
  - `Device Authorization` — with `Device Auth URL`. It shows "Enter code … at …", opens the
    page and polls. `"Open Complete URI": true` opens the link with the code already filled in.
    `Start Polling After Browser` is accepted, but polling starts right away.
  - `Implicit` is not supported.
- Browser and device sign-ins need a person. Don't start them from an agent unless the user
  is at the machine. `respite auth token ID` prints the instructions on stderr, and the token
  stays cached for later runs.
- A config without `Client Secret` is a public client: `client_id` goes in the body.
- `Type: "Mock"` with `"Token"` (and `"ID Token"`) hands out a fixed token, with no request.
- Other fields:
  - `Client Credentials` — how the client authenticates: `basic` (default), `in body`, `none`;
  - `Custom Request Parameters` / `Custom Request Headers` — `"name": "v"`, a list, or
    `{"Value": …, "Use": "Everywhere" | "In Auth Request" | "In Token Request"}`;
  - `Acquire Automatically: false` — requests fail until `respite auth token ID` gets one.
- Config values may use `{{variables}}`. A config in `respite.json` applies to every
  environment; an environment's file can add just some fields (`"Client Secret"`): configs
  merge field by field. Configs always count as secret.
- The token is acquired before the first request that needs it, then reused. Near expiry it is
  renewed with the refresh token, falling back to the full grant. Changing the config drops it.
- Cache: `~/.respite/history/oauth.<env>.json`, owner-only. Tokens are masked in
  history.
- `check` reports `unknown_auth` for an id that no env file defines.

### Client certificates, CAs, proxy

Client certificates (mTLS) use JetBrains' `SSLConfiguration` in an env file. It applies to
every request of that environment, token requests included:

```jsonc
// respite-dev.json
{ "SSLConfiguration": {
  "clientCertificate": "certs/client.pem",       // or {"path": "certs/client.der", "format": "DER"}
  "clientCertificateKey": "certs/client.key",
  "caCertificate": "certs/dev-ca.pem",           // Respite's addition: extra trusted CA(s), a path or a list
  "verifyHostCertificate": false                 // skip certificate checks, like @insecure
}}
```

- Paths are relative to the env file that names them.
- Formats: PEM, or DER (`.der` / `.cer`, or `"format"`). The certificate file may also hold the key.
- P12/PFX, encrypted keys and `hasCertificatePassphrase` are not supported. The error gives the
  `openssl` command that converts or decrypts them.
- Fields merge across env files the same way as `Security.Auth`.
- The system trust store is always used; `caCertificate` adds to it.
- macOS rejects server certificates valid for more than 398 days, even when the CA is trusted.
  For a long-lived dev certificate, use `verifyHostCertificate: false`.
- Proxy: the system one by default (macOS settings, `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`). To
  override it, set `request.proxy` (`http://user:pass@host:port`) and `request.no_proxy`
  (`["localhost", ".internal"]`) in settings.json (Settings → Requests). It applies to every
  request.

### Bodies and output

- JSON / XML / text: written as is after the blank line.
- `< ./file.json` sends a file as is; `<@ ./file.json` substitutes `{{variables}}` in it.
- `application/x-www-form-urlencoded`: `a=1` with continuation lines `&b=2`.
- `multipart/form-data; boundary=B`: parts separated by `--B`, closed by `--B--`;
  a part's content can be `< ./file`.
- GraphQL: method `GRAPHQL`, the query, a blank line, then the variables JSON.
- `Authorization: Basic user pass` is base64-encoded automatically.
- `>> ./out/a.json` after the body saves the response; `>>! path` overwrites.

## Assertions

`# @assert <subject> <operator> [value]`, one per line, above the request line. The lines are
plain comments to VS Code REST Client and JetBrains, so the file still runs there.

- Subjects:
  - `status`, `header <name>`;
  - `body` — the whole body as text;
  - `body <JSONPath>` — e.g. `$.items[0].id`; a bare `a.b` means `$.a.b`;
  - `duration` — total time in ms; the value may use `ms` / `s` / `m`.
- Operators with a value:
  - `==` (alias `equals`) `!=` `<` `<=` `>` `>=` — numbers compare as numbers, otherwise as text;
  - `contains` (alias `includes`), `startsWith`, `endsWith`;
  - `matches` — a regex.
- Operators without a value: `exists` `isTrue` `isFalse` `isNumber` `isString` `isBoolean`
  `isArray`.
- The value may use `{{variables}}`; surrounding quotes are optional.
- A malformed line is an `invalid_assert` error in `check`.

In JSON output each request has `"tests": [{"line", "assert", "passed", "detail"}]`, where
`detail` is the actual value of a failed check. Any failure makes `run` exit 1.

## Workflow for testing an endpoint

1. Find or create the `.http` file next to the code: `respite list`; `respite env list`. Put
   `# @env <env>` on its first line when it should always run with one environment.
2. Add the request and its `# @assert` lines; run `respite check <file>`.
3. Run it: `respite run <file> <name> --output json --non-interactive` (add `--env <env>` to
   override the file's `# @env`).
4. Read `response` and `tests`. On failure, report the failed assert, its `detail`, and the
   status plus a body excerpt. Fix the code or the expectation, then run again.
5. For CI there is no `~/.config/respite/env/`: pass the env files with `--env-file`, e.g.
   `respite run <file> --env staging --env-file ci/respite-staging.json --fail-fast
   --non-interactive`; exit code ≠ 0 fails the job.
