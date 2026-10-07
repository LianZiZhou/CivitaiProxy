//! End-to-end tests for the Hugging Face, GitHub and Docker Hub presets against mock upstreams
//! that mimic the real services' redirect / auth behaviour.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use civitai_proxy::config::{Config, Mode, SiteConfig};
use civitai_proxy::proxy::AppState;

const UPSTREAMS: &[&str] = &[
    "huggingface.co",
    "us.aws.cdn.hf.co",
    "cas-server.xethub.hf.co",
    "transfer.xethub.hf.co",
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "registry-1.docker.io",
    "auth.docker.io",
    "production.cloudflare.docker.com",
    "hub.docker.com",
];

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","annotations":{"src":"https://registry-1.docker.io/x"}}"#;
const CONFIG_JSON: &str = r#"{"homepage":"https://huggingface.co/gpt2"}"#;

fn auth(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn mock(req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    match (host.as_str(), path.as_str()) {
        // ---------------- Hugging Face ----------------
        ("huggingface.co", "/gpt2/resolve/main/model.safetensors") => (
            StatusCode::FOUND,
            [
                ("location", "https://us.aws.cdn.hf.co/xet-bridge-us/abc?X-Amz-Signature=s1".to_string()),
                ("x-linked-size", "10".to_string()),
                (
                    "link",
                    r#"<https://huggingface.co/api/models/gpt2/xet-read-token/sha>; rel="xet-auth", <https://cas-server.xethub.hf.co/v1/reconstructions/h1>; rel="xet-reconstruction-info""#.to_string(),
                ),
            ],
        )
            .into_response(),
        ("huggingface.co", "/gpt2/resolve/main/config.json") => {
            (StatusCode::TEMPORARY_REDIRECT, [("location", "/api/resolve-cache/models/gpt2/sha/config.json?etag=x")]).into_response()
        }
        ("huggingface.co", "/api/resolve-cache/models/gpt2/sha/config.json") => {
            ([("content-type", "application/json"), ("etag", "\"abc\"")], CONFIG_JSON).into_response()
        }
        ("huggingface.co", "/api/models/gpt2/xet-read-token/sha") => (
            [("content-type", "application/json"), ("x-xet-cas-url", "https://cas-server.xethub.hf.co")],
            r#"{"casUrl":"https://cas-server.xethub.hf.co","accessToken":"xet","exp":1}"#,
        )
            .into_response(),
        ("huggingface.co", "/api/models/gpt2") => {
            assert_eq!(auth(&req).as_deref(), Some("Bearer hf_x"));
            ([("content-type", "application/json")], r#"{"id":"gpt2","url":"https://huggingface.co/gpt2"}"#).into_response()
        }
        ("us.aws.cdn.hf.co", "/xet-bridge-us/abc") => {
            assert!(auth(&req).is_none(), "token leaked to CDN");
            assert_eq!(query, "X-Amz-Signature=s1");
            ([("content-type", "application/octet-stream")], "0123456789").into_response()
        }
        ("cas-server.xethub.hf.co", "/v1/reconstructions/h1") => {
            assert_eq!(auth(&req).as_deref(), Some("Bearer xet"));
            (
                [("content-type", "application/json")],
                r#"{"fetch_info":{"x":[{"url":"https://transfer.xethub.hf.co/xorb/x?Signature=s2&Key-Pair-Id=k"}]}}"#,
            )
                .into_response()
        }
        ("transfer.xethub.hf.co", "/xorb/x") => {
            assert!(auth(&req).is_none());
            "xorb".into_response()
        }
        // ---------------- GitHub ----------------
        ("github.com", "/cli/cli") => (
            [("content-type", "text/html; charset=utf-8")],
            r#"<script src="https://github.githubassets.com/a.js" integrity="sha512-x" crossorigin="anonymous"></script><a href="https://api.github.com/repos/cli/cli">api</a><meta name="expected-hostname" content="github.com">"#,
        )
            .into_response(),
        ("github.com", "/cli/cli/releases/download/v1/x.txt") => (
            StatusCode::FOUND,
            [("location", "https://release-assets.githubusercontent.com/github-production-release-asset/1/x?sp=r&sig=abc%2F")],
        )
            .into_response(),
        ("release-assets.githubusercontent.com", "/github-production-release-asset/1/x") => {
            assert!(auth(&req).is_none(), "token sent to presigned asset URL");
            assert!(req.headers().get(header::COOKIE).is_none());
            assert_eq!(query, "sp=r&sig=abc%2F");
            ([("content-type", "application/octet-stream")], "asset").into_response()
        }
        ("github.com", "/cli/cli/commit/abc.patch") => {
            ([("content-type", "text/plain")], "From https://github.com/cli/cli\n").into_response()
        }
        ("raw.githubusercontent.com", "/cli/cli/trunk/README.md") => {
            assert_eq!(auth(&req).as_deref(), Some("token ghp_x"));
            ([("content-type", "text/plain; charset=utf-8")], "see https://github.com/cli/cli").into_response()
        }
        ("api.github.com", "/repos/cli/cli") => {
            ([("content-type", "application/json")], r#"{"html_url":"https://github.com/cli/cli","url":"https://api.github.com/repos/cli/cli"}"#).into_response()
        }
        // ---------------- Docker ----------------
        ("registry-1.docker.io", "/v2/") => (
            StatusCode::UNAUTHORIZED,
            [
                ("www-authenticate", r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#),
                ("docker-distribution-api-version", "registry/2.0"),
            ],
        )
            .into_response(),
        ("auth.docker.io", "/token") => {
            assert!(query.contains("scope=repository%3Alibrary%2Falpine%3Apull") || query.contains("scope=repository:library/alpine:pull"), "{query}");
            ([("content-type", "application/json")], r#"{"token":"T"}"#).into_response()
        }
        ("registry-1.docker.io", "/v2/library/alpine/manifests/latest") => {
            assert_eq!(auth(&req).as_deref(), Some("Bearer T"));
            ([("content-type", "application/vnd.oci.image.manifest.v1+json"), ("docker-content-digest", "sha256:m")], MANIFEST).into_response()
        }
        ("registry-1.docker.io", "/v2/library/alpine/blobs/sha256:b") => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "https://production.cloudflare.docker.com/registry-v2/docker/registry/v2/blobs/sha256/b/data?verify=1-x")],
        )
            .into_response(),
        ("production.cloudflare.docker.com", "/registry-v2/docker/registry/v2/blobs/sha256/b/data") => {
            assert!(auth(&req).is_none());
            ([("content-type", "application/octet-stream")], "layer").into_response()
        }
        ("hub.docker.com", "/v2/repositories/library/alpine/") => {
            ([("content-type", "application/json")], r#"{"name":"alpine","url":"https://hub.docker.com/_/alpine"}"#).into_response()
        }
        _ => (StatusCode::NOT_FOUND, format!("mock: no route for {host}{path}")).into_response(),
    }
}

async fn spawn(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    addr
}

async fn setup() -> String {
    let upstream = spawn(Router::new().fallback(mock)).await;
    let resolve: BTreeMap<String, String> = UPSTREAMS
        .iter()
        .map(|h| (h.to_string(), format!("http://{upstream}")))
        .collect();
    let site = |p: &str, d: &str| SiteConfig {
        preset: Some(p.into()),
        mode: Mode::Single,
        public_domain: d.into(),
        ..Default::default()
    };
    let cfg = Config {
        sites: vec![
            site("huggingface", "hf.test"),
            site("github", "gh.test"),
            site("dockerhub", "docker.test"),
        ],
        upstream_resolve: resolve,
        ..Config::default()
    };
    let state = Arc::new(AppState::new(cfg).unwrap());
    format!("http://{}", spawn(civitai_proxy::app(state)).await)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn get(base: &str, host: &str, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut rb = client().get(format!("{base}{path}")).header("host", host);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    rb.send().await.unwrap()
}

fn hdr(r: &reqwest::Response, name: &str) -> String {
    r.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn huggingface() {
    let base = setup().await;
    let h = "hf.test";

    // LFS/xet file: redirect to CDN rewritten, xet Link header rewritten
    let r = get(
        &base,
        h,
        "/gpt2/resolve/main/model.safetensors",
        &[("authorization", "Bearer hf_x")],
    )
    .await;
    assert_eq!(r.status(), 302);
    assert_eq!(
        hdr(&r, "location"),
        "https://hf.test/__h/us.aws.cdn.hf.co/xet-bridge-us/abc?X-Amz-Signature=s1"
    );
    assert_eq!(hdr(&r, "x-linked-size"), "10");
    let link = hdr(&r, "link");
    assert!(
        link.contains("<https://hf.test/api/models/gpt2/xet-read-token/sha>"),
        "{link}"
    );
    assert!(
        link.contains("<https://hf.test/__h/cas-server.xethub.hf.co/v1/reconstructions/h1>"),
        "{link}"
    );

    // follow to the CDN with the token: the token must not reach the presigned URL
    let r = get(
        &base,
        h,
        "/__h/us.aws.cdn.hf.co/xet-bridge-us/abc?X-Amz-Signature=s1",
        &[("authorization", "Bearer hf_x")],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), "0123456789");

    // small file: relative redirect kept, content and ETag passed through untouched
    let r = get(&base, h, "/gpt2/resolve/main/config.json", &[]).await;
    assert_eq!(
        hdr(&r, "location"),
        "/api/resolve-cache/models/gpt2/sha/config.json?etag=x"
    );
    let r = get(
        &base,
        h,
        "/api/resolve-cache/models/gpt2/sha/config.json?etag=x",
        &[],
    )
    .await;
    assert_eq!(hdr(&r, "etag"), "\"abc\"");
    assert_eq!(r.text().await.unwrap(), CONFIG_JSON);

    // xet token: header and body rewritten
    let r = get(&base, h, "/api/models/gpt2/xet-read-token/sha", &[]).await;
    assert_eq!(
        hdr(&r, "x-xet-cas-url"),
        "https://hf.test/__h/cas-server.xethub.hf.co"
    );
    assert!(
        r.text()
            .await
            .unwrap()
            .contains(r#""casUrl":"https://hf.test/__h/cas-server.xethub.hf.co""#)
    );

    // xet reconstruction (needs the xet token), presigned transfer URL rewritten
    let r = get(
        &base,
        h,
        "/__h/cas-server.xethub.hf.co/v1/reconstructions/h1",
        &[("authorization", "Bearer xet")],
    )
    .await;
    let body = r.text().await.unwrap();
    assert!(
        body.contains(
            "https://hf.test/__h/transfer.xethub.hf.co/xorb/x?Signature=s2&Key-Pair-Id=k"
        ),
        "{body}"
    );
    let r = get(
        &base,
        h,
        "/__h/transfer.xethub.hf.co/xorb/x?Signature=s2&Key-Pair-Id=k",
        &[("authorization", "Bearer xet")],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), "xorb");

    // API JSON rewritten
    let r = get(
        &base,
        h,
        "/api/models/gpt2",
        &[("authorization", "Bearer hf_x")],
    )
    .await;
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"id":"gpt2","url":"https://hf.test/gpt2"}"#
    );
}

#[tokio::test]
async fn github() {
    let base = setup().await;
    let h = "gh.test";

    // HTML: URLs rewritten, SRI removed, expected-hostname mapped
    let r = get(&base, h, "/cli/cli", &[]).await;
    let body = r.text().await.unwrap();
    assert!(!body.contains("integrity="), "{body}");
    assert!(
        body.contains("https://gh.test/__h/api.github.com/repos/cli/cli"),
        "{body}"
    );
    assert!(body.contains(r#"content="gh.test""#), "{body}");

    // release download -> presigned asset
    let r = get(&base, h, "/cli/cli/releases/download/v1/x.txt", &[]).await;
    let loc = hdr(&r, "location");
    assert_eq!(
        loc,
        "https://gh.test/__h/release-assets.githubusercontent.com/github-production-release-asset/1/x?sp=r&sig=abc%2F"
    );
    let path = loc.strip_prefix("https://gh.test").unwrap();
    let r = get(
        &base,
        h,
        path,
        &[
            ("authorization", "token ghp_x"),
            ("cookie", "user_session=s"),
        ],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), "asset");

    // ghproxy-style full URL
    let r = get(
        &base,
        h,
        "/https://github.com/cli/cli/releases/download/v1/x.txt",
        &[],
    )
    .await;
    assert_eq!(hdr(&r, "location"), loc);

    // raw content is never rewritten; private-repo tokens reach raw.githubusercontent.com
    let r = get(
        &base,
        h,
        "/__h/raw.githubusercontent.com/cli/cli/trunk/README.md",
        &[("authorization", "token ghp_x")],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), "see https://github.com/cli/cli");
    let r = get(&base, h, "/cli/cli/commit/abc.patch", &[]).await;
    assert_eq!(r.text().await.unwrap(), "From https://github.com/cli/cli\n");

    // API JSON rewritten
    let r = get(&base, h, "/__h/api.github.com/repos/cli/cli", &[]).await;
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"html_url":"https://gh.test/cli/cli","url":"https://gh.test/__h/api.github.com/repos/cli/cli"}"#
    );
}

