use super::*;

/// Every generated file must parse without errors.
fn assert_valid(c: &Collection) -> Vec<(PathBuf, String)> {
    let files = render(c);
    for (path, text) in &files {
        let parsed = http_syntax::parse(text);
        let errors: Vec<_> = parsed.diagnostics.iter().filter(|d| d.severity == http_syntax::Severity::Error).collect();
        assert!(errors.is_empty(), "{}:\n{text}\n{errors:?}", path.display());
    }
    files
}

fn file<'a>(files: &'a [(PathBuf, String)], name: &str) -> &'a str {
    &files.iter().find(|(p, _)| p == Path::new(name)).unwrap_or_else(|| panic!("no {name} in {:?}", files.iter().map(|f| &f.0).collect::<Vec<_>>())).1
}

#[test]
fn postman_collection_and_environment() {
    let json = r#"{
      "info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
      "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "{{token}}"}]},
      "variable": [{"key": "baseUrl", "value": "https://api.shop.dev"}],
      "item": [
        {"name": "Health", "request": {"method": "GET", "url": "{{baseUrl}}/health", "auth": {"type": "noauth"}}},
        {"name": "Orders", "item": [
          {"name": "Create order", "event": [{"listen": "test", "script": {"exec": ["pm.test('ok')"]}}],
           "request": {"method": "POST", "url": {"raw": "{{baseUrl}}/orders?dry=1"},
             "header": [{"key": "X-Trace", "value": "1"}, {"key": "X-Off", "value": "x", "disabled": true}],
             "body": {"mode": "raw", "raw": "{\"sku\":\"A-1\"}", "options": {"raw": {"language": "json"}}}}},
          {"name": "Upload", "request": {"method": "POST", "url": "{{baseUrl}}/files",
             "body": {"mode": "formdata", "formdata": [{"key": "title", "value": "Q1"}, {"key": "file", "type": "file", "src": "report.pdf"}]}}},
          {"name": "Login", "request": {"method": "POST", "url": "{{baseUrl}}/login", "auth": {"type": "basic", "basic": [{"key": "username", "value": "anya"}, {"key": "password", "value": "pw"}]},
             "body": {"mode": "urlencoded", "urlencoded": [{"key": "a", "value": "1"}, {"key": "b", "value": "2"}]}}}
        ]}
      ]
    }"#;
    let c = import_str(json, "x").unwrap();
    assert_eq!((c.name.as_str(), c.format), ("Shop", "Postman collection"));
    assert_eq!(c.envs["$shared"], [("baseUrl".to_string(), "https://api.shop.dev".to_string())]);
    let files = assert_valid(&c);
    let shop = file(&files, "Shop.http");
    assert!(shop.contains("### Health\nGET {{baseUrl}}/health\n") && !shop.contains("Authorization"), "{shop}");
    let orders = file(&files, "Orders.http");
    assert!(orders.contains("POST {{baseUrl}}/orders?dry=1\nX-Trace: 1\nAuthorization: Bearer {{token}}\nContent-Type: application/json\n\n{\n  \"sku\": \"A-1\"\n}"), "{orders}");
    assert!(!orders.contains("X-Off"));
    assert!(orders.contains("# Postman scripts (pre-request / tests) were not converted"));
    assert!(orders.contains("< ./report.pdf") && orders.contains("boundary=RespiteFormBoundary"));
    assert!(orders.contains("Authorization: Basic anya pw") && orders.contains("a=1\n&b=2"));
    assert_eq!(c.warnings.len(), 1);

    let env = import_str(r#"{"name": "prod", "_postman_variable_scope": "environment",
        "values": [{"key": "host", "value": "api", "enabled": true}, {"key": "token", "value": "s", "type": "secret", "enabled": true},
                   {"key": "off", "value": "x", "enabled": false}]}"#, "x").unwrap();
    assert_eq!(env.envs["prod"], [("host".to_string(), "api".to_string())]);
    assert_eq!(env.private_envs["prod"], [("token".to_string(), "s".to_string())]);
}

