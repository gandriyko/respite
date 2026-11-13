use std::collections::HashMap;
use std::path::Path;

use http_env::Variable;
use http_exec::{Context, Error, Executor, Store};
use wiremock::matchers::{body_json, body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn env(base: &str) -> HashMap<String, Variable> {
    HashMap::from([("baseUrl".into(), Variable { value: base.into(), source: "env".into(), secret: false })])
}

async fn run(src: &str, dir: &Path, base: &str, index: usize) -> Result<http_exec::Exchange, Error> {
    let file = http_syntax::parse(src);
    let path = dir.join("api.http");
    let (env, overrides) = (env(base), HashMap::new());
    let cx = Context { path: &path, file: &file, env: &env, overrides: &overrides };
    Executor::default().run(&cx, index).await
}

#[tokio::test]
async fn dependency_auto_run_and_jsonpath() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "T-1"})))
        .expect(1).mount(&server).await;
    Mock::given(method("GET")).and(path("/me")).and(header("authorization", "Bearer T-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("anya")).expect(1).mount(&server).await;

    let src = "@base = {{baseUrl}}\n\
               # @name login\n\
               POST {{base}}/auth\n\
               Content-Type: application/json\n\
               \n\
               {\"user\": \"anya\"}\n\
               \n\
               ###\n\
               @token = {{login.response.body.$.access_token}}\n\
               GET {{base}}/me\n\
               Authorization: Bearer {{token}}\n";
    let ex = run(src, Path::new("/tmp"), &server.uri(), 1).await.unwrap();
    assert_eq!((ex.response.status, ex.response.body.as_slice()), (200, b"anya".as_slice()));
    assert_eq!(ex.dependencies, ["login"]);
    assert_eq!(ex.response.reason, "OK");
}

#[tokio::test]
async fn asserts() {
    let server = MockServer::start().await;
    Mock::given(path("/u"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 7, "name": "Ann", "tags": ["a"], "on": false})))
        .mount(&server).await;
    let src = "@who = Ann\n\
               # @assert status == 201\n\
               # @assert status >= 200\n\
               # @assert header content-type contains json\n\
               # @assert body $.name == {{who}}\n\
               # @assert body name == \"Ann\"\n\
               # @assert body id isNumber\n\
               # @assert body tags isArray\n\
               # @assert body on isFalse\n\
               # @assert body missing exists\n\
               # @assert body $.id == 8\n\
               # @assert body matches \"id\":\\s*7\n\
               # @assert duration < 30 s\n\
               # @assert header x-none startsWith a\n\
               # @assert body $.name > 3\n\
               # @assert status ~ 1\n\
               GET {{baseUrl}}/u\n";
    let ex = run(src, Path::new("/tmp"), &server.uri(), 0).await.unwrap();
    let r: Vec<_> = ex.tests.iter().map(|t| (t.passed, t.detail.as_str())).collect();
    assert_eq!(r, [
        (true, ""), (true, ""), (true, ""), (true, ""), (true, ""), (true, ""), (true, ""), (true, ""),
        (false, "actual: (missing)"),
        (false, "actual: 7"),
        (true, ""), (true, ""),
        (false, "actual: (missing)"),
        (false, "'>' compares numbers; actual: Ann"),
        (false, "unknown operator '~'"),
    ]);
}

