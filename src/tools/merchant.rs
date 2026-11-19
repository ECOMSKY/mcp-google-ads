//! Merchant Center tools (Merchant API v1).
//!
//! Read tools call the API directly. Write tools (`draft_*`) only build a
//! [`ChangePlan`] with [`PlanDispatch::MerchantRequests`]; nothing is sent until
//! `confirm_and_apply`, which also writes the audit log entry.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::merchant::{
    normalize_merchant_id, MerchantClient, MerchantStep, StepMethod, MAX_PRODUCT_PAGES,
};
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan, PlanDispatch};

/// Display name of the supplemental data source this server writes custom
/// labels into. Reused when it already exists.
pub const CUSTOM_LABEL_DATA_SOURCE_NAME: &str = "MCP google-ads-write — custom labels";

/// Display name prefix of promotion data sources auto-created by this server.
pub const PROMOTION_DATA_SOURCE_PREFIX: &str = "MCP google-ads-write — promotions";

fn validation(msg: impl Into<String>) -> McpGoogleAdsError {
    McpGoogleAdsError::Validation(msg.into())
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(|x| x.as_str()).unwrap_or("")
}

// ── Product status ───────────────────────────────────────────────────────

/// Aggregate review status of a product across destinations and countries.
///
/// - `disapproved`: refused somewhere and approved nowhere
/// - `limited`: refused in some countries/destinations but approved in
///   others, or carrying a `DEMOTED` issue
/// - `pending`: only pending countries
/// - `approved`: everything else
pub fn product_status(product: &Value) -> &'static str {
    let status = product.get("productStatus");
    let dests = status
        .and_then(|s| s.get("destinationStatuses"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let non_empty = |d: &Value, k: &str| {
        d.get(k)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    };
    let any_approved = dests.iter().any(|d| non_empty(d, "approvedCountries"));
    let any_disapproved = dests.iter().any(|d| non_empty(d, "disapprovedCountries"));
    let any_pending = dests.iter().any(|d| non_empty(d, "pendingCountries"));
    let demoted = status
        .and_then(|s| s.get("itemLevelIssues"))
        .and_then(|i| i.as_array())
        .is_some_and(|a| a.iter().any(|i| s(i, "/severity") == "DEMOTED"));

    if any_disapproved && !any_approved {
        "disapproved"
    } else if any_disapproved || demoted {
        "limited"
    } else if any_pending && !any_approved {
        "pending"
    } else {
        "approved"
    }
}

/// "in stock", "IN_STOCK", "in_stock" → "in_stock".
fn normalize_availability(a: &str) -> String {
    a.trim().to_lowercase().replace([' ', '-'], "_")
}

fn compact_issues(product: &Value) -> Vec<Value> {
    product
        .pointer("/productStatus/itemLevelIssues")
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .map(|i| {
                    json!({
                        "code": i.get("code"),
                        "severity": i.get("severity"),
                        "description": i.get("description"),
                        "detail": i.get("detail"),
                        "attribute": i.get("attribute"),
                        "resolution": i.get("resolution"),
                        "reporting_context": i.get("reportingContext"),
                        "countries": i.get("applicableCountries"),
                        "documentation": i.get("documentation"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compact_product(p: &Value) -> Value {
    let attrs = p.get("productAttributes").cloned().unwrap_or(json!({}));
    let price = attrs.get("price").map(|pr| {
        let micros: f64 = pr
            .get("amountMicros")
            .and_then(|m| {
                m.as_str()
                    .and_then(|x| x.parse().ok())
                    .or_else(|| m.as_f64())
            })
            .unwrap_or(0.0);
        json!({ "amount": micros / 1_000_000.0, "currency": pr.get("currencyCode") })
    });
    let labels: Vec<Value> = (0..5)
        .map(|i| {
            attrs
                .get(format!("customLabel{}", i))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    json!({
        "offer_id": p.get("offerId"),
        "title": attrs.get("title"),
        "content_language": p.get("contentLanguage"),
        "feed_label": p.get("feedLabel"),
        "availability": attrs.get("availability"),
        "price": price,
        "custom_labels": labels,
        "status": product_status(p),
        "destination_statuses": p.pointer("/productStatus/destinationStatuses"),
        "issues": compact_issues(p),
        "name": p.get("name"),
    })
}

async fn fetch_products(client: &MerchantClient, merchant_id: &str) -> Result<(Vec<Value>, bool)> {
    client
        .list_all(
            &format!("products/v1/accounts/{}/products", merchant_id),
            "products",
            1000,
            MAX_PRODUCT_PAGES,
        )
        .await
}

/// `gmc_list_products`: products with status and issues, optionally filtered.
pub async fn list_products(
    config: &Config,
    merchant_id: &str,
    status: Option<&str>,
    availability: Option<&str>,
    limit: Option<u32>,
) -> Result<Value> {
    let mid = normalize_merchant_id(merchant_id)?;
    let status_filter = status
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty() && s != "any");
    if let Some(sf) = &status_filter {
        if !["approved", "disapproved", "limited", "pending"].contains(&sf.as_str()) {
            return Err(validation(format!(
                "Invalid status '{}': expected approved, disapproved, limited, pending or any",
                sf
            )));
        }
    }
    let avail_filter = availability
        .map(normalize_availability)
        .filter(|a| !a.is_empty());
    let limit = limit.unwrap_or(100).clamp(1, 1000) as usize;

    let client = MerchantClient::new(config)?;
    let (products, truncated) = fetch_products(&client, &mid).await?;
    let scanned = products.len();

    let matching: Vec<Value> = products
        .iter()
        .filter(|p| {
            status_filter
                .as_deref()
                .is_none_or(|sf| product_status(p) == sf)
        })
        .filter(|p| {
            avail_filter.as_deref().is_none_or(|af| {
                normalize_availability(s(p, "/productAttributes/availability")) == af
            })
        })
        .map(compact_product)
        .collect();

    let mut counts = serde_json::Map::new();
    for p in &products {
        let k = product_status(p);
        let n = counts.get(k).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        counts.insert(k.to_string(), json!(n));
    }

    Ok(json!({
        "merchant_id": mid,
        "products_scanned": scanned,
        "scan_truncated": truncated,
        "status_counts": counts,
        "matching": matching.len(),
        "returned": matching.len().min(limit),
        "products": matching.into_iter().take(limit).collect::<Vec<_>>(),
    }))
}

/// `gmc_product_issues`: disapproved or limited products, with the reasons.
pub async fn product_issues(
    config: &Config,
    merchant_id: &str,
    limit: Option<u32>,
) -> Result<Value> {
    let mid = normalize_merchant_id(merchant_id)?;
    let limit = limit.unwrap_or(200).clamp(1, 1000) as usize;
    let client = MerchantClient::new(config)?;
    let (products, truncated) = fetch_products(&client, &mid).await?;

    let mut by_code: std::collections::BTreeMap<String, (u64, String)> = Default::default();
    let flagged: Vec<Value> = products
        .iter()
        .filter(|p| matches!(product_status(p), "disapproved" | "limited"))
        .map(|p| {
            let blocking: Vec<Value> = compact_issues(p)
                .into_iter()
                .filter(|i| i["severity"] != "NOT_IMPACTED")
                .collect();
            for i in &blocking {
                let code = i["code"].as_str().unwrap_or("unknown").to_string();
                let desc = i["description"].as_str().unwrap_or("").to_string();
                let e = by_code.entry(code).or_insert((0, desc));
                e.0 += 1;
            }
            json!({
                "offer_id": p.get("offerId"),
                "title": p.pointer("/productAttributes/title"),
                "content_language": p.get("contentLanguage"),
                "feed_label": p.get("feedLabel"),
                "status": product_status(p),
                "issues": blocking,
            })
        })
        .collect();

    let mut summary: Vec<Value> = by_code
        .into_iter()
        .map(|(code, (n, desc))| json!({ "code": code, "products": n, "description": desc }))
        .collect();
    summary.sort_by_key(|v| std::cmp::Reverse(v["products"].as_u64().unwrap_or(0)));

    Ok(json!({
        "merchant_id": mid,
        "products_scanned": products.len(),
        "scan_truncated": truncated,
        "flagged_products": flagged.len(),
        "issues_by_code": summary,
        "products": flagged.into_iter().take(limit).collect::<Vec<_>>(),
    }))
}

/// `gmc_list_promotions`.
pub async fn list_promotions(config: &Config, merchant_id: &str) -> Result<Value> {
    let mid = normalize_merchant_id(merchant_id)?;
    let client = MerchantClient::new(config)?;
    let (promos, truncated) = client
        .list_all(
            &format!("promotions/v1/accounts/{}/promotions", mid),
            "promotions",
            250,
            20,
        )
        .await?;
    let compact: Vec<Value> = promos
        .iter()
        .map(|p| {
            json!({
                "promotion_id": p.get("promotionId"),
                "title": p.pointer("/attributes/longTitle"),
                "target_country": p.get("targetCountry"),
                "content_language": p.get("contentLanguage"),
                "coupon_value_type": p.pointer("/attributes/couponValueType"),
                "percent_off": p.pointer("/attributes/percentOff"),
                "money_off": p.pointer("/attributes/moneyOffAmount"),
                "minimum_purchase": p.pointer("/attributes/minimumPurchaseAmount"),
                "code": p.pointer("/attributes/genericRedemptionCode"),
                "product_applicability": p.pointer("/attributes/productApplicability"),
                "effective_period": p.pointer("/attributes/promotionEffectiveTimePeriod"),
                "destination_statuses": p.pointer("/promotionStatus/destinationStatuses"),
                "issues": p.pointer("/promotionStatus/itemLevelIssues"),
                "data_source": p.get("dataSource"),
            })
        })
        .collect();
    Ok(
        json!({ "merchant_id": mid, "count": compact.len(), "truncated": truncated, "promotions": compact }),
    )
}

// ── Write tools ──────────────────────────────────────────────────────────

fn merchant_plan(
    operation: &str,
    entity_type: &str,
    entity_id: &str,
    merchant_id: &str,
    changes: Value,
    steps: Vec<MerchantStep>,
) -> Value {
    let mut changes = changes;
    changes["requests"] = json!(steps
        .iter()
        .map(|st| json!({
            "description": st.description,
            "method": st.method,
            "path": st.path,
            "query": st.query,
            "body": st.body,
            "fallback_on_not_found": st.fallback_on_not_found.as_ref().map(|f| json!({
                "description": f.description, "method": f.method, "path": f.path, "query": f.query, "body": f.body
            })),
        }))
        .collect::<Vec<_>>());
    let plan = ChangePlan::new(
        operation.to_string(),
        entity_type.to_string(),
        entity_id.to_string(),
        merchant_id.to_string(),
        changes,
        false,
        Vec::new(),
    )
    .with_dispatch(PlanDispatch::MerchantRequests { steps });
    let preview = plan.to_preview();
    store_plan(plan);
    preview
}

/// `gmc_register_gcp`: register the OAuth client's GCP project with a
/// Merchant Center account (once per account).
pub fn draft_register_gcp(
    config: &Config,
    merchant_id: &str,
    developer_email: &str,
) -> Result<Value> {
    check_blocked_operation("gmc_register_gcp", &config.safety)?;
    let mid = normalize_merchant_id(merchant_id)?;
    let email = developer_email.trim();
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(validation(format!("Invalid developer_email '{}'", email)));
    }
    let step = MerchantStep {
        description: format!(
            "Register this GCP project with Merchant Center {} (developer {})",
            mid, email
        ),
        method: StepMethod::Post,
        path: format!(
            "accounts/v1/accounts/{}/developerRegistration:registerGcp",
            mid
        ),
        query: vec![],
        body: Some(json!({ "developerEmail": email })),
        capture_name_as: None,
        fallback_on_not_found: None,
    };
    Ok(merchant_plan(
        "gmc_register_gcp",
        "gmc_developer_registration",
        &mid,
        &mid,
        json!({ "developer_email": email }),
        vec![step],
    ))
}

/// Inputs of `gmc_create_promotion`.
#[derive(Debug, Clone, Default)]
pub struct PromotionInput {
    pub merchant_id: String,
    pub promotion_id: String,
    pub title: String,
    pub percent_off: Option<u32>,
    pub amount_off: Option<f64>,
    pub currency: Option<String>,
    pub promo_code: Option<String>,
    pub minimum_order_amount: Option<f64>,
    pub start: String,
    pub end: String,
    pub target_country: String,
    pub content_language: String,
    pub offer_ids: Option<Vec<String>>,
    pub destinations: Option<Vec<String>>,
}

/// Parse `YYYY-MM-DD` (start of day / end of day, UTC) or RFC 3339.
fn parse_promo_time(raw: &str, end_of_day: bool) -> Result<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Utc));
    }
    let d = NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| {
        validation(format!(
            "Invalid date '{}': use YYYY-MM-DD or RFC 3339",
            raw
        ))
    })?;
    let t = if end_of_day {
        d.and_hms_opt(23, 59, 59)
    } else {
        d.and_hms_opt(0, 0, 0)
    };
    Ok(t.expect("valid time").and_utc())
}

fn micros(amount: f64) -> String {
    ((amount * 1_000_000.0).round() as i64).to_string()
}

/// Build the Merchant API `Promotion` payload (validated).
pub fn build_promotion(input: &PromotionInput) -> Result<Value> {
    let pid = input.promotion_id.trim();
    if pid.is_empty()
        || pid.len() > 50
        || !pid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(validation(
            "promotion_id: 1-50 characters, letters, digits, '_' or '-'",
        ));
    }
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 60 {
        return Err(validation("title: 1-60 characters"));
    }
    let country = input.target_country.trim().to_uppercase();
    if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(validation(
            "target_country: two-letter ISO 3166-1 code, e.g. FR",
        ));
    }
    let lang = input.content_language.trim().to_lowercase();
    if lang.len() != 2 || !lang.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(validation(
            "content_language: two-letter ISO 639-1 code, e.g. fr",
        ));
    }

    let currency = input.currency.as_deref().map(|c| c.trim().to_uppercase());
    let need_currency = input.amount_off.is_some() || input.minimum_order_amount.is_some();
    let currency = match (need_currency, currency) {
        (true, Some(c)) if c.len() == 3 => Some(c),
        (true, _) => {
            return Err(validation(
                "currency (e.g. EUR) is required with amount_off or minimum_order_amount",
            ))
        }
        (false, c) => c,
    };

    let mut attrs = serde_json::Map::new();
    attrs.insert("longTitle".into(), json!(title));
    match (input.percent_off, input.amount_off) {
        (Some(p), None) => {
            if !(1..=99).contains(&p) {
                return Err(validation("percent_off must be between 1 and 99"));
            }
            attrs.insert("couponValueType".into(), json!("PERCENT_OFF"));
            attrs.insert("percentOff".into(), json!(p.to_string()));
        }
        (None, Some(a)) => {
            if a.is_nan() || a <= 0.0 {
                return Err(validation("amount_off must be > 0"));
            }
            attrs.insert("couponValueType".into(), json!("MONEY_OFF"));
            attrs.insert(
                "moneyOffAmount".into(),
                json!({ "amountMicros": micros(a), "currencyCode": currency }),
            );
        }
        _ => return Err(validation("Give exactly one of percent_off or amount_off")),
    }

    if let Some(min) = input.minimum_order_amount {
        if min.is_nan() || min <= 0.0 {
            return Err(validation("minimum_order_amount must be > 0"));
        }
        if input.amount_off.is_some_and(|a| a >= min) {
            return Err(validation(
                "amount_off must be lower than minimum_order_amount",
            ));
        }
        attrs.insert(
            "minimumPurchaseAmount".into(),
            json!({ "amountMicros": micros(min), "currencyCode": currency }),
        );
    }

    match input
        .promo_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        Some(code) => {
            if code.chars().count() > 20 {
                return Err(validation("promo_code: at most 20 characters"));
            }
            attrs.insert("offerType".into(), json!("GENERIC_CODE"));
            attrs.insert("genericRedemptionCode".into(), json!(code));
        }
        None => {
            attrs.insert("offerType".into(), json!("NO_CODE"));
        }
    }

    let start = parse_promo_time(&input.start, false)?;
    let end = parse_promo_time(&input.end, true)?;
    if end <= start {
        return Err(validation("end must be after start"));
    }
    if (end - start).num_days() > 183 {
        return Err(validation("A promotion can run at most 183 days"));
    }
    if end <= Utc::now() {
        return Err(validation("end is in the past"));
    }
    let fmt = |d: DateTime<Utc>| d.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    attrs.insert(
        "promotionEffectiveTimePeriod".into(),
        json!({ "startTime": fmt(start), "endTime": fmt(end) }),
    );

    let offer_ids: Vec<String> = input
        .offer_ids
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    if offer_ids.is_empty() {
        attrs.insert("productApplicability".into(), json!("ALL_PRODUCTS"));
    } else {
        attrs.insert("productApplicability".into(), json!("SPECIFIC_PRODUCTS"));
        attrs.insert("itemIdInclusion".into(), json!(offer_ids));
    }

    let destinations = input
        .destinations
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| vec!["SHOPPING_ADS".into(), "FREE_LISTINGS".into()]);
    attrs.insert(
        "promotionDestinations".into(),
        json!(destinations
            .iter()
            .map(|d| d.trim().to_uppercase())
            .collect::<Vec<_>>()),
    );

    Ok(json!({
        "promotionId": pid,
        "contentLanguage": lang,
        "targetCountry": country,
        "redemptionChannel": ["ONLINE"],
        "attributes": attrs,
    }))
}

