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
async fn errors() {
    let cycle = "# @name a\nGET http://x/{{b.response.body.*}}\n###\n# @name b\nGET http://x/{{a.response.body.*}}\n";
    assert!(matches!(run(cycle, Path::new("/tmp"), "", 0).await, Err(Error::DependencyCycle(n)) if n == "a"));
    let undefined = "GET http://x/{{nope}}\n";
    assert!(matches!(run(undefined, Path::new("/tmp"), "", 0).await, Err(Error::Resolve(_))));
    let missing_file = "POST http://x\n\n< ./missing.bin\n";
    assert!(matches!(run(missing_file, Path::new("/tmp"), "", 0).await, Err(Error::File(..))));
}