#[tokio::test]
async fn bodies_auth_graphql_redirect() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.json"), "{\"id\": \"{{baseUrl}}\"}").unwrap();
    std::fs::write(dir.path().join("raw.txt"), "{{kept}}").unwrap();
    let base = server.uri();

    Mock::given(path("/file")).and(body_string(format!("{{\"id\": \"{base}\"}}\n{{{{kept}}}}")))
        .respond_with(ResponseTemplate::new(201)).mount(&server).await;
    Mock::given(path("/basic")).and(header("authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(ResponseTemplate::new(204)).mount(&server).await;
    Mock::given(path("/gql")).and(method("POST"))
        .and(body_json(serde_json::json!({"query": "query { me { id } }", "variables": {"a": 1}})))
        .respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(path("/form")).and(body_string("user=anya&password=x"))
        .respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(path("/moved"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere")).mount(&server).await;

    let src = "POST {{baseUrl}}/file\n\n<@ ./t.json\n< ./raw.txt\n\
               ###\nGET {{baseUrl}}/basic\nAuthorization: Basic user pass\n\
               ###\nGRAPHQL {{baseUrl}}/gql\n\nquery { me { id } }\n\n{\"a\": 1}\n\
               ###\nPOST {{baseUrl}}/form\nContent-Type: application/x-www-form-urlencoded\n\nuser=anya\n&password=x\n\
               ###\n# @no-redirect\nGET {{baseUrl}}/moved\n\
               >>! ./out/moved.txt\n";
    let status = async |i| run(src, dir.path(), &base, i).await.unwrap().response.status;
    assert_eq!(status(0).await, 201);
    assert_eq!(status(1).await, 204);
    assert_eq!(status(2).await, 200);
    assert_eq!(status(3).await, 200);
    assert_eq!(status(4).await, 302);
    assert!(dir.path().join("out/moved.txt").exists());
}

#[test]
fn literal_variable_in_url_is_named() {
    // The GUI sends an undefined variable as written (an override equal to `{{name}}`).
    let file = http_syntax::parse("GET {{host}}/a\nX-Note: {{host}}\n");
    let path = Path::new("/tmp/api.http");
    let (env, overrides) = (HashMap::new(), HashMap::from([("host".to_string(), "{{host}}".to_string())]));
    let cx = Context { path, file: &file, env: &env, overrides: &overrides };
    let err = Executor::default().prepare(&cx, 0).unwrap_err().to_string();
    assert!(err.contains("the URL has {{host}}, which is not defined"), "{err}");
}

#[tokio::test]
async fn redirect_chain_recorded() {
    let server = MockServer::start().await;
    Mock::given(path("/a")).respond_with(ResponseTemplate::new(301).insert_header("location", "/b")).mount(&server).await;
    Mock::given(path("/b")).respond_with(ResponseTemplate::new(302).insert_header("location", "/c")).mount(&server).await;
    Mock::given(path("/c")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let base = server.uri();
    let r = run("GET {{baseUrl}}/a\n", Path::new("/tmp"), &base, 0).await.unwrap().response;
    assert_eq!((r.status, r.url.as_str()), (200, format!("{base}/c").as_str()));
    let hops: Vec<_> = r.redirects.iter().map(|h| (h.status, h.from.as_str(), h.to.as_str())).collect();
    assert_eq!(hops, [(301, format!("{base}/a").as_str(), format!("{base}/b").as_str()), (302, format!("{base}/b").as_str(), format!("{base}/c").as_str())]);
}

#[tokio::test]
async fn history_cache_and_cookies_survive_restart() {
    let server = MockServer::start().await;
    Mock::given(path("/auth"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"t": "T"}))
                .insert_header("set-cookie", "sid=42; Max-Age=3600; Path=/"),
        )
        .expect(1).mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer T")).and(header("cookie", "sid=42"))
        .respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let (base, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let file = http_syntax::parse(
        "# @name login\nPOST {{baseUrl}}/auth\n###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{login.response.body.$.t}}\n",
    );
    let path = ws.path().join("api.http");
    let (env, overrides) = (env(&server.uri()), HashMap::new());
    let cx = Context { path: &path, file: &file, env: &env, overrides: &overrides };
    let executor = || Executor::new(Some(Store::open(base.path(), ws.path()).unwrap()), Some("dev"), "cli");

    executor().run(&cx, 0).await.unwrap();
    // A fresh executor = app restart: `login` comes from history, the cookie from disk.
    let ex = executor().run(&cx, 1).await.unwrap();
    assert_eq!(ex.response.status, 200);
    assert!(ex.dependencies.is_empty());

    let entries = Store::open(base.path(), ws.path()).unwrap().list();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, ex.history_id.unwrap());
    assert_eq!((entries[0].file.as_str(), entries[0].source.as_str()), ("api.http", "cli"));
    assert_eq!(entries[0].request_headers.iter().find(|(n, _)| n == "Authorization").unwrap().1, "••••");
}

#[tokio::test]
async fn oauth2() {
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    let token = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=password"))
        .and(body_string_contains("username=ann"))
        .and(body_string_contains("password=pw"))
        .and(body_string_contains("client_id=c"))
        .and(body_string_contains("audience=x"))
        .respond_with(token(serde_json::json!({"access_token": "A1", "expires_in": 1, "refresh_token": "R1", "id_token": "I1"})))
        .expect(1).mount(&server).await;
    Mock::given(path("/token")).and(body_string_contains("grant_type=refresh_token")).and(body_string_contains("refresh_token=R1"))
        .respond_with(token(serde_json::json!({"access_token": "A2", "expires_in": "3600"})))
        .expect(1).mount(&server).await;
    // No secret: a public client, identified in the body.
    Mock::given(path("/token")).and(body_string_contains("client_id=bad"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "invalid_client"})))
        .mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer A1")).and(header("x-id", "I1"))
        .respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer A2")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer M")).respond_with(ResponseTemplate::new(202)).mount(&server).await;

    let base = server.uri();
    let auth = |id: &str, cfg: serde_json::Value| (format!("$auth.{id}"), Variable { value: cfg.to_string(), source: "env".into(), secret: true });
    let mut env = env(&base);
    env.extend([
        auth("pw", serde_json::json!({
            "Grant Type": "Password", "Token URL": format!("{base}/token"), "Client ID": "c", "Client Credentials": "in body",
            "Username": "ann", "Password": "{{pass}}",
            "Custom Request Parameters": {"audience": {"Value": "x", "Use": "In Token Request"}, "prompt": {"Value": "login", "Use": "In Auth Request"}},
        })),
        auth("mock", serde_json::json!({"Type": "Mock", "Token": "M"})),
        auth("bad", serde_json::json!({"Grant Type": "Client Credentials", "Token URL": format!("{base}/token"), "Client ID": "bad"})),
        auth("manual", serde_json::json!({"Type": "Mock", "Token": "M", "Acquire Automatically": false})),
        ("pass".into(), Variable { value: "pw".into(), source: "env".into(), secret: true }),
    ]);
    let file = http_syntax::parse(
        "GET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"pw\")}}\nX-Id: {{$auth.idToken(\"pw\")}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token('mock')}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"bad\")}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"manual\")}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.idToken(\"mock\")}}\n",
    );
    let (path, overrides) = (Path::new("/tmp/api.http"), HashMap::new());
    let cx = Context { path, file: &file, env: &env, overrides: &overrides };
    let exec = Executor::default();

    // Acquired once, reused while fresh, then refreshed with the kept refresh token.
    assert_eq!(exec.run(&cx, 0).await.unwrap().response.status, 200);
    assert_eq!(exec.run(&cx, 0).await.unwrap().response.status, 200);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let ex = exec.run(&cx, 0);
    // The refreshed token has no id_token, so `idToken` asks again and finds none.
    assert!(matches!(ex.await, Err(Error::Auth { id, message }) if id == "pw" && message.contains("id_token")));
    assert_eq!(exec.cached_token("pw").unwrap().refresh_token.as_deref(), Some("R1"));

    assert_eq!(exec.run(&cx, 1).await.unwrap().response.status, 202);
    assert!(matches!(exec.run(&cx, 2).await, Err(Error::Auth { message, .. }) if message.contains("401") && message.contains("invalid_client")));
    assert!(matches!(exec.run(&cx, 3).await, Err(Error::Auth { message, .. }) if message.contains("Acquire Automatically")));
    assert_eq!(exec.auth_token(&cx, "manual", false).await.unwrap().access_token, "M");
    assert!(matches!(exec.run(&cx, 4).await, Err(Error::Auth { id, .. }) if id == "mock"));
}

