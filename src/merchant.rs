//! Merchant Center client for the Merchant API v1 (`merchantapi.googleapis.com`).
//!
//! Reuses the OAuth credentials of the Google Ads client, with the
//! `https://www.googleapis.com/auth/content` scope. No developer token is
//! needed, but the GCP project behind the OAuth client must be registered once
//! per Merchant Center account (`gmc_register_gcp`).
//!
//! Writes never go out from here directly: draft tools build a list of
//! [`MerchantStep`]s stored in a `ChangePlan`, and `confirm_and_apply` runs
//! them through [`execute_steps`]. Every step is checked against
//! [`validate_step`], a whitelist that has no delete route at all.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};

/// Default Merchant API base URL.
pub const DEFAULT_MERCHANT_BASE_URL: &str = "https://merchantapi.googleapis.com";

/// Environment variable overriding the Merchant API base URL (tests / proxies).
pub const MERCHANT_BASE_URL_ENV: &str = "MERCHANT_API_BASE_URL";

/// Upper bound on product pages scanned by list tools (1000 products/page).
pub const MAX_PRODUCT_PAGES: usize = 20;

/// HTTP verb of a write step. There is deliberately no `Delete` variant:
/// product and data source deletion is out of scope for this server.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum StepMethod {
    Post,
    Patch,
}

/// One HTTP request of a Merchant write plan.
///
/// `path`, `query` values and string leaves of `body` may contain
/// `{{var}}` placeholders, filled from the `name` field of an earlier step's
/// response (see `capture_name_as`). This is how "create the supplemental data
/// source, then write into it" works with a server-assigned id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MerchantStep {
    /// Human-readable summary shown in the preview.
    pub description: String,
    pub method: StepMethod,
    /// Path below the base URL, e.g. `products/v1/accounts/123/productInputs:insert`.
    pub path: String,
    #[serde(default)]
    pub query: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<Value>,
    /// Store the response's `name` under this variable for later steps.
    #[serde(default)]
    pub capture_name_as: Option<String>,
    /// Step to run instead when this one answers 404 (e.g. patch → insert).
    #[serde(default)]
    pub fallback_on_not_found: Option<Box<MerchantStep>>,
}

/// Merchant API REST client.
pub struct MerchantClient {
    http: reqwest::Client,
    config: Config,
    base_url: String,
}

impl MerchantClient {
    pub fn new(config: &Config) -> Result<Self> {
        let base = std::env::var(MERCHANT_BASE_URL_ENV)
            .unwrap_or_else(|_| DEFAULT_MERCHANT_BASE_URL.to_string());
        Self::with_base_url(config, base)
    }

    pub fn with_base_url(config: &Config, base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder().build().map_err(|e| {
            McpGoogleAdsError::Config(format!("Failed to build HTTP client: {}", e))
        })?;
        Ok(Self {
            http,
            config: config.clone(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }

    async fn token(&self) -> Result<String> {
        let is_test = !self.base_url.starts_with(DEFAULT_MERCHANT_BASE_URL);
        if is_test && !self.config.google.credentials_path.exists() {
            return Ok("test-access-token".to_string());
        }
        auth::get_access_token_for_scopes(&self.config, &[auth::MERCHANT_SCOPE]).await
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let mut req = self
            .http
            .request(method, &url)
            .bearer_auth(self.token().await?)
            .query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(parse_merchant_error(status, &text));
        }
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// GET a single resource.
    pub async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        self.send(reqwest::Method::GET, path, query, None).await
    }

    /// GET every page of a list endpoint, concatenating `items_field`.
    /// Stops after `max_pages` pages; the bool says whether more remained.
    pub async fn list_all(
        &self,
        path: &str,
        items_field: &str,
        page_size: u32,
        max_pages: usize,
    ) -> Result<(Vec<Value>, bool)> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..max_pages {
            let mut query = vec![("pageSize".to_string(), page_size.to_string())];
            if let Some(t) = &page_token {
                query.push(("pageToken".to_string(), t.clone()));
            }
            let page = self.get(path, &query).await?;
            if let Some(arr) = page.get(items_field).and_then(|v| v.as_array()) {
                items.extend(arr.iter().cloned());
            }
            page_token = page
                .get("nextPageToken")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            if page_token.is_none() {
                return Ok((items, false));
            }
        }
        Ok((items, true))
    }

    async fn run_step(&self, step: &MerchantStep, vars: &HashMap<String, String>) -> Result<Value> {
        let path = substitute(&step.path, vars)?;
        let query = step
            .query
            .iter()
            .map(|(k, v)| Ok((k.clone(), substitute(v, vars)?)))
            .collect::<Result<Vec<_>>>()?;
        let body = step
            .body
            .as_ref()
            .map(|b| substitute_json(b, vars))
            .transpose()?;
        validate_step(step.method, &path, &query)?;
        let method = match step.method {
            StepMethod::Post => reqwest::Method::POST,
            StepMethod::Patch => reqwest::Method::PATCH,
        };
        self.send(method, &path, &query, body.as_ref()).await
    }
}

/// Run a write plan's steps in order. Stops at the first failure and reports
/// which steps already succeeded (Merchant API has no multi-call transaction).
pub async fn execute_steps(client: &MerchantClient, steps: &[MerchantStep]) -> Result<Value> {
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut results = Vec::new();

    for (i, step) in steps.iter().enumerate() {
        let outcome = match client.run_step(step, &vars).await {
            Err(e) if is_not_found(&e) && step.fallback_on_not_found.is_some() => {
                let fb = step
                    .fallback_on_not_found
                    .as_deref()
                    .expect("checked above");
                client
                    .run_step(fb, &vars)
                    .await
                    .map(|r| (fb.description.clone(), r))
            }
            other => other.map(|r| (step.description.clone(), r)),
        };

        match outcome {
            Ok((description, response)) => {
                if let Some(var) = &step.capture_name_as {
                    let name = response
                        .get("name")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            McpGoogleAdsError::Validation(format!(
                                "Step {} ({}) returned no resource name to capture",
                                i + 1,
                                description
                            ))
                        })?;
                    vars.insert(var.clone(), name.to_string());
                }
                results.push(
                    json!({ "step": i + 1, "description": description, "response": response }),
                );
            }
            Err(e) => {
                let done: Vec<String> = results
                    .iter()
                    .filter_map(|r| r["description"].as_str().map(String::from))
                    .collect();
                let msg = if done.is_empty() {
                    format!("Step {} ({}) failed: {}", i + 1, step.description, e)
                } else {
                    format!(
                        "Step {} ({}) failed: {}. Already applied before the failure: {}",
                        i + 1,
                        step.description,
                        e,
                        done.join("; ")
                    )
                };
                return Err(match e {
                    McpGoogleAdsError::GoogleAds {
                        error_code,
                        details,
                        ..
                    } => McpGoogleAdsError::GoogleAds {
                        message: msg,
                        error_code,
                        details,
                    },
                    _ => McpGoogleAdsError::Validation(msg),
                });
            }
        }
    }

    Ok(json!({ "status": "APPLIED", "results": results }))
}