/// `gmc_create_promotion`: insert a promotion into a promotion data source
/// matching (country, language), creating that data source if needed.
pub async fn draft_create_promotion(config: &Config, input: &PromotionInput) -> Result<Value> {
    check_blocked_operation("gmc_create_promotion", &config.safety)?;
    let mid = normalize_merchant_id(&input.merchant_id)?;
    let promotion = build_promotion(input)?;
    let country = promotion["targetCountry"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let lang = promotion["contentLanguage"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let client = MerchantClient::new(config)?;
    let (sources, _) = client
        .list_all(
            &format!("datasources/v1/accounts/{}/dataSources", mid),
            "dataSources",
            1000,
            10,
        )
        .await?;
    let existing = sources.iter().find(|d| {
        s(d, "/promotionDataSource/targetCountry").eq_ignore_ascii_case(&country)
            && s(d, "/promotionDataSource/contentLanguage").eq_ignore_ascii_case(&lang)
            && s(d, "/input") == "API"
    });

    let mut steps = Vec::new();
    let data_source = match existing {
        Some(d) => s(d, "/name").to_string(),
        None => {
            steps.push(MerchantStep {
                description: format!("Create promotion data source for {}/{}", country, lang),
                method: StepMethod::Post,
                path: format!("datasources/v1/accounts/{}/dataSources", mid),
                query: vec![],
                body: Some(json!({
                    "displayName": format!("{} {}-{}", PROMOTION_DATA_SOURCE_PREFIX, country, lang),
                    "promotionDataSource": { "targetCountry": country, "contentLanguage": lang },
                })),
                capture_name_as: Some("promotion_ds".into()),
                fallback_on_not_found: None,
            });
            "{{promotion_ds}}".to_string()
        }
    };
    steps.push(MerchantStep {
        description: format!("Insert promotion '{}'", input.promotion_id.trim()),
        method: StepMethod::Post,
        path: format!("promotions/v1/accounts/{}/promotions:insert", mid),
        query: vec![],
        body: Some(json!({ "promotion": promotion, "dataSource": data_source })),
        capture_name_as: None,
        fallback_on_not_found: None,
    });

    Ok(merchant_plan(
        "gmc_create_promotion",
        "gmc_promotion",
        input.promotion_id.trim(),
        &mid,
        json!({
            "promotion": promotion,
            "data_source": if existing.is_some() { json!(data_source) } else { json!("(created by this plan)") },
            "note": "Google reviews new promotions (up to ~24h) before they show.",
        }),
        steps,
    ))
}

/// Inputs of `gmc_set_custom_label`.
#[derive(Debug, Clone, Default)]
pub struct CustomLabelInput {
    pub merchant_id: String,
    pub offer_id: String,
    pub label_index: u8,
    /// Empty string clears the label in the supplemental source.
    pub value: String,
    pub content_language: Option<String>,
    pub feed_label: Option<String>,
}

/// Reference to `supplemental` in a primary's default rule?
fn rule_has_source(primary: &Value, supplemental: &str) -> bool {
    primary
        .pointer("/primaryProductDataSource/defaultRule/takeFromDataSources")
        .and_then(|v| v.as_array())
        .is_some_and(|a| {
            a.iter()
                .any(|r| s(r, "/supplementalDataSourceName") == supplemental)
        })
}

/// New `takeFromDataSources` with the supplemental source FIRST (first wins),
/// keeping every existing entry in order. Unset rule = `[self]`.
pub fn default_rule_with_supplemental(primary: &Value, supplemental: &str) -> Value {
    let mut list = vec![json!({ "supplementalDataSourceName": supplemental })];
    match primary
        .pointer("/primaryProductDataSource/defaultRule/takeFromDataSources")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
    {
        Some(existing) => list.extend(existing.iter().cloned()),
        None => list.push(json!({ "self": true })),
    }
    json!({ "primaryProductDataSource": { "defaultRule": { "takeFromDataSources": list } } })
}

fn encode_segment(seg: &str) -> String {
    seg.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// `gmc_set_custom_label`: set custom_label_N on one product through a
/// supplemental API data source (created and linked when missing).
pub async fn draft_set_custom_label(config: &Config, input: &CustomLabelInput) -> Result<Value> {
    check_blocked_operation("gmc_set_custom_label", &config.safety)?;
    let mid = normalize_merchant_id(&input.merchant_id)?;
    if input.label_index > 4 {
        return Err(validation("label_index must be 0-4"));
    }
    let value = input.value.trim().to_string();
    if value.chars().count() > 100 {
        return Err(validation("Custom label value: at most 100 characters"));
    }
    let offer_id = input.offer_id.trim();
    if offer_id.is_empty() {
        return Err(validation("offer_id is required"));
    }

    let client = MerchantClient::new(config)?;

    // 1. Find the product (one offer id can exist per language / feed label).
    let (products, _) = fetch_products(&client, &mid).await?;
    let lang_f = input
        .content_language
        .as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty());
    let label_f = input
        .feed_label
        .as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty());
    let matches: Vec<&Value> = products
        .iter()
        .filter(|p| s(p, "/offerId") == offer_id)
        .filter(|p| lang_f.is_none_or(|l| s(p, "/contentLanguage").eq_ignore_ascii_case(l)))
        .filter(|p| label_f.is_none_or(|l| s(p, "/feedLabel").eq_ignore_ascii_case(l)))
        .collect();
    let product = match matches.as_slice() {
        [one] => *one,
        [] => {
            return Err(validation(format!(
                "No product with offer_id '{}' in Merchant Center {}",
                offer_id, mid
            )))
        }
        many => {
            let variants: Vec<String> = many
                .iter()
                .map(|p| format!("{}/{}", s(p, "/contentLanguage"), s(p, "/feedLabel")))
                .collect();
            return Err(validation(format!(
                "offer_id '{}' exists for several language/feed label pairs ({}): pass content_language and feed_label",
                offer_id,
                variants.join(", ")
            )));
        }
    };
    let lang = s(product, "/contentLanguage").to_string();
    let feed_label = s(product, "/feedLabel").to_string();
    let primary_name = s(product, "/dataSource").to_string();
    let field = format!("customLabel{}", input.label_index);
    let current = product
        .pointer(&format!("/productAttributes/{}", field))
        .cloned();

    // 2. Find our supplemental source, and the product's primary source.
    let (sources, _) = client
        .list_all(
            &format!("datasources/v1/accounts/{}/dataSources", mid),
            "dataSources",
            1000,
            10,
        )
        .await?;
    let supplemental = sources.iter().find(|d| {
        s(d, "/displayName") == CUSTOM_LABEL_DATA_SOURCE_NAME
            && d.get("supplementalProductDataSource").is_some()
            && s(d, "/input") == "API"
    });
    let primary = sources
        .iter()
        .find(|d| !primary_name.is_empty() && s(d, "/name") == primary_name)
        .filter(|d| d.get("primaryProductDataSource").is_some());

    let mut steps = Vec::new();
    let supp_ref = match supplemental {
        Some(d) => s(d, "/name").to_string(),
        None => {
            steps.push(MerchantStep {
                description: format!(
                    "Create supplemental API data source '{}'",
                    CUSTOM_LABEL_DATA_SOURCE_NAME
                ),
                method: StepMethod::Post,
                path: format!("datasources/v1/accounts/{}/dataSources", mid),
                query: vec![],
                body: Some(json!({
                    "displayName": CUSTOM_LABEL_DATA_SOURCE_NAME,
                    "supplementalProductDataSource": {},
                })),
                capture_name_as: Some("supplemental".into()),
                fallback_on_not_found: None,
            });
            "{{supplemental}}".to_string()
        }
    };

    // 3. Link it into the primary's default rule (first = highest priority).
    let mut rule_change = Value::Null;
    match primary {
        Some(p) if supplemental.is_none() || !rule_has_source(p, &supp_ref) => {
            let body = default_rule_with_supplemental(p, &supp_ref);
            rule_change = json!({
                "primary_data_source": primary_name,
                "before": p.pointer("/primaryProductDataSource/defaultRule"),
                "after": body.pointer("/primaryProductDataSource/defaultRule"),
            });
            steps.push(MerchantStep {
                description: format!(
                    "Link the supplemental source into the default rule of primary source {} (supplemental first, existing sources kept)",
                    primary_name
                ),
                method: StepMethod::Patch,
                path: format!("datasources/v1/{}", primary_name),
                query: vec![("updateMask".into(), "primaryProductDataSource.defaultRule".into())],
                body: Some(body),
                capture_name_as: None,
                fallback_on_not_found: None,
            });
        }
        Some(_) => {}
        None => {
            return Err(validation(format!(
                "Could not find the primary data source '{}' of this product among the account's data sources; \
                 the supplemental source could not be linked",
                primary_name
            )))
        }
    }

    // 4. Write the label: patch (keeps the other labels already in the
    //    supplemental source); insert when the product has no input there yet.
    let product_attrs = if value.is_empty() {
        json!({})
    } else {
        json!({ field.clone(): value })
    };
    let insert = MerchantStep {
        description: format!(
            "Insert {} = '{}' for {} into the supplemental source",
            field, value, offer_id
        ),
        method: StepMethod::Post,
        path: format!("products/v1/accounts/{}/productInputs:insert", mid),
        query: vec![("dataSource".into(), supp_ref.clone())],
        body: Some(json!({
            "offerId": offer_id,
            "contentLanguage": lang,
            "feedLabel": feed_label,
            "productAttributes": product_attrs,
        })),
        capture_name_as: None,
        fallback_on_not_found: None,
    };
    let input_name = format!(
        "{}~{}~{}",
        encode_segment(&lang),
        encode_segment(&feed_label),
        encode_segment(offer_id)
    );
    let label_step = if supplemental.is_none() {
        insert
    } else {
        MerchantStep {
            description: format!(
                "Patch {} = '{}' for {} in the supplemental source",
                field, value, offer_id
            ),
            method: StepMethod::Patch,
            path: format!("products/v1/accounts/{}/productInputs/{}", mid, input_name),
            query: vec![
                ("updateMask".into(), format!("productAttributes.{}", field)),
                ("dataSource".into(), supp_ref.clone()),
            ],
            body: Some(json!({ "productAttributes": product_attrs })),
            capture_name_as: None,
            fallback_on_not_found: Some(Box::new(insert)),
        }
    };
    steps.push(label_step);

    Ok(merchant_plan(
        "gmc_set_custom_label",
        "gmc_product_custom_label",
        offer_id,
        &mid,
        json!({
            "offer_id": offer_id,
            "content_language": lang,
            "feed_label": feed_label,
            "field": format!("custom_label_{}", input.label_index),
            "current_value": current,
            "new_value": if value.is_empty() { Value::Null } else { json!(value) },
            "supplemental_data_source": supplemental.map(|d| json!(s(d, "/name"))).unwrap_or(json!("(created by this plan)")),
            "default_rule_change": rule_change,
            "note": "Merchant Center reprocesses the product within minutes to a few hours; no product or data source is deleted.",
        }),
        steps,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn promo() -> PromotionInput {
        PromotionInput {
            merchant_id: "123".into(),
            promotion_id: "BF-2099".into(),
            title: "-20% sur tout".into(),
            percent_off: Some(20),
            promo_code: Some("BF20".into()),
            minimum_order_amount: Some(50.0),
            currency: Some("eur".into()),
            start: "2099-11-20".into(),
            end: "2099-11-30".into(),
            target_country: "fr".into(),
            content_language: "FR".into(),
            ..Default::default()
        }
    }

    #[test]
    fn promotion_payload_percent_with_code_and_minimum() {
        let p = build_promotion(&promo()).unwrap();
        assert_eq!(p["targetCountry"], "FR");
        assert_eq!(p["contentLanguage"], "fr");
        let a = &p["attributes"];
        assert_eq!(a["couponValueType"], "PERCENT_OFF");
        assert_eq!(a["percentOff"], "20");
        assert_eq!(a["offerType"], "GENERIC_CODE");
        assert_eq!(a["genericRedemptionCode"], "BF20");
        assert_eq!(a["minimumPurchaseAmount"]["amountMicros"], "50000000");
        assert_eq!(a["minimumPurchaseAmount"]["currencyCode"], "EUR");
        assert_eq!(a["productApplicability"], "ALL_PRODUCTS");
        assert_eq!(
            a["promotionEffectiveTimePeriod"]["startTime"],
            "2099-11-20T00:00:00Z"
        );
        assert_eq!(
            a["promotionEffectiveTimePeriod"]["endTime"],
            "2099-11-30T23:59:59Z"
        );
    }

    #[test]
    fn promotion_validation() {
        let mut both = promo();
        both.amount_off = Some(5.0);
        assert!(build_promotion(&both).is_err());

        let mut money = promo();
        money.percent_off = None;
        money.amount_off = Some(10.0);
        money.offer_ids = Some(vec!["sku1".into()]);
        let p = build_promotion(&money).unwrap();
        assert_eq!(p["attributes"]["couponValueType"], "MONEY_OFF");
        assert_eq!(p["attributes"]["productApplicability"], "SPECIFIC_PRODUCTS");

        let mut no_currency = money.clone();
        no_currency.currency = None;
        assert!(build_promotion(&no_currency).is_err());

        let mut past = promo();
        past.start = "2020-01-01".into();
        past.end = "2020-01-02".into();
        assert!(build_promotion(&past).is_err());
    }

    #[test]
    fn status_aggregation() {
        let p = |dests: Value, issues: Value| json!({"productStatus": {"destinationStatuses": dests, "itemLevelIssues": issues}});
        assert_eq!(
            product_status(&p(json!([{"disapprovedCountries": ["FR"]}]), json!([]))),
            "disapproved"
        );
        assert_eq!(
            product_status(&p(
                json!([{"approvedCountries": ["BE"], "disapprovedCountries": ["FR"]}]),
                json!([])
            )),
            "limited"
        );
        assert_eq!(
            product_status(&p(
                json!([{"approvedCountries": ["FR"]}]),
                json!([{"severity": "DEMOTED"}])
            )),
            "limited"
        );
        assert_eq!(
            product_status(&p(json!([{"pendingCountries": ["FR"]}]), json!([]))),
            "pending"
        );
        assert_eq!(
            product_status(&p(json!([{"approvedCountries": ["FR"]}]), json!([]))),
            "approved"
        );
    }

    #[test]
    fn default_rule_puts_supplemental_first_and_keeps_existing() {
        let unset = json!({"primaryProductDataSource": {}});
        let r = default_rule_with_supplemental(&unset, "accounts/1/dataSources/9");
        let list = &r["primaryProductDataSource"]["defaultRule"]["takeFromDataSources"];
        assert_eq!(
            list[0]["supplementalDataSourceName"],
            "accounts/1/dataSources/9"
        );
        assert_eq!(list[1]["self"], true);

        let set = json!({"primaryProductDataSource": {"defaultRule": {"takeFromDataSources": [
            {"supplementalDataSourceName": "accounts/1/dataSources/5"}, {"self": true}
        ]}}});
        let r = default_rule_with_supplemental(&set, "accounts/1/dataSources/9");
        let list = r["primaryProductDataSource"]["defaultRule"]["takeFromDataSources"]
            .as_array()
            .unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(
            list[1]["supplementalDataSourceName"],
            "accounts/1/dataSources/5"
        );
    }

    #[test]
    fn availability_normalisation() {
        assert_eq!(normalize_availability("in stock"), "in_stock");
        assert_eq!(normalize_availability("IN_STOCK"), "in_stock");
    }
}
