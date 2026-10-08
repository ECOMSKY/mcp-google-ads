use serde_json::json;

use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::models::{AdStatus, NextActionHint};
use crate::safety::guards::{
    check_blocked_operation, check_budget_cap, validate_description, validate_headline,
};
use crate::safety::preview::{store_plan, ChangePlan};

/// Convert a dollar amount to micros (Google Ads uses micros: $1 = 1_000_000).
fn dollars_to_micros(dollars: f64) -> i64 {
    (dollars * 1_000_000.0) as i64
}

/// Parameters for creating a Performance Max campaign.
pub struct CreatePmaxCampaignParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_name: &'a str,
    pub daily_budget: f64,
    pub bidding_strategy: &'a str,
    pub final_urls: Vec<String>,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub business_name: &'a str,
    pub geo_target_ids: Vec<String>,
    pub start_paused: bool,
}

/// Create a Performance Max campaign as an atomic batch.
///
/// Uses temporary resource IDs: -1 for budget, -2 for campaign, -3 for asset group.
/// Text assets (headlines, long headlines, descriptions) are created and linked to the asset group.
/// Image assets require separate upload via `upload_image_asset`.
///
/// Validations:
/// - 3-15 headlines (max 30 chars each)
/// - 1-5 long headlines (max 90 chars each)
/// - 2-5 descriptions (max 90 chars each)
/// - business_name max 25 chars
/// - budget cap check
/// - at least one final URL
///
/// Returns a ChangePlan preview that must be confirmed via `confirm_and_apply`.
pub fn create_pmax_campaign(params: &CreatePmaxCampaignParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_pmax_campaign", &params.config.safety)?;
    check_budget_cap(params.daily_budget, &params.config.safety)?;

    // Validate headline count
    if params.headlines.len() < 3 || params.headlines.len() > 15 {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 3-15 headlines, got {}",
            params.headlines.len()
        )));
    }

    // Validate long headline count
    if params.long_headlines.is_empty() || params.long_headlines.len() > 5 {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 1-5 long headlines, got {}",
            params.long_headlines.len()
        )));
    }

    // Validate description count
    if params.descriptions.len() < 2 || params.descriptions.len() > 5 {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 2-5 descriptions, got {}",
            params.descriptions.len()
        )));
    }

    // Validate individual headline lengths (max 30 chars)
    for headline in &params.headlines {
        validate_headline(headline)?;
    }

    // Validate long headlines (max 90 chars)
    for lh in &params.long_headlines {
        validate_description(lh)?;
    }

    // Validate descriptions (max 90 chars)
    for desc in &params.descriptions {
        validate_description(desc)?;
    }

    // Validate business name (max 25 chars)
    if params.business_name.len() > 25 {
        return Err(McpGoogleAdsError::Validation(format!(
            "Business name exceeds 25 character limit ({} chars)",
            params.business_name.len()
        )));
    }

    if params.final_urls.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one final URL is required".to_string(),
        ));
    }

    let cid = crate::client::GoogleAdsClient::normalize_customer_id(params.customer_id);
    let mut operations: Vec<serde_json::Value> = Vec::new();

    // 1. Campaign budget (temp resource ID -1)
    let budget_resource = format!("customers/{}/campaignBudgets/-1", cid);
    operations.push(json!({
        "campaignBudgetOperation": {
            "create": {
                "name": format!("{} Budget", params.campaign_name),
                "amountMicros": dollars_to_micros(params.daily_budget).to_string(),
                "deliveryMethod": "STANDARD",
                // Must be explicit: budgets default to shared, and Google rejects
                // a shared budget on a Performance Max campaign with
                // BIDDING_STRATEGY_TYPE_INCOMPATIBLE_WITH_SHARED_BUDGET.
                "explicitlyShared": false,
                "resourceName": budget_resource
            }
        }
    }));

    // 2. Campaign (temp resource ID -2)
    let campaign_resource = format!("customers/{}/campaigns/-2", cid);
    let resolved_status = if params.start_paused {
        AdStatus::Paused
    } else {
        AdStatus::Enabled
    };

    let mut campaign_create = json!({
        "name": params.campaign_name,
        "status": resolved_status.as_api_str(),
        "advertisingChannelType": "PERFORMANCE_MAX",
        "campaignBudget": budget_resource,
        // Required on every campaign create since the EU political ads
        // regulation; omitting it fails with fieldError=REQUIRED.
        "containsEuPoliticalAdvertising": "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING",
        "resourceName": campaign_resource
    });

    // Apply bidding strategy
    match params.bidding_strategy {
        "MAXIMIZE_CONVERSIONS" => {
            campaign_create
                .as_object_mut()
                .map(|o| o.insert("maximizeConversions".to_string(), json!({})));
        }
        "MAXIMIZE_CONVERSION_VALUE" => {
            campaign_create
                .as_object_mut()
                .map(|o| o.insert("maximizeConversionValue".to_string(), json!({})));
        }
        "TARGET_CPA" => {
            campaign_create
                .as_object_mut()
                .map(|o| o.insert("maximizeConversions".to_string(), json!({})));
        }
        _ => {
            campaign_create
                .as_object_mut()
                .map(|o| o.insert("maximizeConversions".to_string(), json!({})));
        }
    }

    operations.push(json!({
        "campaignOperation": {
            "create": campaign_create
        }
    }));

    // 3. Geo targets
    for geo_id in &params.geo_target_ids {
        operations.push(json!({
            "campaignCriterionOperation": {
                "create": {
                    "campaign": campaign_resource,
                    "location": {
                        "geoTargetConstant": format!("geoTargetConstants/{}", geo_id)
                    }
                }
            }
        }));
    }

    // 4. Asset group (temp resource ID -3) — inherits the campaign-level
    // status so a PMax campaign that opted into ENABLED doesn't ship with
    // a paused asset group blocking traffic.
    let asset_group_resource = format!("customers/{}/assetGroups/-3", cid);
    operations.push(json!({
        "assetGroupOperation": {
            "create": {
                "name": format!("{} Asset Group", params.campaign_name),
                "campaign": campaign_resource,
                "finalUrls": params.final_urls,
                "status": resolved_status.as_api_str(),
                "resourceName": asset_group_resource
            }
        }
    }));

    // 5. Text assets — headlines
    let mut temp_asset_id: i64 = -100;
    for headline in &params.headlines {
        let asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
        operations.push(json!({
            "assetOperation": {
                "create": {
                    "resourceName": asset_resource,
                    "textAsset": {
                        "text": headline
                    }
                }
            }
        }));
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": asset_resource,
                    "fieldType": "HEADLINE"
                }
            }
        }));
        temp_asset_id -= 1;
    }

    // 6. Text assets — long headlines
    for lh in &params.long_headlines {
        let asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
        operations.push(json!({
            "assetOperation": {
                "create": {
                    "resourceName": asset_resource,
                    "textAsset": {
                        "text": lh
                    }
                }
            }
        }));
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": asset_resource,
                    "fieldType": "LONG_HEADLINE"
                }
            }
        }));
        temp_asset_id -= 1;
    }

    // 7. Text assets — descriptions
    for desc in &params.descriptions {
        let asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
        operations.push(json!({
            "assetOperation": {
                "create": {
                    "resourceName": asset_resource,
                    "textAsset": {
                        "text": desc
                    }
                }
            }
        }));
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": asset_resource,
                    "fieldType": "DESCRIPTION"
                }
            }
        }));
        temp_asset_id -= 1;
    }

    // 8. Business name asset
    let biz_asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
    operations.push(json!({
        "assetOperation": {
            "create": {
                "resourceName": biz_asset_resource,
                "textAsset": {
                    "text": params.business_name
                }
            }
        }
    }));
    operations.push(json!({
        "assetGroupAssetOperation": {
            "create": {
                "assetGroup": asset_group_resource,
                "asset": biz_asset_resource,
                "fieldType": "BUSINESS_NAME"
            }
        }
    }));

    let changes = json!({
        "campaign_name": params.campaign_name,
        "daily_budget": params.daily_budget,
        "bidding_strategy": params.bidding_strategy,
        "channel_type": "PERFORMANCE_MAX",
        "headlines_count": params.headlines.len(),
        "long_headlines_count": params.long_headlines.len(),
        "descriptions_count": params.descriptions.len(),
        "business_name": params.business_name,
        "final_urls": params.final_urls,
        "geo_targets": params.geo_target_ids,
        "start_paused": params.start_paused,
        "note": "Image assets require separate upload via upload_image_asset"
    });

    let mut plan = ChangePlan::new(
        "create_pmax_campaign".to_string(),
        "campaign".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        operations,
    )
    .with_status_after_apply(resolved_status);

    if resolved_status == AdStatus::Paused {
        plan = plan.with_next_action_hint(NextActionHint::enable_parent_campaign_after_create(
            "customers/{cid}/campaigns/-2",
        ));
    }

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Field types an existing asset may be linked with when creating an asset group.
const LINKABLE_FIELD_TYPES: &[&str] = &[
    "MARKETING_IMAGE",
    "SQUARE_MARKETING_IMAGE",
    "PORTRAIT_MARKETING_IMAGE",
    "LOGO",
    "LANDSCAPE_LOGO",
    "YOUTUBE_VIDEO",
    "BUSINESS_NAME",
    "CALL_TO_ACTION_SELECTION",
    "HEADLINE",
    "LONG_HEADLINE",
    "DESCRIPTION",
];