/// Authorization Code + PKCE and Device Authorization, with the "browser" played by `open_url`.
#[tokio::test]
async fn oauth2_browser_and_device() {
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    let base = server.uri();
    let json = |v: serde_json::Value| ResponseTemplate::new(200).set_body_json(v);
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=C1"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains("client_id=web"))
        .respond_with(json(serde_json::json!({"access_token": "A3", "expires_in": 3600})))
        .expect(1).mount(&server).await;
    Mock::given(path("/device")).and(body_string_contains("client_id=tv"))
        .respond_with(json(serde_json::json!({
            "device_code": "D", "user_code": "U-1", "verification_uri": "https://v", "verification_uri_complete": "https://v?c=U-1",
            "interval": 0, "expires_in": 60,
        })))
        .expect(1).mount(&server).await;
    Mock::given(path("/token")).and(body_string_contains("device_code=D"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "authorization_pending"})))
        .up_to_n_times(1).with_priority(1).expect(1).mount(&server).await;
    Mock::given(path("/token")).and(body_string_contains("device_code=D"))
        .respond_with(json(serde_json::json!({"access_token": "A4"}))).with_priority(2).expect(1).mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer A3")).respond_with(ResponseTemplate::new(203)).mount(&server).await;
    Mock::given(path("/me")).and(header("authorization", "Bearer A4")).respond_with(ResponseTemplate::new(204)).mount(&server).await;

    let auth = |id: &str, cfg: serde_json::Value| (format!("$auth.{id}"), Variable { value: cfg.to_string(), source: "env".into(), secret: false });
    let mut env = env(&base);
    env.extend([
        auth("web", serde_json::json!({
            "Grant Type": "Authorization Code", "Auth URL": format!("{base}/authorize"), "Token URL": format!("{base}/token"),
            "Client ID": "web", "PKCE": true, "Scope": "openid",
            "Custom Request Parameters": {"prompt": {"Value": "login", "Use": "In Auth Request"}},
        })),
        auth("tv", serde_json::json!({
            "Grant Type": "Device Authorization", "Device Auth URL": format!("{base}/device"), "Token URL": format!("{base}/token"),
            "Client ID": "tv", "Open Complete URI": true,
        })),
        auth("remote", serde_json::json!({
            "Grant Type": "Authorization Code", "Auth URL": format!("{base}/authorize"), "Token URL": format!("{base}/token"),
            "Client ID": "web", "Redirect URL": "https://example.com/cb",
        })),
    ]);
    let file = http_syntax::parse(
        "GET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"web\")}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"tv\")}}\n\
         ###\nGET {{baseUrl}}/me\nAuthorization: Bearer {{$auth.token(\"remote\")}}\n",
    );
    let (path, overrides) = (Path::new("/tmp/api.http"), HashMap::new());
    let cx = Context { path, file: &file, env: &env, overrides: &overrides };

    let (notices, opened) = (Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])));
    let exec = Executor::default();
    let (n, o) = (notices.clone(), opened.clone());
    exec.set_auth_ui(http_exec::AuthUi {
        notice: Arc::new(move |m| n.lock().unwrap().push(m)),
        open_url: Arc::new(move |url| {
            o.lock().unwrap().push(url.to_string());
            let url = reqwest::Url::parse(url).unwrap();
            let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let Some(redirect) = q.get("redirect_uri").cloned() else { return };
            let back = format!("{redirect}?code=C1&state={}", q["state"]);
            // The browser: a stray request first, then the redirect with the code.
            tokio::spawn(async move {
                let _ = reqwest::get(format!("{redirect}/../favicon.ico")).await;
                assert!(reqwest::get(back).await.unwrap().text().await.unwrap().contains("Signed in"));
            });
        }),
    });

    assert_eq!(exec.run(&cx, 0).await.unwrap().response.status, 203);
    let authorize = opened.lock().unwrap()[0].clone();
    for part in ["response_type=code", "client_id=web", "code_challenge_method=S256", "prompt=login", "redirect_uri=http%3A%2F%2F127.0.0.1%3A"] {
        assert!(authorize.contains(part), "{part} in {authorize}");
    }

    assert_eq!(exec.run(&cx, 1).await.unwrap().response.status, 204);
    assert_eq!(opened.lock().unwrap()[1], "https://v?c=U-1");
    assert!(notices.lock().unwrap().iter().any(|m| m == "Enter code U-1 at https://v"));

    let remote = exec.run(&cx, 2).await;
    assert!(matches!(remote, Err(Error::Auth { message, .. }) if message.contains("loopback")));
}