fn is_not_found(e: &McpGoogleAdsError) -> bool {
    matches!(e, McpGoogleAdsError::GoogleAds { error_code: Some(c), .. } if c == "NOT_FOUND")
}

/// Whitelist of write routes. Anything else is rejected before HTTP traffic.
///
/// - `POST accounts/v1/accounts/{id}/developerRegistration:registerGcp`
/// - `POST datasources/v1/accounts/{id}/dataSources` (create)
/// - `PATCH datasources/v1/accounts/{id}/dataSources/{ds}` — only with
///   `updateMask=primaryProductDataSource.defaultRule` (linking a supplemental source)
/// - `POST promotions/v1/accounts/{id}/promotions:insert`
/// - `POST products/v1/accounts/{id}/productInputs:insert` — only into a data
///   source (query `dataSource`)
/// - `PATCH products/v1/accounts/{id}/productInputs/{input}` — only with an
///   explicit `updateMask` restricted to custom labels
pub fn validate_step(method: StepMethod, path: &str, query: &[(String, String)]) -> Result<()> {
    let q = |k: &str| {
        query
            .iter()
            .find(|(qk, _)| qk == k)
            .map(|(_, v)| v.as_str())
    };
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let reject = |why: &str| {
        Err(McpGoogleAdsError::Safety(format!(
            "Merchant write refused ({} {}): {}",
            match method {
                StepMethod::Post => "POST",
                StepMethod::Patch => "PATCH",
            },
            path,
            why
        )))
    };

    if path.contains(":delete") || path.contains("..") {
        return reject("deletion routes are not allowed");
    }
    if segs.len() < 4 || segs[2] != "accounts" || !segs[3].chars().all(|c| c.is_ascii_digit()) {
        return reject("path must target accounts/{numeric_id}");
    }

    match (method, segs[0], segs[1], &segs[4..]) {
        (StepMethod::Post, "accounts", "v1", ["developerRegistration:registerGcp"]) => Ok(()),
        (StepMethod::Post, "datasources", "v1", ["dataSources"]) => Ok(()),
        (StepMethod::Patch, "datasources", "v1", ["dataSources", _]) => {
            if q("updateMask") == Some("primaryProductDataSource.defaultRule") {
                Ok(())
            } else {
                reject("data source updates are limited to primaryProductDataSource.defaultRule")
            }
        }
        (StepMethod::Post, "promotions", "v1", ["promotions:insert"]) => Ok(()),
        (StepMethod::Post, "products", "v1", ["productInputs:insert"]) => {
            if q("dataSource").is_some() {
                Ok(())
            } else {
                reject("productInputs:insert requires a dataSource")
            }
        }
        (StepMethod::Patch, "products", "v1", ["productInputs", _]) => {
            let mask_ok = q("updateMask").is_some_and(|m| {
                !m.is_empty()
                    && m.split(',').all(|f| {
                        f.strip_prefix("productAttributes.customLabel")
                            .is_some_and(|n| matches!(n, "0" | "1" | "2" | "3" | "4"))
                    })
            });
            if mask_ok && q("dataSource").is_some() {
                Ok(())
            } else {
                reject("productInputs patch is limited to custom labels with an explicit updateMask and dataSource")
            }
        }
        _ => reject("route is not in the Merchant write whitelist"),
    }
}

