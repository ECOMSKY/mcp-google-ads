//! Merchant API write steps against a stubbed server: the supplemental data
//! source created in step 1 is referenced by later steps, a patch on a product
//! input that does not exist yet falls back to an insert, and routes outside
//! the whitelist never reach the wire.

use mcp_google_ads::config::Config;
use mcp_google_ads::merchant::{execute_steps, MerchantClient, MerchantStep, StepMethod};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn step(
    method: StepMethod,
    path: &str,
    query: &[(&str, &str)],
    body: serde_json::Value,
) -> MerchantStep {
    MerchantStep {
        description: path.to_string(),
        method,
        path: path.to_string(),
        query: query
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        body: Some(body),
        capture_name_as: None,
        fallback_on_not_found: None,
    }
}

fn client(server: &MockServer) -> MerchantClient {
    let mut config = Config::default();
    config.google.credentials_path = "/nonexistent/credentials.json".into();
    MerchantClient::with_base_url(&config, server.uri()).unwrap()
}

#[tokio::test]
async fn captured_name_feeds_later_steps_and_patch_falls_back_to_insert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/datasources/v1/accounts/42/dataSources"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"name": "accounts/42/dataSources/777"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/products/v1/accounts/42/productInputs/fr~FR~sku1"))
        .and(query_param("dataSource", "accounts/42/dataSources/777"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "not found"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/products/v1/accounts/42/productInputs:insert"))
        .and(query_param("dataSource", "accounts/42/dataSources/777"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"name": "accounts/42/productInputs/fr~FR~sku1"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut create = step(
        StepMethod::Post,
        "datasources/v1/accounts/42/dataSources",
        &[],
        json!({"displayName": "x", "supplementalProductDataSource": {}}),
    );
    create.capture_name_as = Some("supp".into());
    let insert = step(
        StepMethod::Post,
        "products/v1/accounts/42/productInputs:insert",
        &[("dataSource", "{{supp}}")],
        json!({"offerId": "sku1", "productAttributes": {"customLabel2": "promo"}}),
    );
    let mut patch = step(
        StepMethod::Patch,
        "products/v1/accounts/42/productInputs/fr~FR~sku1",
        &[
            ("updateMask", "productAttributes.customLabel2"),
            ("dataSource", "{{supp}}"),
        ],
        json!({"productAttributes": {"customLabel2": "promo"}}),
    );
    patch.fallback_on_not_found = Some(Box::new(insert));

    let out = execute_steps(&client(&server), &[create, patch])
        .await
        .unwrap();
    assert_eq!(out["status"], "APPLIED");
    assert_eq!(out["results"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn non_whitelisted_route_is_refused_before_http() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    // Widening a primary data source update beyond its default rule.
    let wide = step(
        StepMethod::Patch,
        "datasources/v1/accounts/42/dataSources/1",
        &[("updateMask", "primaryProductDataSource")],
        json!({}),
    );
    let err = execute_steps(&client(&server), &[wide]).await.unwrap_err();
    assert!(err.to_string().contains("refused"), "{}", err);

    let delete = step(
        StepMethod::Post,
        "products/v1/accounts/42/productInputs/fr~FR~sku1:delete",
        &[],
        json!({}),
    );
    assert!(execute_steps(&client(&server), &[delete]).await.is_err());
}