/// `Options::proxy`: a plain-HTTP request to an unresolvable host goes through it; `no_proxy`
/// sends it direct (and it fails).
#[tokio::test]
async fn proxy_and_no_proxy() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET")).and(path("/via")).and(header("host", "respite.invalid"))
        .respond_with(ResponseTemplate::new(200).set_body_string("proxied")).expect(1).mount(&proxy).await;
    let file = http_syntax::parse("GET http://respite.invalid/via\n");
    let (env, overrides) = (HashMap::new(), HashMap::new());
    let cx = Context { path: Path::new("/tmp/api.http"), file: &file, env: &env, overrides: &overrides };
    let exec = Executor::default();
    exec.set_options(http_exec::Options { proxy: Some(proxy.uri()), ..Default::default() });
    let ex = exec.run(&cx, 0).await.unwrap();
    assert_eq!(ex.response.body, b"proxied");

    exec.set_options(http_exec::Options { proxy: Some(proxy.uri()), no_proxy: vec!["respite.invalid".into()], ..Default::default() });
    assert!(matches!(exec.run(&cx, 0).await, Err(Error::Http(_))));
    exec.set_options(http_exec::Options { proxy: Some("not a url".into()), ..Default::default() });
    assert!(matches!(exec.run(&cx, 0).await, Err(Error::BadRequest(m)) if m.contains("invalid proxy")));
}

#[tokio::test]
async fn errors() {
    let cycle = "# @name a\nGET http://x/{{b.response.body.*}}\n###\n# @name b\nGET http://x/{{a.response.body.*}}\n";
    assert!(matches!(run(cycle, Path::new("/tmp"), "", 0).await, Err(Error::DependencyCycle(n)) if n == "a"));
    let undefined = "GET http://x/{{nope}}\n";
    assert!(matches!(run(undefined, Path::new("/tmp"), "", 0).await, Err(Error::Resolve(_))));
    let missing_file = "POST http://x\n\n< ./missing.bin\n";
    assert!(matches!(run(missing_file, Path::new("/tmp"), "", 0).await, Err(Error::File(..))));
}