#[test]
fn insomnia_v4_and_v5() {
    let v4 = r#"{"_type": "export", "__export_format": 4, "resources": [
      {"_id": "wrk_1", "_type": "workspace", "name": "Billing"},
      {"_id": "fld_1", "_type": "request_group", "parentId": "wrk_1", "name": "Invoices"},
      {"_id": "req_1", "_type": "request", "parentId": "fld_1", "name": "List", "method": "GET", "url": "{{ _.baseUrl }}/invoices",
       "parameters": [{"name": "page", "value": "1"}], "authentication": {"type": "bearer", "token": "{{ _.token }}"}},
      {"_id": "req_2", "_type": "request", "parentId": "wrk_1", "name": "Ping", "method": "POST", "url": "{{_.baseUrl}}/ping",
       "body": {"mimeType": "application/json", "text": "{\"a\":1}"}},
      {"_id": "env_1", "_type": "environment", "parentId": "wrk_1", "name": "Base", "data": {"baseUrl": "https://b.dev"}},
      {"_id": "env_2", "_type": "environment", "parentId": "env_1", "name": "Staging", "data": {"token": "t", "port": 8080}}
    ]}"#;
    let c = import_str(v4, "x").unwrap();
    assert_eq!(c.name, "Billing");
    assert_eq!(c.envs["Staging"], [("token".to_string(), "t".to_string()), ("port".to_string(), "8080".to_string())]);
    let files = assert_valid(&c);
    assert!(file(&files, "Invoices.http").contains("GET {{baseUrl}}/invoices?page=1\nAuthorization: Bearer {{token}}\n"));
    assert!(file(&files, "Billing.http").contains("Content-Type: application/json\n\n{\n  \"a\": 1\n}"));

    let v5 = "type: collection.insomnia.rest/5.0\nname: Hooks\ncollection:\n  - name: Admin\n    children:\n      - name: Users\n        method: GET\n        url: \"{{ _.host }}/users\"\n        headers:\n          - name: Accept\n            value: application/json\nenvironments:\n  name: Base\n  data:\n    host: https://h.dev\n  subEnvironments:\n    - name: dev\n      data:\n        host: http://localhost\n";
    let c = import_str(v5, "x").unwrap();
    assert_eq!((c.format, c.envs["dev"][0].1.as_str()), ("Insomnia v5", "http://localhost"));
    let files = assert_valid(&c);
    assert!(file(&files, "Admin.http").contains("GET {{host}}/users\nAccept: application/json\n"));
}

#[test]
fn openapi3_yaml_and_swagger2() {
    let yaml = r##"
openapi: 3.0.3
info: {title: Pets}
servers: [{url: "https://{env}.pets.dev/v1", variables: {env: {default: api}}}]
components:
  securitySchemes: {bearerAuth: {type: http, scheme: bearer}}
  schemas:
    Pet:
      type: object
      properties: {id: {type: string, format: uuid}, name: {type: string, example: Rex}, tags: {type: array, items: {type: string}}}
security: [{bearerAuth: []}]
paths:
  /pets/{petId}:
    parameters: [{name: petId, in: path, required: true}]
    get: {tags: [pets], summary: Get pet, parameters: [{name: expand, in: query}]}
    put:
      tags: [pets]
      operationId: updatePet
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Pet"}}}}
  /health:
    get: {summary: Health, security: []}
    post: {summary: Ping, security: [], requestBody: {content: {}}}
"##;
    let c = import_str(yaml, "x").unwrap();
    assert_eq!(c.envs["$shared"], [("baseUrl".to_string(), "https://api.pets.dev/v1".to_string())]);
    assert_eq!(c.private_envs["$shared"][0].0, "token");
    let files = assert_valid(&c);
    let pets = file(&files, "pets.http");
    assert!(pets.contains("### Get pet\n# optional query: expand\n@petId =\nGET {{baseUrl}}/pets/{{petId}}\nAuthorization: Bearer {{token}}\n"), "{pets}");
    // Declared once per file.
    assert_eq!(pets.matches("@petId").count(), 1, "{pets}");
    assert!(pets.contains("### updatePet") && pets.contains("\"id\": \"{{$uuid}}\"") && pets.contains("\"name\": \"Rex\""), "{pets}");
    assert!(file(&files, "default.http").contains("GET {{baseUrl}}/health\n"));
    // `content: {}` means no body, not a panic.
    assert!(file(&files, "default.http").contains("### Ping\nPOST {{baseUrl}}/health\n"), "{}", file(&files, "default.http"));

    let swagger = r#"{"swagger": "2.0", "info": {"title": "Old"}, "host": "old.dev", "basePath": "/api", "schemes": ["http"],
      "paths": {"/items": {"post": {"tags": ["items"], "parameters": [{"in": "body", "name": "b", "schema": {"type": "object", "properties": {"n": {"type": "integer"}}}},
        {"in": "header", "name": "X-Req", "required": true}]}}}}"#;
    let c = import_str(swagger, "x").unwrap();
    assert_eq!(c.envs["$shared"][0].1, "http://old.dev/api");
    let files = assert_valid(&c);
    assert!(file(&files, "items.http").contains("@X-Req =\nPOST {{baseUrl}}/items\nContent-Type: application/json\nX-Req: {{X-Req}}\n\n{\n  \"n\": 0\n}"), "{}", file(&files, "items.http"));
}