/// Max length of a PMax search theme (same limit as `add_asset_group_signal`).
const MAX_ASSET_GROUP_SEARCH_THEME_LEN: usize = 80;

/// An existing asset (image, logo, video, business name…) to link to the new asset group.
#[derive(Debug, Clone)]
pub struct ExistingAssetLink {
    pub asset_id: String,
    pub field_type: String,
}

/// Parameters for adding a new asset group to an EXISTING Performance Max campaign.
pub struct CreateAssetGroupParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    pub asset_group_name: &'a str,
    pub final_urls: Vec<String>,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    /// Optional business name text asset. Omit when the business name is set at
    /// campaign level (brand guidelines) or reused via `existing_assets`.
    pub business_name: Option<&'a str>,
    /// Existing assets to link (typically the images / logos / videos of a
    /// sibling asset group in the same campaign).
    pub existing_assets: Vec<ExistingAssetLink>,
    pub search_themes: Vec<String>,
    /// Audience IDs (numeric part of customers/{cid}/audiences/{id}).
    pub audience_ids: Vec<String>,
    pub status: AdStatus,
}

/// Create a new asset group inside an existing Performance Max campaign, as ONE
/// atomic mutate: the asset group (temp ID -1), its new text assets, links to
/// existing assets (images, logos, videos…) and its signals (search themes and
/// audiences). Google requires the minimum asset set to be present in the same
/// request as the asset group create, which is why everything travels together.
///
/// Returns a ChangePlan preview that must be confirmed via `confirm_and_apply`.
pub fn create_asset_group(params: &CreateAssetGroupParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_asset_group", &params.config.safety)?;

    if params.campaign_id.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "campaign_id is required".to_string(),
        ));
    }
    if params.asset_group_name.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "asset_group_name is required".to_string(),
        ));
    }
    if params.final_urls.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one final URL is required".to_string(),
        ));
    }
    // Counts include existing assets linked with the same field type: Google requires
    // HEADLINE and DESCRIPTION assets to exist BEFORE the asset-group mutate (create
    // them with `create_text_assets`, then pass their IDs in `existing_assets`).
    let linked = |ft: &str| params.existing_assets.iter().filter(|l| l.field_type == ft).count();
    let n_headlines = params.headlines.len() + linked("HEADLINE");
    let n_long = params.long_headlines.len() + linked("LONG_HEADLINE");
    let n_desc = params.descriptions.len() + linked("DESCRIPTION");
    if !(3..=15).contains(&n_headlines) {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 3-15 headlines (new + linked), got {}",
            n_headlines
        )));
    }
    if !(1..=5).contains(&n_long) {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 1-5 long headlines (new + linked), got {}",
            n_long
        )));
    }
    if !(2..=5).contains(&n_desc) {
        return Err(McpGoogleAdsError::Validation(format!(
            "PMax requires 2-5 descriptions (new + linked), got {}",
            n_desc
        )));
    }
    for h in &params.headlines {
        validate_headline(h)?;
    }
    for lh in &params.long_headlines {
        validate_description(lh)?;
    }
    for d in &params.descriptions {
        validate_description(d)?;
    }
    if let Some(name) = params.business_name {
        if name.chars().count() > 25 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Business name exceeds 25 character limit ({} chars)",
                name.chars().count()
            )));
        }
    }
    for link in &params.existing_assets {
        if link.asset_id.trim().is_empty() || !link.asset_id.chars().all(|c| c.is_ascii_digit()) {
            return Err(McpGoogleAdsError::Validation(format!(
                "Invalid existing asset id '{}': must be numeric",
                link.asset_id
            )));
        }
        if !LINKABLE_FIELD_TYPES.contains(&link.field_type.as_str()) {
            return Err(McpGoogleAdsError::Validation(format!(
                "Invalid field_type '{}'. Must be one of: {}",
                link.field_type,
                LINKABLE_FIELD_TYPES.join(", ")
            )));
        }
    }
    if params.search_themes.len() > 50 {
        return Err(McpGoogleAdsError::Validation(format!(
            "An asset group accepts at most 50 search themes, got {}",
            params.search_themes.len()
        )));
    }
    for theme in &params.search_themes {
        if theme.trim().is_empty() {
            return Err(McpGoogleAdsError::Validation(
                "Search theme cannot be empty".to_string(),
            ));
        }
        if theme.chars().count() > MAX_ASSET_GROUP_SEARCH_THEME_LEN {
            return Err(McpGoogleAdsError::Validation(format!(
                "Search theme '{}' is {} chars, exceeds the {} char limit",
                theme,
                theme.chars().count(),
                MAX_ASSET_GROUP_SEARCH_THEME_LEN
            )));
        }
    }

    let cid = crate::client::GoogleAdsClient::normalize_customer_id(params.customer_id);
    let campaign_resource = format!("customers/{}/campaigns/{}", cid, params.campaign_id);
    let asset_group_resource = format!("customers/{}/assetGroups/-1", cid);
    let mut operations: Vec<serde_json::Value> = Vec::new();

    // 1. Asset group
    operations.push(json!({
        "assetGroupOperation": {
            "create": {
                "resourceName": asset_group_resource,
                "name": params.asset_group_name,
                "campaign": campaign_resource,
                "finalUrls": params.final_urls,
                "status": params.status.as_api_str()
            }
        }
    }));

    // 2. New text assets + links
    let mut temp_asset_id: i64 = -100;
    let mut push_text = |ops: &mut Vec<serde_json::Value>, text: &str, field_type: &str| {
        let asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
        ops.push(json!({
            "assetOperation": {
                "create": {
                    "resourceName": asset_resource,
                    "textAsset": { "text": text }
                }
            }
        }));
        ops.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": asset_resource,
                    "fieldType": field_type
                }
            }
        }));
        temp_asset_id -= 1;
    };
    for h in &params.headlines {
        push_text(&mut operations, h, "HEADLINE");
    }
    for lh in &params.long_headlines {
        push_text(&mut operations, lh, "LONG_HEADLINE");
    }
    for d in &params.descriptions {
        push_text(&mut operations, d, "DESCRIPTION");
    }
    if let Some(name) = params.business_name {
        push_text(&mut operations, name, "BUSINESS_NAME");
    }

    // 3. Links to existing assets (images, logos, videos…)
    for link in &params.existing_assets {
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": format!("customers/{}/assets/{}", cid, link.asset_id),
                    "fieldType": link.field_type
                }
            }
        }));
    }

    // 4. Signals
    for theme in &params.search_themes {
        operations.push(json!({
            "assetGroupSignalOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "searchTheme": { "text": theme }
                }
            }
        }));
    }
    for audience_id in &params.audience_ids {
        let audience_resource = if audience_id.starts_with("customers/") {
            audience_id.clone()
        } else {
            format!("customers/{}/audiences/{}", cid, audience_id)
        };
        operations.push(json!({
            "assetGroupSignalOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "audience": { "audience": audience_resource }
                }
            }
        }));
    }

    let changes = json!({
        "campaign_id": params.campaign_id,
        "asset_group_name": params.asset_group_name,
        "final_urls": params.final_urls,
        "status": params.status.as_api_str(),
        "headlines": params.headlines,
        "long_headlines": params.long_headlines,
        "descriptions": params.descriptions,
        "business_name": params.business_name,
        "existing_assets_linked": params.existing_assets.iter()
            .map(|l| json!({"asset_id": l.asset_id, "field_type": l.field_type}))
            .collect::<Vec<_>>(),
        "search_themes_count": params.search_themes.len(),
        "audience_ids": params.audience_ids,
    });

    let plan = ChangePlan::new(
        "create_asset_group".to_string(),
        "asset_group".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        operations,
    )
    .with_status_after_apply(params.status);

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}