#[tokio::test]
async fn dockerhub() {
    let base = setup().await;
    let h = "docker.test";

    // registry ping: realm points back at the proxy, service name untouched
    let r = get(&base, h, "/v2/", &[]).await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        hdr(&r, "www-authenticate"),
        r#"Bearer realm="https://docker.test/__h/auth.docker.io/token",service="registry.docker.io""#
    );
    assert_eq!(hdr(&r, "docker-distribution-api-version"), "registry/2.0");

    // token
    let r = get(
        &base,
        h,
        "/__h/auth.docker.io/token?service=registry.docker.io&scope=repository:library/alpine:pull",
        &[],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), r#"{"token":"T"}"#);

    // `docker pull docker.test/alpine` -> library/alpine; manifest bytes untouched
    let r = get(
        &base,
        h,
        "/v2/alpine/manifests/latest",
        &[("authorization", "Bearer T")],
    )
    .await;
    assert_eq!(hdr(&r, "docker-content-digest"), "sha256:m");
    assert_eq!(r.text().await.unwrap(), MANIFEST);

    // blob redirect to the signed CDN URL, followed without the registry token
    let r = get(
        &base,
        h,
        "/v2/library/alpine/blobs/sha256:b",
        &[("authorization", "Bearer T")],
    )
    .await;
    let loc = hdr(&r, "location");
    assert_eq!(
        loc,
        "https://docker.test/__h/production.cloudflare.docker.com/registry-v2/docker/registry/v2/blobs/sha256/b/data?verify=1-x"
    );
    let r = get(
        &base,
        h,
        loc.strip_prefix("https://docker.test").unwrap(),
        &[("authorization", "Bearer T")],
    )
    .await;
    assert_eq!(r.text().await.unwrap(), "layer");

    // Hub web API shares /v2/ but goes to hub.docker.com and is rewritten
    let r = get(&base, h, "/v2/repositories/library/alpine/", &[]).await;
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"name":"alpine","url":"https://docker.test/_/alpine"}"#
    );
}
