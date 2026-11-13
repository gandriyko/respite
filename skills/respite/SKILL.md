---
name: respite
description: Send HTTP requests and test an API with the Respite CLI and plain `.http` files (VS Code REST Client / JetBrains syntax), including declarative `# @assert` checks, environments, request chaining, imports from Postman / Insomnia / OpenAPI / Bruno / HAR and cURL conversion. Use when the user asks to call, try, smoke-test or regression-test an HTTP endpoint, write or fix `.http` / `.rest` files, import an API collection, or check an API in CI, and `respite` is installed or the project has `.http` files.
---

# Respite: HTTP requests and API tests from `.http` files

`respite` is one binary: the desktop app with no arguments (or a path), a CLI with a subcommand.
From an agent, use the CLI only; never launch the GUI.

## Rules for agents

- Pass `--non-interactive` to `run`: otherwise a `@prompt` or `@note` waits for input.
- Pass `--output json` (or `jsonl`) when you read the result; without a TTY JSON is the default.
- Before sending anything with side effects, check it with `--dry-run` or `show --resolved`.
- Never write secrets into `.http` files; use environment files (below).
- After editing a `.http` file, run `respite check` on it.

## Commands

Requests are addressed by `@name`, `###` title, `:LINE` (any line of the block) or `#N` (1-based).

| Command | What it does |
|---|---|
| `respite list [PATH]` | Requests in a file or directory: file, line, name, method, URL |
| `respite show FILE REQUEST [--resolved]` | The request block; `--resolved` substitutes variables |
| `respite run FILE [REQUESTS]...` | Send requests (none named = all, in file order) |
| `respite check [PATHS]...` | Diagnostics only, sends nothing (pre-commit / CI) |
| `respite import PATH [--to DIR]` | Postman / Insomnia / OpenAPI / Swagger / Bruno folder / HAR → `.http` files in a new folder |
| `respite import-curl` | cURL command(s) from stdin → request block(s) |
| `respite export-curl FILE REQUEST [--raw]` | Request → cURL; `--raw` keeps `{{variables}}` |
| `respite env list [--dir DIR]` | Environment names visible from a directory |
| `respite env show NAME [--dir DIR]` | Merged variables of one environment, secrets masked |
| `respite auth token ID [--force] [--dir DIR]` | OAuth 2.0 token of a config: cached while fresh, else refreshed or acquired; `--force` runs the full grant |
| `respite auth clear [--env NAME] [--dir DIR]` | Forget the cached OAuth 2.0 tokens of one environment |
| `respite history [--last N]` | Past executions of this workspace (default 20) |

`list`, `check`, `import`, `env`, `history` and `run` take `--output human|json|jsonl`.

Variables (`run`, `show`, `export-curl`, `auth token`):

- `--env NAME` — environment (`dev` reads `respite-dev.json`).
- `--var KEY=VALUE` — override any variable, highest priority, repeatable.
- `--env-file PATH` — extra env file above the chain, named `respite-<env>(.private).json`, repeatable.

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

## File syntax

```http
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
- Environments: `respite-<env>.json` — a flat `{"key": "value"}` object. Shared values go in
  `respite.json`. Secrets go in git-ignored `respite-<env>.private.json` / `respite.private.json`;
  their values are masked in output and history. Files are found in every folder from the
  workspace root down to the `.http` file, and deeper folders win.
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
// respite-dev.json (committed)
{ "Security": { "Auth": { "api": {
  "Type": "OAuth2",
  "Grant Type": "Client Credentials",
  "Token URL": "https://auth.example.com/oauth/token",
  "Client ID": "respite",
  "Client Secret": "{{clientSecret}}",
  "Scope": "read write"
}}}}
// respite-dev.private.json (git-ignored): { "clientSecret": "…" }
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
- Config values may use `{{variables}}`; keep secrets in `.private` files. A private file can
  hold just the fields it adds (`"Client Secret"`): configs merge field by field.
- The token is acquired before the first request that needs it, then reused. Near expiry it is
  renewed with the refresh token, falling back to the full grant. Changing the config drops it.
- Cache: `~/.respite/history/<project>/oauth.<env>.json`, owner-only. Tokens are masked in
  history.
- `check` reports `unknown_auth` for an id that no env file defines.

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

1. Find or create the `.http` file next to the code: `respite list`; `respite env list`.
2. Add the request and its `# @assert` lines; run `respite check <file>`.
3. Run it: `respite run <file> <name> --env <env> --output json --non-interactive`.
4. Read `response` and `tests`. On failure, report the failed assert, its `detail`, and the
   status plus a body excerpt. Fix the code or the expectation, then run again.
5. For CI: `respite run <file> --env <env> --fail-fast --non-interactive`; exit code ≠ 0 fails
   the job.
