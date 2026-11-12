use super::*;

fn imp(cmd: &str) -> String {
    import(cmd, &Options { keep_original: false, ..Options::default() }).unwrap()
}

/// curl → block → parse → export_raw → block again must be stable.
fn roundtrip(cmd: &str) {
    let opts = Options { keep_original: false, no_redirect_without_l: false, ..Options::default() };
    let block = import(cmd, &opts).unwrap();
    let file = http_syntax::parse(&block);
    assert!(file.diagnostics.is_empty(), "{block}\n{:?}", file.diagnostics);
    let again = import(&export_raw(&file.requests[0]), &Options { keep_original: false, ..Options::default() }).unwrap();
    // export adds -L, so the second import has no @no-redirect either
    assert_eq!(block, again, "\n--- first\n{block}\n--- second\n{again}");
}

#[test]
fn spec_example() {
    let cmd = r#"curl 'https://api.example.com/v1/orders?limit=20' \
  -X POST \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer eyJhbGciOi...' \
  -b 'session=abc123' \
  --data-raw '{"sku":"A-1","qty":2}' \
  --compressed"#;
    let block = import(cmd, &Options::default()).unwrap();
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(lines[0], "### POST /v1/orders");
    assert!(lines[1].starts_with("# curl 'https://api.example.com/v1/orders?limit=20' -X POST") && lines[1].ends_with("(original truncated)"));
    assert_eq!(&lines[2..], [
        "# @no-redirect",
        "POST https://api.example.com/v1/orders?limit=20",
        "Content-Type: application/json",
        "Authorization: Bearer eyJhbGciOi...",
        "Cookie: session=abc123",
        "Accept-Encoding: gzip, deflate, br",
        "",
        "{",
        "  \"sku\": \"A-1\",",
        "  \"qty\": 2",
        "}",
    ]);
    roundtrip(cmd);
}

#[test]
fn chrome_bash_with_ansi_c_body_and_noise() {
    let cmd = "curl 'https://x.dev/api?a=1&b=2&c=3' \\\n  -H 'accept: */*' \\\n  -H 'sec-ch-ua: \"Chromium\"' \\\n  -H 'sec-fetch-mode: cors' \\\n  -H 'priority: u=1, i' \\\n  --data-raw $'{\"note\":\"it\\'s\\\\n ok\"}'";
    let block = imp(cmd);
    assert!(!block.contains("sec-") && !block.contains("priority"), "{block}");
    assert!(block.contains("POST https://x.dev/api\n    ?a=1\n    &b=2\n    &c=3\naccept: */*\nContent-Type: application/x-www-form-urlencoded\n"), "{block}");
    assert!(block.contains(r#""note": "it's\n ok""#), "{block}");
    roundtrip(cmd);
}

#[test]
fn chrome_cmd_format() {
    let cmd = "curl ^\"https://x.dev/api^\" ^\n  -H ^\"content-type: application/json^\" ^\n  --data-raw ^\"^{^\\^\"a^\\^\":1^}^\"";
    let block = imp(cmd);
    assert!(block.contains("POST https://x.dev/api\ncontent-type: application/json\n\n{\n  \"a\": 1\n}"), "{block}");
}

#[test]
fn powershell_backticks_and_bundled_flags() {
    let block = imp("curl.exe -sSLk `\n  https://x.dev/a `\n  -XDELETE -m 5 --connect-timeout=2 --http2");
    assert_eq!(block, "### DELETE /a\n# @insecure\n# @timeout 5 s\n# @connection-timeout 2 s\nDELETE https://x.dev/a HTTP/2\n");
}

#[test]
fn form_get_user_files_output() {
    let block = imp("curl -F title=Звіт -F 'file=@docs/report.pdf;type=application/pdf' https://x.dev/upload -o out.json");
    assert!(block.contains("Content-Type: multipart/form-data; boundary=RespiteFormBoundary\n\n--RespiteFormBoundary\nContent-Disposition: form-data; name=\"title\"\n\nЗвіт\n--RespiteFormBoundary\nContent-Disposition: form-data; name=\"file\"; filename=\"report.pdf\"\nContent-Type: application/pdf\n\n< ./docs/report.pdf\n--RespiteFormBoundary--\n\n>> ./out.json"), "{block}");

    assert!(imp("curl -G -d q=rust -d page=2 https://x.dev/s").contains("GET https://x.dev/s?q=rust&page=2\n"));
    assert!(imp("curl -u anya:s3cret https://x.dev").contains("Authorization: Basic anya s3cret"));
    assert!(imp("curl -d @body.json -H 'Content-Type: application/json' https://x.dev").ends_with("\n\n< ./body.json\n"));
    assert!(imp("curl --json '{\"a\":1}' https://x.dev").contains("Content-Type: application/json\nAccept: application/json\n"));
    assert!(imp("curl --data-urlencode 'q=a b&c' https://x.dev").ends_with("q=a%20b%26c\n"));
    assert!(imp("curl -I https://x.dev").contains("HEAD https://x.dev\n"));
    assert!(imp("curl --frobnicate https://x.dev").contains("# ignored: --frobnicate\n"));
    roundtrip("curl -X PUT https://x.dev/form -d user=anya -d 'password=p w'");
}

#[test]
fn variables_and_multiple_commands() {
    let vars = [("baseUrl".to_string(), "https://api.example.com/v1".to_string()), ("token".to_string(), "abc123xyz".to_string())];
    let opts = Options { keep_original: false, vars: &vars, ..Options::default() };
    let blocks = import_all(
        "curl https://api.example.com/v1/me -H 'Authorization: Bearer abc123xyz'\n\ncurl -L 'https://other.dev/x'\n",
        &opts,
    )
    .unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].contains("GET {{baseUrl}}/me\nAuthorization: Bearer {{token}}\n"), "{}", blocks[0]);
    assert_eq!(blocks[1], "### GET /x\nGET https://other.dev/x\n");
    assert!(import_all("echo hi", &opts).is_err());
    assert!(is_curl("  curl https://x") && !is_curl("curly"));
}

#[test]
fn export_quotes_and_directives() {
    let file = http_syntax::parse(
        "# @no-redirect\n# @timeout 500 ms\nPOST {{base}}/x HTTP/2\nX-Q: it's\n\n{\"a\": 1}\n###\nGET https://x.dev/a?b=1\n###\nPOST https://x.dev\n\n< ./big.bin\n",
    );
    assert_eq!(
        export_raw(&file.requests[0]),
        "curl \\\n  -X POST \\\n  '{{base}}/x' \\\n  --http2 \\\n  --max-time 0.5 \\\n  -H 'X-Q: it'\\''s' \\\n  --data-raw '{\"a\": 1}'"
    );
    assert_eq!(export_raw(&file.requests[1]), "curl \\\n  'https://x.dev/a?b=1' \\\n  -L");
    assert!(export_raw(&file.requests[2]).ends_with("--data-binary @./big.bin"));

    let sent = SentRequest { method: "GET".into(), url: "https://x.dev/a".into(), headers: vec![("A".into(), "b".into())], body: vec![] };
    assert_eq!(export(&sent, &file.requests[1]), "curl \\\n  https://x.dev/a \\\n  -L \\\n  -H 'A: b'");
}