#[test]
fn bruno_folder() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("bruno.json"), r#"{"version": "1", "name": "Todo", "type": "collection"}"#).unwrap();
    std::fs::create_dir_all(d.join("tasks")).unwrap();
    std::fs::create_dir_all(d.join("environments")).unwrap();
    std::fs::write(
        d.join("tasks/create.bru"),
        "meta {\n  name: Create task\n  type: http\n  seq: 2\n}\n\npost {\n  url: {{host}}/tasks\n  body: json\n  auth: bearer\n}\n\nheaders {\n  X-A: 1\n  ~X-Off: 0\n}\n\nauth:bearer {\n  token: {{token}}\n}\n\nbody:json {\n  {\"title\": \"x\"}\n}\n\nscript:pre-request {\n  bru.setVar('a', 1)\n}\n",
    )
    .unwrap();
    std::fs::write(d.join("tasks/list.bru"), "meta {\n  name: List tasks\n  seq: 1\n}\n\nget {\n  url: {{host}}/tasks\n}\n\nparams:query {\n  page: 1\n}\n").unwrap();
    std::fs::write(d.join("environments/dev.bru"), "vars {\n  host: http://localhost:3000\n}\nvars:secret [\n  token\n]\n").unwrap();

    let c = import(d).unwrap();
    assert_eq!((c.name.as_str(), c.format), ("Todo", "Bruno"));
    assert_eq!(c.envs["dev"], [("host".to_string(), "http://localhost:3000".to_string())]);
    assert_eq!(c.private_envs["dev"], [("token".to_string(), String::new())]);
    let files = assert_valid(&c);
    let tasks = file(&files, "tasks.http");
    assert!(tasks.find("### List tasks").unwrap() < tasks.find("### Create task").unwrap(), "seq order: {tasks}");
    assert!(tasks.contains("GET {{host}}/tasks?page=1\n"));
    assert!(tasks.contains("POST {{host}}/tasks\nX-A: 1\nAuthorization: Bearer {{token}}\nContent-Type: application/json\n\n{\n  \"title\": \"x\"\n}"), "{tasks}");
    assert!(!tasks.contains("X-Off") && tasks.contains("# Bruno scripts / tests were not converted"));
}

#[test]
fn har_and_write() {
    let har = r#"{"log": {"entries": [{"request": {"method": "POST", "url": "https://x.dev/api/items?a=1",
      "headers": [{"name": ":authority", "value": "x.dev"}, {"name": "accept", "value": "*/*"}, {"name": "sec-fetch-mode", "value": "cors"}],
      "postData": {"mimeType": "application/json", "text": "{\"q\":1}"}}, "response": {"status": 201}}]}}"#;
    let c = import_str(har, "x").unwrap();
    let files = assert_valid(&c);
    assert_eq!(files[0].1, "### POST /api/items\n# recorded response: 201\nPOST https://x.dev/api/items?a=1\naccept: */*\nContent-Type: application/json\n\n{\n  \"q\": 1\n}\n");

    // write(): new folder, never overwrites; env-only imports go next to existing env files.
    let dest = tempfile::tempdir().unwrap();
    let first = write(&c, dest.path()).unwrap();
    let second = write(&c, dest.path()).unwrap();
    assert!(first[0].starts_with(dest.path().join("har-import")) && second[0].starts_with(dest.path().join("har-import-2")));
    let env = import_str(
        r#"{"name": "dev", "_postman_variable_scope": "environment", "values": [{"key": "a", "value": "1"}, {"key": "t", "value": "s", "type": "secret"}]}"#,
        "x",
    )
    .unwrap();
    std::fs::write(dest.path().join("respite-dev.json"), r#"{"keep": "me"}"#).unwrap();
    let written = write(&env, dest.path()).unwrap();
    let read = |name: &str| serde_json::from_str::<Value>(&std::fs::read_to_string(dest.path().join(name)).unwrap()).unwrap();
    assert_eq!(written, [dest.path().join("respite-dev-2.json"), dest.path().join("respite-dev-2.private.json")]);
    assert_eq!(read("respite-dev.json"), serde_json::json!({"keep": "me"}));
    assert_eq!((read("respite-dev-2.json"), read("respite-dev-2.private.json")), (serde_json::json!({"a": "1"}), serde_json::json!({"t": "s"})));

    assert!(import_str("{\"hello\": 1}", "x").is_err());
}

#[test]
fn env_named_like_private_file() {
    assert_eq!(env_file("x.private", false), "respite-x-private.json");
    assert_eq!(env_file("x.private", true), "respite-x-private.private.json");
    assert_eq!(env_file("$shared", true), "respite.private.json");
}