/// Create several reusable TEXT assets in one atomic mutate (no links).
///
/// Performance Max requires HEADLINE and DESCRIPTION assets to exist before the
/// asset group that uses them is created; create them here, read the asset IDs
/// from the apply response, then link them via `create_asset_group.existing_assets`.
pub fn create_text_assets(
    config: &Config,
    customer_id: &str,
    texts: &[String],
) -> Result<serde_json::Value> {
    check_blocked_operation("create_text_assets", &config.safety)?;
    if texts.is_empty() || texts.len() > 100 {
        return Err(McpGoogleAdsError::Validation(format!(
            "Provide 1-100 texts, got {}",
            texts.len()
        )));
    }
    for t in texts {
        if t.trim().is_empty() {
            return Err(McpGoogleAdsError::Validation("Text cannot be empty".to_string()));
        }
        if t.chars().count() > 90 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Text '{}' exceeds 90 characters",
                t
            )));
        }
    }
    let cid = crate::client::GoogleAdsClient::normalize_customer_id(customer_id);
    let operations: Vec<serde_json::Value> = texts
        .iter()
        .map(|t| json!({"assetOperation": {"create": {"textAsset": {"text": t}}}}))
        .collect();
    let plan = ChangePlan::new(
        "create_text_assets".to_string(),
        "asset".to_string(),
        "new".to_string(),
        cid,
        json!({"texts": texts, "count": texts.len(),
               "note": "Asset resource names are returned in the apply response, in the same order as texts"}),
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn default_params(config: &Config) -> CreatePmaxCampaignParams<'_> {
        CreatePmaxCampaignParams {
            config,
            customer_id: "123-456-7890",
            campaign_name: "Test PMax",
            daily_budget: 10.0,
            bidding_strategy: "MAXIMIZE_CONVERSIONS",
            final_urls: vec!["https://example.com".to_string()],
            headlines: vec![
                "Headline 1".to_string(),
                "Headline 2".to_string(),
                "Headline 3".to_string(),
            ],
            long_headlines: vec!["Long Headline 1".to_string()],
            descriptions: vec!["Description 1".to_string(), "Description 2".to_string()],
            business_name: "Test Business",
            geo_target_ids: vec!["2840".to_string()],
            start_paused: true,
        }
    }

    #[test]
    fn test_create_pmax_success() {
        let config = Config::default();
        let params = default_params(&config);
        let result = create_pmax_campaign(&params);
        assert!(result.is_ok());
        let preview = result.ok().unwrap_or_default();
        assert_eq!(preview["operation"], "create_pmax_campaign");
        assert_eq!(preview["status"], "PENDING_CONFIRMATION");
    }

    #[test]
    fn test_create_pmax_budget_cap_exceeded() {
        let mut config = Config::default();
        config.safety.max_daily_budget = 5.0;
        let mut params = default_params(&config);
        params.daily_budget = 10.0;
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_too_few_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines = vec!["H1".to_string(), "H2".to_string()];
        let result = create_pmax_campaign(&params);
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("3-15 headlines"));
    }

    #[test]
    fn test_create_pmax_too_many_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines = (0..16).map(|i| format!("H{}", i)).collect();
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_no_long_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.long_headlines = vec![];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_too_few_descriptions() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.descriptions = vec!["D1".to_string()];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_headline_too_long() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines[0] = "A".repeat(31);
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_business_name_too_long() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.business_name = Box::leak("A".repeat(26).into_boxed_str());
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_no_final_urls() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.final_urls = vec![];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_blocked() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["create_pmax_campaign".to_string()];
        let params = default_params(&config);
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_start_paused_true_sets_paused_in_payload() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.start_paused = true;
        let preview = create_pmax_campaign(&params).unwrap();
        assert_eq!(preview["status_after_apply"], "PAUSED");
    }

    #[test]
    fn test_create_pmax_start_paused_false_sets_enabled_in_payload() {
        // Regression for the v0.2.x bug: `start_paused=false` was ignored and
        // the campaign always shipped PAUSED. The fix wires start_paused into
        // the actual mutate operation status.
        let config = Config::default();
        let mut params = default_params(&config);
        params.start_paused = false;
        let preview = create_pmax_campaign(&params).unwrap();
        assert_eq!(preview["status_after_apply"], "ENABLED");
        // No hint when ENABLED — the workflow is complete.
        assert!(preview.get("next_action_hint").is_none() || preview["next_action_hint"].is_null());
    }

    /// Pull the raw mutate operations back off the stored plan.
    fn pmax_ops() -> Vec<serde_json::Value> {
        let config = Config::default();
        let params = default_params(&config);
        let preview = create_pmax_campaign(&params).unwrap();
        let plan_id = preview["plan_id"].as_str().unwrap_or_default();
        let plan = crate::safety::preview::get_plan(plan_id).expect("plan stored");
        plan.mutate_operations.clone()
    }

    #[test]
    fn test_create_pmax_budget_not_shared() {
        // Budgets default to shared server-side, and PMax rejects a shared budget
        // with BIDDING_STRATEGY_TYPE_INCOMPATIBLE_WITH_SHARED_BUDGET. draft_campaign
        // got this fix in 5670d2d; create_pmax_campaign was missed.
        let ops = pmax_ops();
        let budget_create = ops
            .iter()
            .find_map(|op| op.pointer("/campaignBudgetOperation/create"))
            .expect("budget operation present");
        assert_eq!(budget_create["explicitlyShared"], json!(false));
    }

    #[test]
    fn test_create_pmax_sets_eu_political_advertising() {
        // Required on every campaign create; without it the whole mutate fails
        // with fieldError=REQUIRED on contains_eu_political_advertising.
        let ops = pmax_ops();
        let campaign_create = ops
            .iter()
            .find_map(|op| op.pointer("/campaignOperation/create"))
            .expect("campaign operation present");
        assert_eq!(
            campaign_create["containsEuPoliticalAdvertising"],
            json!("DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING")
        );
    }

    // ── create_asset_group ───────────────────────────────────────────

    fn ag_params(config: &Config) -> CreateAssetGroupParams<'_> {
        CreateAssetGroupParams {
            config,
            customer_id: "123-456-7890",
            campaign_id: "111",
            asset_group_name: "Friends",
            final_urls: vec!["https://example.com".to_string()],
            headlines: vec!["H1".into(), "H2".into(), "H3".into()],
            long_headlines: vec!["Long headline".into()],
            descriptions: vec!["D1".into(), "D2".into()],
            business_name: None,
            existing_assets: vec![ExistingAssetLink {
                asset_id: "555".into(),
                field_type: "MARKETING_IMAGE".into(),
            }],
            search_themes: vec!["escape room brussels".into()],
            audience_ids: vec!["319003115".into()],
            status: AdStatus::Enabled,
        }
    }

    #[test]
    fn test_create_asset_group_ops() {
        let config = Config::default();
        let preview = create_asset_group(&ag_params(&config)).unwrap();
        assert_eq!(preview["operation"], "create_asset_group");
        let plan = crate::safety::preview::get_plan(preview["plan_id"].as_str().unwrap()).unwrap();
        let ops = &plan.mutate_operations;
        let ag = ops[0].pointer("/assetGroupOperation/create").expect("asset group first");
        assert_eq!(ag["campaign"], "customers/1234567890/campaigns/111");
        assert_eq!(ag["resourceName"], "customers/1234567890/assetGroups/-1");
        assert_eq!(ag["status"], "ENABLED");
        // 6 text assets created + linked = 12 ops, 1 image link, 1 theme, 1 audience
        assert_eq!(ops.len(), 1 + 12 + 1 + 1 + 1);
        assert!(ops.iter().any(|o| o.pointer("/assetGroupAssetOperation/create/asset")
            == Some(&json!("customers/1234567890/assets/555"))));
        assert!(ops.iter().any(|o| o.pointer("/assetGroupSignalOperation/create/audience/audience")
            == Some(&json!("customers/1234567890/audiences/319003115"))));
        assert!(ops.iter().any(|o| o.pointer("/assetGroupSignalOperation/create/searchTheme/text")
            == Some(&json!("escape room brussels"))));
    }

    #[test]
    fn test_create_asset_group_rejects_bad_field_type() {
        let config = Config::default();
        let mut p = ag_params(&config);
        p.existing_assets[0].field_type = "NOPE".into();
        assert!(create_asset_group(&p).is_err());
    }

    #[test]
    fn test_create_asset_group_rejects_51_themes() {
        let config = Config::default();
        let mut p = ag_params(&config);
        p.search_themes = (0..51).map(|i| format!("t{}", i)).collect();
        assert!(create_asset_group(&p).is_err());
    }

    #[test]
    fn test_create_asset_group_headline_unicode_len() {
        let config = Config::default();
        let mut p = ag_params(&config);
        // 29 chars incl. accents and a curly apostrophe: must pass (char count, not bytes)
        p.headlines[0] = "Essayez quelque chose d’autre".into();
        assert!(create_asset_group(&p).is_ok());
    }

    #[test]
    fn test_create_asset_group_blocked() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["create_asset_group".to_string()];
        assert!(create_asset_group(&ag_params(&config)).is_err());
    }

    #[test]
    fn test_create_asset_group_counts_linked_text_assets() {
        let config = Config::default();
        let mut p = ag_params(&config);
        p.headlines = vec![];
        p.descriptions = vec![];
        for id in ["1", "2", "3"] {
            p.existing_assets.push(ExistingAssetLink { asset_id: id.into(), field_type: "HEADLINE".into() });
        }
        for id in ["4", "5"] {
            p.existing_assets.push(ExistingAssetLink { asset_id: id.into(), field_type: "DESCRIPTION".into() });
        }
        assert!(create_asset_group(&p).is_ok());
        p.existing_assets.retain(|l| l.field_type != "DESCRIPTION");
        assert!(create_asset_group(&p).is_err());
    }

    #[test]
    fn test_create_text_assets() {
        let config = Config::default();
        let preview = create_text_assets(&config, "123-456-7890", &["A".into(), "B".into()]).unwrap();
        let plan = crate::safety::preview::get_plan(preview["plan_id"].as_str().unwrap()).unwrap();
        assert_eq!(plan.mutate_operations.len(), 2);
        assert_eq!(plan.mutate_operations[1]["assetOperation"]["create"]["textAsset"]["text"], "B");
        assert!(create_text_assets(&config, "1", &[]).is_err());
        assert!(create_text_assets(&config, "1", &["x".repeat(91)]).is_err());
    }
}