fn substitute(s: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    if out.contains("{{") {
        return Err(McpGoogleAdsError::Validation(format!(
            "Unresolved placeholder in '{}'",
            out
        )));
    }
    Ok(out)
}

fn substitute_json(v: &Value, vars: &HashMap<String, String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(substitute(s, vars)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| substitute_json(x, vars))
                .collect::<Result<_>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| Ok((k.clone(), substitute_json(x, vars)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

fn parse_merchant_error(status: reqwest::StatusCode, body: &str) -> McpGoogleAdsError {
    let json: Option<Value> = serde_json::from_str(body).ok();
    let err = json.as_ref().and_then(|j| j.get("error"));
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or(body)
        .to_string();
    let error_code = err
        .and_then(|e| e.get("status"))
        .and_then(|s| s.as_str())
        .map(String::from)
        .or_else(|| (status == reqwest::StatusCode::NOT_FOUND).then(|| "NOT_FOUND".to_string()));
    let details = err
        .and_then(|e| e.get("details"))
        .and_then(|d| d.as_array())
        .map(|a| a.iter().map(|d| d.to_string()).collect())
        .unwrap_or_default();
    let hint = if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        " — check that the token was granted the https://www.googleapis.com/auth/content scope \
         (re-run scripts/generate_token.sh) and that the GCP project is registered with gmc_register_gcp"
    } else {
        ""
    };
    McpGoogleAdsError::GoogleAds {
        message: format!("Merchant API [{}] {}{}", status, message, hint),
        error_code,
        details,
    }
}

/// Normalise a Merchant Center account id ("123-456" → "123456"), digits only.
pub fn normalize_merchant_id(id: &str) -> Result<String> {
    let digits: String = id
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "Invalid merchant_id '{}': expected the numeric Merchant Center account id",
            id
        )));
    }
    Ok(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn whitelist_accepts_known_routes() {
        assert!(validate_step(
            StepMethod::Post,
            "promotions/v1/accounts/1/promotions:insert",
            &[]
        )
        .is_ok());
        assert!(validate_step(
            StepMethod::Post,
            "datasources/v1/accounts/1/dataSources",
            &[]
        )
        .is_ok());
        assert!(validate_step(
            StepMethod::Patch,
            "products/v1/accounts/1/productInputs/fr~FR~sku",
            &q(&[
                ("updateMask", "productAttributes.customLabel3"),
                ("dataSource", "accounts/1/dataSources/2")
            ])
        )
        .is_ok());
    }

    #[test]
    fn whitelist_rejects_deletes_and_wide_patches() {
        assert!(validate_step(
            StepMethod::Post,
            "products/v1/accounts/1/productInputs/x:delete",
            &[]
        )
        .is_err());
        assert!(validate_step(
            StepMethod::Patch,
            "datasources/v1/accounts/1/dataSources/9",
            &q(&[("updateMask", "displayName")])
        )
        .is_err());
        assert!(validate_step(
            StepMethod::Patch,
            "products/v1/accounts/1/productInputs/fr~FR~sku",
            &q(&[
                ("updateMask", "productAttributes.price"),
                ("dataSource", "d")
            ])
        )
        .is_err());
        assert!(validate_step(
            StepMethod::Post,
            "products/v1/accounts/1/productInputs:insert",
            &[]
        )
        .is_err());
        assert!(validate_step(
            StepMethod::Post,
            "products/v1/accounts/abc/productInputs:insert",
            &q(&[("dataSource", "d")])
        )
        .is_err());
    }

    #[test]
    fn placeholders_are_substituted_or_rejected() {
        let mut vars = HashMap::new();
        vars.insert("ds".to_string(), "accounts/1/dataSources/7".to_string());
        assert_eq!(
            substitute("x={{ds}}", &vars).unwrap(),
            "x=accounts/1/dataSources/7"
        );
        assert!(substitute("{{other}}", &vars).is_err());
        let v = substitute_json(&json!({"a": ["{{ds}}"]}), &vars).unwrap();
        assert_eq!(v["a"][0], "accounts/1/dataSources/7");
    }

    #[test]
    fn merchant_id_normalisation() {
        assert_eq!(normalize_merchant_id("123-456 789").unwrap(), "123456789");
        assert!(normalize_merchant_id("acc_12").is_err());
    }
}
