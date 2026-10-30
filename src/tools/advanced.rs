//! Advanced write tools: complete Performance Max setup, exclusions,
//! campaign settings, asset helpers and a guarded raw-mutate escape hatch.
//!
//! Every function returns a preview plan to confirm with `confirm_and_apply`.
//! Field names follow the Google Ads API v25 protos and the official
//! `add_performance_max_campaign` / `add_performance_max_retail_campaign`
//! examples.

use serde_json::{json, Value};

use crate::client::{GoogleAdsClient, VALID_MUTATE_OPERATION_KEYS};
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::{check_blocked_operation, check_budget_cap};
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::demand_gen::parse_youtube_id;

fn micros(amount: f64) -> i64 {
    (amount * 1_000_000.0).round() as i64
}

fn invalid(msg: impl Into<String>) -> McpGoogleAdsError {
    McpGoogleAdsError::Validation(msg.into())
}

fn require_numeric(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid(format!("Invalid {} '{}': must be numeric", label, value)));
    }
    Ok(())
}

fn check_texts(kind: &str, items: &[String], min: usize, max: usize, max_chars: usize) -> Result<()> {
    if items.len() < min || items.len() > max {
        return Err(invalid(format!(
            "{} needs {}-{} items, got {}",
            kind,
            min,
            max,
            items.len()
        )));
    }
    for t in items {
        let n = t.chars().count();
        if n == 0 || n > max_chars {
            return Err(invalid(format!(
                "{} '{}' must be 1-{} characters ({} given)",
                kind, t, max_chars, n
            )));
        }
    }
    Ok(())
}

fn finish(
    operation: &str,
    entity_type: &str,
    entity_id: &str,
    cid: String,
    changes: Value,
    double_confirm: bool,
    operations: Vec<Value>,
) -> Value {
    let plan = ChangePlan::new(
        operation.to_string(),
        entity_type.to_string(),
        entity_id.to_string(),
        cid,
        changes,
        double_confirm,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    preview
}

const CALL_TO_ACTIONS: &[&str] = &[
    "LEARN_MORE",
    "GET_QUOTE",
    "APPLY_NOW",
    "SIGN_UP",
    "CONTACT_US",
    "SUBSCRIBE",
    "DOWNLOAD",
    "BOOK_NOW",
    "SHOP_NOW",
    "BUY_NOW",
    "DONATE_NOW",
    "ORDER_NOW",
    "PLAY_NOW",
    "SEE_MORE",
    "START_NOW",
    "VISIT_SITE",
    "WATCH_NOW",
];

// ── Performance Max: complete setup ─────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PmaxImageAssets {
    pub marketing: Vec<String>,
    pub square_marketing: Vec<String>,
    pub portrait_marketing: Vec<String>,
    pub logo: Vec<String>,
    pub landscape_logo: Vec<String>,
}

pub struct CreatePmaxFullParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_name: &'a str,
    pub daily_budget: f64,
    /// MAXIMIZE_CONVERSIONS, TARGET_CPA, MAXIMIZE_CONVERSION_VALUE, TARGET_ROAS.
    pub bidding_strategy: &'a str,
    pub target_cpa: Option<f64>,
    pub target_roas: Option<f64>,
    pub geo_target_ids: Vec<String>,
    pub language_ids: Vec<String>,
    pub final_urls: Vec<String>,
    pub asset_group_name: Option<&'a str>,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub business_name: Option<&'a str>,
    pub images: PmaxImageAssets,
    pub youtube_videos: Vec<String>,
    pub call_to_action: Option<&'a str>,
    pub brand_guidelines: bool,
    /// None = Google default (ON for PMax). Some(false) = opt out.
    pub final_url_expansion: Option<bool>,
    /// None = Google default (ON for PMax). Some(false) = opt out.
    pub text_asset_automation: Option<bool>,
    pub term_exclusions: Vec<String>,
    pub messaging_restrictions: Vec<String>,
    pub search_themes: Vec<String>,
    pub audience_ids: Vec<String>,
    pub negative_keywords: Vec<String>,
    pub url_exclusions: Vec<String>,
    /// Retail: Merchant Center id. Adds shoppingSetting + an "all products" listing group.
    pub merchant_id: Option<i64>,
    pub feed_label: Option<&'a str>,
    pub start_date: Option<&'a str>,
    pub end_date: Option<&'a str>,
    pub start_paused: bool,
}

fn date_time(d: &str, end: bool) -> Result<String> {
    let digits: String = d.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 8 {
        return Err(invalid(format!("Date '{}' must be YYYY-MM-DD", d)));
    }
    Ok(format!(
        "{} {}",
        digits,
        if end { "23:59:59" } else { "00:00:00" }
    ))
}

pub fn create_pmax_campaign_full(p: &CreatePmaxFullParams) -> Result<Value> {
    check_blocked_operation("create_pmax_campaign_full", &p.config.safety)?;
    check_budget_cap(p.daily_budget, &p.config.safety)?;
    if p.campaign_name.trim().is_empty() {
        return Err(invalid("campaign_name is required"));
    }
    if p.final_urls.is_empty() {
        return Err(invalid("At least one final URL is required"));
    }
    if p.geo_target_ids.is_empty() {
        return Err(invalid("geo_target_ids is required"));
    }
    for g in &p.geo_target_ids {
        require_numeric("geo_target_id", g)?;
    }
    for l in &p.language_ids {
        require_numeric("language_id", l)?;
    }
    for a in &p.audience_ids {
        require_numeric("audience_id", a)?;
    }
    let all_images = [
        &p.images.marketing,
        &p.images.square_marketing,
        &p.images.portrait_marketing,
        &p.images.logo,
        &p.images.landscape_logo,
    ];
    for list in all_images {
        for id in list.iter() {
            require_numeric("image asset id", id)?;
        }
    }

    let retail = p.merchant_id.is_some();
    // Text assets: optional for retail (feed-only), required otherwise.
    let has_text = !p.headlines.is_empty() || !p.long_headlines.is_empty() || !p.descriptions.is_empty();
    if !retail || has_text {
        check_texts("Headline", &p.headlines, 3, 15, 30)?;
        check_texts("Long headline", &p.long_headlines, 1, 5, 90)?;
        check_texts("Description", &p.descriptions, 2, 5, 90)?;
        if !p.descriptions.iter().any(|d| d.chars().count() <= 60) {
            return Err(invalid("PMax needs at least one description of 60 characters or less"));
        }
    }
    if !retail {
        if p.business_name.map(|b| b.trim().is_empty()).unwrap_or(true) {
            return Err(invalid("business_name is required"));
        }
        if p.images.logo.is_empty() {
            return Err(invalid("At least one logo (1:1) asset is required"));
        }
        if p.images.marketing.is_empty() || p.images.square_marketing.is_empty() {
            return Err(invalid(
                "At least one landscape (1.91:1) and one square (1:1) marketing image are required",
            ));
        }
    }
    if let Some(b) = p.business_name {
        if b.chars().count() > 25 {
            return Err(invalid("business_name exceeds 25 characters"));
        }
    }
    if let Some(cta) = p.call_to_action {
        if !CALL_TO_ACTIONS.contains(&cta.to_ascii_uppercase().as_str()) {
            return Err(invalid(format!(
                "Unknown call_to_action '{}'. One of: {}",
                cta,
                CALL_TO_ACTIONS.join(", ")
            )));
        }
    }
    for t in &p.search_themes {
        if t.trim().is_empty() || t.chars().count() > 80 {
            return Err(invalid(format!("Search theme '{}' must be 1-80 characters", t)));
        }
    }

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let budget_rn = format!("customers/{}/campaignBudgets/-1", cid);
    let campaign_rn = format!("customers/{}/campaigns/-2", cid);
    let ag_rn = format!("customers/{}/assetGroups/-3", cid);
    let status = if p.start_paused { "PAUSED" } else { "ENABLED" };

    let mut ops: Vec<Value> = Vec::new();
    ops.push(json!({ "campaignBudgetOperation": { "create": {
        "resourceName": budget_rn,
        "name": format!("{} Budget", p.campaign_name),
        "amountMicros": micros(p.daily_budget).to_string(),
        "deliveryMethod": "STANDARD",
        "explicitlyShared": false
    }}}));

    let mut campaign = json!({
        "resourceName": campaign_rn,
        "name": p.campaign_name,
        "status": status,
        "advertisingChannelType": "PERFORMANCE_MAX",
        "campaignBudget": budget_rn,
        "containsEuPoliticalAdvertising": "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING",
        "brandGuidelinesEnabled": p.brand_guidelines
    });
    match p.bidding_strategy.to_ascii_uppercase().as_str() {
        "MAXIMIZE_CONVERSIONS" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = p.target_cpa {
                m.insert("targetCpaMicros".into(), json!(micros(v).to_string()));
            }
            campaign["maximizeConversions"] = Value::Object(m);
        }
        "TARGET_CPA" => {
            let v = p.target_cpa.ok_or_else(|| invalid("TARGET_CPA needs target_cpa"))?;
            campaign["maximizeConversions"] = json!({ "targetCpaMicros": micros(v).to_string() });
        }
        "MAXIMIZE_CONVERSION_VALUE" | "TARGET_ROAS" => {
            let mut m = serde_json::Map::new();
            if let Some(r) = p.target_roas {
                m.insert("targetRoas".into(), json!(r));
            } else if p.bidding_strategy.eq_ignore_ascii_case("TARGET_ROAS") {
                return Err(invalid("TARGET_ROAS needs target_roas"));
            }
            campaign["maximizeConversionValue"] = Value::Object(m);
        }
        other => {
            return Err(invalid(format!(
                "Unsupported PMax bidding_strategy '{}'. Use MAXIMIZE_CONVERSIONS, TARGET_CPA, \
                 MAXIMIZE_CONVERSION_VALUE or TARGET_ROAS.",
                other
            )))
        }
    }
    let mut automation = Vec::new();
    if let Some(on) = p.final_url_expansion {
        automation.push(json!({
            "assetAutomationType": "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION",
            "assetAutomationStatus": if on { "OPTED_IN" } else { "OPTED_OUT" }
        }));
    }
    if let Some(on) = p.text_asset_automation {
        automation.push(json!({
            "assetAutomationType": "TEXT_ASSET_AUTOMATION",
            "assetAutomationStatus": if on { "OPTED_IN" } else { "OPTED_OUT" }
        }));
    }
    if !automation.is_empty() {
        campaign["assetAutomationSettings"] = json!(automation);
    }
    if !p.term_exclusions.is_empty() || !p.messaging_restrictions.is_empty() {
        let restrictions: Vec<Value> = p
            .messaging_restrictions
            .iter()
            .map(|r| json!({ "restrictionText": r, "restrictionType": "RESTRICTION_BASED_EXCLUSION" }))
            .collect();
        campaign["textGuidelines"] = json!({
            "termExclusions": p.term_exclusions,
            "messagingRestrictions": restrictions
        });
    }
    if let Some(mid) = p.merchant_id {
        let mut s = json!({ "merchantId": mid.to_string() });
        if let Some(fl) = p.feed_label {
            s["feedLabel"] = json!(fl);
        }
        campaign["shoppingSetting"] = s;
    }
    if let Some(d) = p.start_date {
        campaign["startDateTime"] = json!(date_time(d, false)?);
    }
    if let Some(d) = p.end_date {
        campaign["endDateTime"] = json!(date_time(d, true)?);
    }
    ops.push(json!({ "campaignOperation": { "create": campaign } }));

    for g in &p.geo_target_ids {
        ops.push(json!({ "campaignCriterionOperation": { "create": {
            "campaign": campaign_rn,
            "location": { "geoTargetConstant": format!("geoTargetConstants/{}", g) }
        }}}));
    }
    for l in &p.language_ids {
        ops.push(json!({ "campaignCriterionOperation": { "create": {
            "campaign": campaign_rn,
            "language": { "languageConstant": format!("languageConstants/{}", l) }
        }}}));
    }
    for kw in &p.negative_keywords {
        ops.push(json!({ "campaignCriterionOperation": { "create": {
            "campaign": campaign_rn,
            "negative": true,
            "keyword": { "text": kw, "matchType": "PHRASE" }
        }}}));
    }
    for u in &p.url_exclusions {
        ops.push(json!({ "campaignCriterionOperation": { "create": {
            "campaign": campaign_rn,
            "negative": true,
            "webpage": {
                "criterionName": format!("Exclude {}", u),
                "conditions": [{ "operand": "URL", "argument": u }]
            }
        }}}));
    }

    ops.push(json!({ "assetGroupOperation": { "create": {
        "resourceName": ag_rn,
        "name": p.asset_group_name.map(str::to_string).unwrap_or_else(|| format!("{} Asset Group", p.campaign_name)),
        "campaign": campaign_rn,
        "finalUrls": p.final_urls,
        "status": status
    }}}));

    let mut temp: i64 = -100;
    let mut new_text = |ops: &mut Vec<Value>, text: &str, field: &str| {
        let rn = format!("customers/{}/assets/{}", cid, temp);
        temp -= 1;
        ops.push(json!({ "assetOperation": { "create": { "resourceName": rn, "textAsset": { "text": text } } } }));
        ops.push(json!({ "assetGroupAssetOperation": { "create": {
            "assetGroup": ag_rn, "asset": rn, "fieldType": field
        }}}));
    };
    for h in &p.headlines {
        new_text(&mut ops, h, "HEADLINE");
    }
    for h in &p.long_headlines {
        new_text(&mut ops, h, "LONG_HEADLINE");
    }
    for d in &p.descriptions {
        new_text(&mut ops, d, "DESCRIPTION");
    }

    // Business name + logos: campaign assets under brand guidelines, else asset group assets.
    let link_brand = |ops: &mut Vec<Value>, asset: String, field: &str| {
        if p.brand_guidelines {
            ops.push(json!({ "campaignAssetOperation": { "create": {
                "campaign": campaign_rn, "asset": asset, "fieldType": field
            }}}));
        } else {
            ops.push(json!({ "assetGroupAssetOperation": { "create": {
                "assetGroup": ag_rn, "asset": asset, "fieldType": field
            }}}));
        }
    };
    if let Some(b) = p.business_name {
        let rn = format!("customers/{}/assets/-90", cid);
        ops.push(json!({ "assetOperation": { "create": { "resourceName": rn, "textAsset": { "text": b } } } }));
        link_brand(&mut ops, rn, "BUSINESS_NAME");
    }
    for id in &p.images.logo {
        link_brand(&mut ops, format!("customers/{}/assets/{}", cid, id), "LOGO");
    }
    for id in &p.images.landscape_logo {
        link_brand(&mut ops, format!("customers/{}/assets/{}", cid, id), "LANDSCAPE_LOGO");
    }
    let link = |ops: &mut Vec<Value>, ids: &[String], field: &str| {
        for id in ids {
            ops.push(json!({ "assetGroupAssetOperation": { "create": {
                "assetGroup": ag_rn,
                "asset": format!("customers/{}/assets/{}", cid, id),
                "fieldType": field
            }}}));
        }
    };
    link(&mut ops, &p.images.marketing, "MARKETING_IMAGE");
    link(&mut ops, &p.images.square_marketing, "SQUARE_MARKETING_IMAGE");
    link(&mut ops, &p.images.portrait_marketing, "PORTRAIT_MARKETING_IMAGE");

    let mut video_ids = Vec::new();
    for (i, v) in p.youtube_videos.iter().enumerate() {
        let id = parse_youtube_id(v)?;
        if video_ids.contains(&id) {
            continue;
        }
        let rn = format!("customers/{}/assets/-{}", cid, 200 + i);
        ops.push(json!({ "assetOperation": { "create": {
            "resourceName": rn, "name": format!("YouTube {}", id),
            "youtubeVideoAsset": { "youtubeVideoId": id }
        }}}));
        ops.push(json!({ "assetGroupAssetOperation": { "create": {
            "assetGroup": ag_rn, "asset": rn, "fieldType": "YOUTUBE_VIDEO"
        }}}));
        video_ids.push(id);
    }
    if let Some(cta) = p.call_to_action {
        let rn = format!("customers/{}/assets/-91", cid);
        ops.push(json!({ "assetOperation": { "create": {
            "resourceName": rn, "callToActionAsset": { "callToAction": cta.to_ascii_uppercase() }
        }}}));
        ops.push(json!({ "assetGroupAssetOperation": { "create": {
            "assetGroup": ag_rn, "asset": rn, "fieldType": "CALL_TO_ACTION_SELECTION"
        }}}));
    }
    for theme in &p.search_themes {
        ops.push(json!({ "assetGroupSignalOperation": { "create": {
            "assetGroup": ag_rn, "searchTheme": { "text": theme }
        }}}));
    }
    for a in &p.audience_ids {
        ops.push(json!({ "assetGroupSignalOperation": { "create": {
            "assetGroup": ag_rn, "audience": { "audience": format!("customers/{}/audiences/{}", cid, a) }
        }}}));
    }
    if retail {
        ops.push(json!({ "assetGroupListingGroupFilterOperation": { "create": {
            "assetGroup": ag_rn,
            "type": "UNIT_INCLUDED",
            "listingSource": "SHOPPING"
        }}}));
    }

    let changes = json!({
        "campaign_name": p.campaign_name,
        "daily_budget": p.daily_budget,
        "bidding_strategy": p.bidding_strategy,
        "target_cpa": p.target_cpa,
        "target_roas": p.target_roas,
        "status": status,
        "geo_target_ids": p.geo_target_ids,
        "language_ids": p.language_ids,
        "final_urls": p.final_urls,
        "final_url_expansion": p.final_url_expansion.map(|b| if b { "ON" } else { "OFF" }).unwrap_or("Google default (ON)"),
        "text_asset_automation": p.text_asset_automation.map(|b| if b { "ON" } else { "OFF" }).unwrap_or("Google default (ON)"),
        "brand_guidelines": p.brand_guidelines,
        "headlines": p.headlines.len(),
        "long_headlines": p.long_headlines.len(),
        "descriptions": p.descriptions.len(),
        "images": {
            "marketing": p.images.marketing.len(),
            "square_marketing": p.images.square_marketing.len(),
            "portrait_marketing": p.images.portrait_marketing.len(),
            "logo": p.images.logo.len(),
            "landscape_logo": p.images.landscape_logo.len()
        },
        "youtube_videos": video_ids,
        "call_to_action": p.call_to_action,
        "search_themes": p.search_themes,
        "audience_signals": p.audience_ids,
        "negative_keywords": p.negative_keywords,
        "url_exclusions": p.url_exclusions,
        "term_exclusions": p.term_exclusions,
        "messaging_restrictions": p.messaging_restrictions,
        "merchant_id": p.merchant_id,
        "start_date": p.start_date,
        "end_date": p.end_date,
        "operations": ops.len()
    });
    Ok(finish("create_pmax_campaign_full", "campaign", "new", cid, changes, false, ops))
}

/// Opt a campaign in or out of asset automation features (PMax / Demand Gen / Search).
pub fn set_asset_automation(
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    settings: &[(String, bool)],
) -> Result<Value> {
    check_blocked_operation("set_asset_automation", &config.safety)?;
    require_numeric("campaign_id", campaign_id)?;
    const TYPES: &[&str] = &[
        "TEXT_ASSET_AUTOMATION",
        "GENERATE_VERTICAL_YOUTUBE_VIDEOS",
        "GENERATE_SHORTER_YOUTUBE_VIDEOS",
        "GENERATE_LANDING_PAGE_PREVIEW",
        "GENERATE_LANDING_PAGE_TEXT",
        "GENERATE_ENHANCED_YOUTUBE_VIDEOS",
        "GENERATE_IMAGE_ENHANCEMENT",
        "GENERATE_IMAGE_EXTRACTION",
        "GENERATE_DESIGN_VERSIONS_FOR_IMAGES",
        "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION",
        "GENERATE_VIDEOS_FROM_OTHER_ASSETS",
        "GENERATE_ANIMATED_IMAGES_FROM_OTHER_ASSETS",
    ];
    if settings.is_empty() {
        return Err(invalid("Give at least one setting"));
    }
    let mut list = Vec::new();
    for (t, on) in settings {
        let t = t.to_ascii_uppercase();
        if !TYPES.contains(&t.as_str()) {
            return Err(invalid(format!("Unknown automation type '{}'. One of: {}", t, TYPES.join(", "))));
        }
        list.push(json!({
            "assetAutomationType": t,
            "assetAutomationStatus": if *on { "OPTED_IN" } else { "OPTED_OUT" }
        }));
    }
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let op = json!({ "campaignOperation": {
        "update": {
            "resourceName": format!("customers/{}/campaigns/{}", cid, campaign_id),
            "assetAutomationSettings": list
        },
        "updateMask": "assetAutomationSettings"
    }});
    let changes = json!({
        "campaign_id": campaign_id,
        "settings": settings.iter().map(|(t, on)| json!({"type": t, "opted_in": on})).collect::<Vec<_>>()
    });
    Ok(finish("set_asset_automation", "campaign", campaign_id, cid, changes, false, vec![op]))
}

// ── Exclusions ──────────────────────────────────────────────────────────

pub struct CampaignExclusionsParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    /// Websites / app placements (e.g. "example.com").
    pub placements: Vec<String>,
    /// YouTube channel ids (UC...).
    pub youtube_channels: Vec<String>,
    /// YouTube video ids or URLs.
    pub youtube_videos: Vec<String>,
    /// ContentLabelType values (JUVENILE, TRAGEDY, LIVE_STREAMING_VIDEO, ...).
    pub content_labels: Vec<String>,
    /// URLs or URL fragments to exclude from landing pages (PMax / DSA).
    pub url_exclusions: Vec<String>,
    /// Devices to exclude via a -100% bid modifier: MOBILE, TABLET, DESKTOP, CONNECTED_TV.
    pub exclude_devices: Vec<String>,
    /// Topic constant ids to exclude.
    pub topic_ids: Vec<String>,
}

const CONTENT_LABELS: &[&str] = &[
    "SEXUALLY_SUGGESTIVE",
    "BELOW_THE_FOLD",
    "PARKED_DOMAIN",
    "JUVENILE",
    "PROFANITY",
    "TRAGEDY",
    "VIDEO",
    "VIDEO_RATING_DV_G",
    "VIDEO_RATING_DV_PG",
    "VIDEO_RATING_DV_T",
    "VIDEO_RATING_DV_MA",
    "VIDEO_NOT_YET_RATED",
    "EMBEDDED_VIDEO",
    "LIVE_STREAMING_VIDEO",
    "SOCIAL_ISSUES",
    "BRAND_SUITABILITY_CONTENT_FOR_FAMILIES",
    "BRAND_SUITABILITY_GAMES_FIGHTING",
    "BRAND_SUITABILITY_GAMES_MATURE",
    "BRAND_SUITABILITY_HEALTH_SENSITIVE",
    "BRAND_SUITABILITY_HEALTH_SOURCE_UNDETERMINED",
    "BRAND_SUITABILITY_NEWS_RECENT",
    "BRAND_SUITABILITY_NEWS_SENSITIVE",
    "BRAND_SUITABILITY_NEWS_SOURCE_NOT_FEATURED",
    "BRAND_SUITABILITY_POLITICS",
    "BRAND_SUITABILITY_RELIGION",
];

pub fn add_campaign_exclusions(p: &CampaignExclusionsParams) -> Result<Value> {
    check_blocked_operation("add_campaign_exclusions", &p.config.safety)?;
    require_numeric("campaign_id", p.campaign_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let campaign_rn = format!("customers/{}/campaigns/{}", cid, p.campaign_id);
    let mut ops = Vec::new();
    let neg = |criterion: Value| -> Value {
        let mut c = json!({ "campaign": campaign_rn, "negative": true });
        if let (Some(o), Some(src)) = (c.as_object_mut(), criterion.as_object()) {
            for (k, v) in src {
                o.insert(k.clone(), v.clone());
            }
        }
        json!({ "campaignCriterionOperation": { "create": c } })
    };
    for u in &p.placements {
        let u = u.trim();
        if u.is_empty() {
            continue;
        }
        ops.push(neg(json!({ "placement": { "url": u } })));
    }
    for ch in &p.youtube_channels {
        let ch = ch.trim();
        if !ch.starts_with("UC") {
            return Err(invalid(format!("YouTube channel id '{}' must start with UC", ch)));
        }
        ops.push(neg(json!({ "youtubeChannel": { "channelId": ch } })));
    }
    for v in &p.youtube_videos {
        let id = parse_youtube_id(v)?;
        ops.push(neg(json!({ "youtubeVideo": { "videoId": id } })));
    }
    for l in &p.content_labels {
        let l = l.to_ascii_uppercase();
        if !CONTENT_LABELS.contains(&l.as_str()) {
            return Err(invalid(format!("Unknown content label '{}'. One of: {}", l, CONTENT_LABELS.join(", "))));
        }
        ops.push(neg(json!({ "contentLabel": { "type": l } })));
    }
    for u in &p.url_exclusions {
        ops.push(neg(json!({ "webpage": {
            "criterionName": format!("Exclude {}", u),
            "conditions": [{ "operand": "URL", "argument": u }]
        }})));
    }
    for t in &p.topic_ids {
        require_numeric("topic_id", t)?;
        ops.push(neg(json!({ "topic": { "topicConstant": format!("topicConstants/{}", t) } })));
    }
    for d in &p.exclude_devices {
        let d = d.to_ascii_uppercase();
        if !["MOBILE", "TABLET", "DESKTOP", "CONNECTED_TV"].contains(&d.as_str()) {
            return Err(invalid(format!("Unknown device '{}'", d)));
        }
        ops.push(json!({ "campaignCriterionOperation": { "create": {
            "campaign": campaign_rn,
            "device": { "type": d },
            "bidModifier": 0.0
        }}}));
    }
    if ops.is_empty() {
        return Err(invalid("Nothing to exclude"));
    }
    let changes = json!({
        "campaign_id": p.campaign_id,
        "placements": p.placements,
        "youtube_channels": p.youtube_channels,
        "youtube_videos": p.youtube_videos,
        "content_labels": p.content_labels,
        "url_exclusions": p.url_exclusions,
        "topic_ids": p.topic_ids,
        "exclude_devices": p.exclude_devices,
        "note": "Each kind is accepted only by the campaign types that support it; the apply is atomic, so if one is rejected nothing is written."
    });
    Ok(finish("add_campaign_exclusions", "campaign_criterion", p.campaign_id, cid, changes, false, ops))
}

// ── Campaign settings ───────────────────────────────────────────────────

pub struct CampaignSettingsParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    pub name: Option<&'a str>,
    pub final_url_suffix: Option<&'a str>,
    pub tracking_url_template: Option<&'a str>,
    pub start_date: Option<&'a str>,
    pub end_date: Option<&'a str>,
}

pub fn update_campaign_settings(p: &CampaignSettingsParams) -> Result<Value> {
    check_blocked_operation("update_campaign_settings", &p.config.safety)?;
    require_numeric("campaign_id", p.campaign_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let mut update = json!({ "resourceName": format!("customers/{}/campaigns/{}", cid, p.campaign_id) });
    let mut mask = Vec::new();
    if let Some(n) = p.name {
        update["name"] = json!(n);
        mask.push("name");
    }
    if let Some(s) = p.final_url_suffix {
        update["finalUrlSuffix"] = json!(s.trim_start_matches('?'));
        mask.push("finalUrlSuffix");
    }
    if let Some(t) = p.tracking_url_template {
        update["trackingUrlTemplate"] = json!(t);
        mask.push("trackingUrlTemplate");
    }
    if let Some(d) = p.start_date {
        update["startDateTime"] = json!(date_time(d, false)?);
        mask.push("startDateTime");
    }
    if let Some(d) = p.end_date {
        update["endDateTime"] = json!(date_time(d, true)?);
        mask.push("endDateTime");
    }
    if mask.is_empty() {
        return Err(invalid("Nothing to update"));
    }
    let op = json!({ "campaignOperation": { "update": update, "updateMask": mask.join(",") } });
    let changes = json!({
        "campaign_id": p.campaign_id,
        "name": p.name,
        "final_url_suffix": p.final_url_suffix,
        "tracking_url_template": p.tracking_url_template,
        "start_date": p.start_date,
        "end_date": p.end_date
    });
    Ok(finish("update_campaign_settings", "campaign", p.campaign_id, cid, changes, false, vec![op]))
}

/// Change channels / optimized targeting on an existing Demand Gen ad group.
pub fn update_demand_gen_ad_group(
    config: &Config,
    customer_id: &str,
    ad_group_id: &str,
    channels: Option<&crate::tools::demand_gen::DemandGenChannels>,
    optimized_targeting: Option<bool>,
) -> Result<Value> {
    check_blocked_operation("update_demand_gen_ad_group", &config.safety)?;
    require_numeric("ad_group_id", ad_group_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let mut update = json!({ "resourceName": format!("customers/{}/adGroups/{}", cid, ad_group_id) });
    let mut mask = Vec::new();
    if let Some(c) = channels {
        update["demandGenAdGroupSettings"] = json!({ "channelControls": { "selectedChannels": {
            "youtubeInStream": c.youtube_in_stream,
            "youtubeInFeed": c.youtube_in_feed,
            "youtubeShorts": c.youtube_shorts,
            "discover": c.discover,
            "gmail": c.gmail,
            "display": c.display,
            "maps": c.maps
        }}});
        mask.push("demandGenAdGroupSettings.channelControls.selectedChannels");
    }
    if let Some(o) = optimized_targeting {
        update["optimizedTargetingEnabled"] = json!(o);
        mask.push("optimizedTargetingEnabled");
    }
    if mask.is_empty() {
        return Err(invalid("Nothing to update"));
    }
    let op = json!({ "adGroupOperation": { "update": update, "updateMask": mask.join(",") } });
    let changes = json!({ "ad_group_id": ad_group_id, "updated": mask });
    Ok(finish("update_demand_gen_ad_group", "ad_group", ad_group_id, cid, changes, false, vec![op]))
}

// ── Asset helpers ───────────────────────────────────────────────────────

pub fn create_youtube_video_assets(config: &Config, customer_id: &str, videos: &[String]) -> Result<Value> {
    check_blocked_operation("create_youtube_video_assets", &config.safety)?;
    if videos.is_empty() || videos.len() > 50 {
        return Err(invalid("Give 1-50 videos"));
    }
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let mut ids = Vec::new();
    for v in videos {
        let id = parse_youtube_id(v)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let ops = ids
        .iter()
        .map(|id| json!({ "assetOperation": { "create": {
            "name": format!("YouTube {}", id),
            "youtubeVideoAsset": { "youtubeVideoId": id }
        }}}))
        .collect();
    let changes = json!({ "youtube_video_ids": ids, "note": "Asset resource names come back in the apply response, in this order." });
    Ok(finish("create_youtube_video_assets", "asset", "new", cid, changes, false, ops))
}

/// Image asset from raw bytes (fetched by the caller from a URL).
pub fn image_asset_from_bytes(
    config: &Config,
    customer_id: &str,
    asset_name: &str,
    bytes: &[u8],
    source_url: &str,
) -> Result<Value> {
    use base64::Engine;
    check_blocked_operation("upload_image_asset_from_url", &config.safety)?;
    if asset_name.trim().is_empty() {
        return Err(invalid("asset_name is required"));
    }
    if bytes.is_empty() || bytes.len() > 5_120_000 {
        return Err(invalid(format!("Image must be 1 byte to 5 MB (got {} bytes)", bytes.len())));
    }
    let kind = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "PNG"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "JPEG"
    } else if bytes.starts_with(b"GIF8") {
        "GIF"
    } else {
        return Err(invalid("Unsupported image: only PNG, JPEG or GIF"));
    };
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let op = json!({ "assetOperation": { "create": {
        "name": asset_name,
        "type": "IMAGE",
        "imageAsset": { "data": base64::engine::general_purpose::STANDARD.encode(bytes) }
    }}});
    let changes = json!({ "asset_name": asset_name, "source_url": source_url, "format": kind, "size_bytes": bytes.len() });
    Ok(finish("upload_image_asset_from_url", "asset", "new", cid, changes, false, vec![op]))
}

// ── Raw mutate escape hatch ─────────────────────────────────────────────

/// Any `googleAds:mutate` operations, checked before anything is sent:
/// keys must be valid MutateOperation keys, new/updated budgets must respect
/// the daily budget cap, and any `remove` requires a double confirmation.
pub fn mutate_raw(config: &Config, customer_id: &str, description: &str, operations: Vec<Value>) -> Result<Value> {
    check_blocked_operation("mutate_raw", &config.safety)?;
    if description.trim().is_empty() {
        return Err(invalid("description is required: say what these operations do"));
    }
    if operations.is_empty() || operations.len() > 1000 {
        return Err(invalid("Give 1-1000 operations"));
    }
    let mut has_remove = false;
    let mut kinds = Vec::new();
    for (i, op) in operations.iter().enumerate() {
        let obj = op
            .as_object()
            .ok_or_else(|| invalid(format!("Operation {} is not an object", i)))?;
        if obj.len() != 1 {
            return Err(invalid(format!("Operation {} must have exactly one key", i)));
        }
        let (key, body) = obj.iter().next().expect("one key");
        if !VALID_MUTATE_OPERATION_KEYS.contains(&key.as_str()) {
            return Err(invalid(format!("Operation {}: '{}' is not a googleAds:mutate operation", i, key)));
        }
        if body.get("remove").is_some() {
            has_remove = true;
        }
        if key == "campaignBudgetOperation" {
            for part in ["create", "update"] {
                if let Some(amount) = body.get(part).and_then(|b| b.get("amountMicros")) {
                    let micros_val = amount
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .or_else(|| amount.as_f64())
                        .unwrap_or(0.0);
                    check_budget_cap(micros_val / 1_000_000.0, &config.safety)?;
                }
            }
        }
        if !kinds.contains(key) {
            kinds.push(key.clone());
        }
    }
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let changes = json!({
        "description": description,
        "operation_count": operations.len(),
        "operation_kinds": kinds,
        "contains_remove": has_remove,
        "operations": operations
    });
    Ok(finish("mutate_raw", "raw", "batch", cid, changes, has_remove, operations))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_rejects_unknown_key_and_flags_remove() {
        let cfg = Config::default();
        assert!(mutate_raw(&cfg, "1", "x", vec![json!({"notAMutation": {}})]).is_err());
        let p = mutate_raw(&cfg, "1", "x", vec![json!({"campaignOperation": {"remove": "customers/1/campaigns/2"}})]).unwrap();
        assert_eq!(p["requires_double_confirm"], true);
    }

    #[test]
    fn pmax_requires_images_unless_retail() {
        let cfg = Config::default();
        let base = CreatePmaxFullParams {
            config: &cfg,
            customer_id: "1",
            campaign_name: "x",
            daily_budget: 10.0,
            bidding_strategy: "MAXIMIZE_CONVERSIONS",
            target_cpa: None,
            target_roas: None,
            geo_target_ids: vec!["2840".into()],
            language_ids: vec![],
            final_urls: vec!["https://a.com".into()],
            asset_group_name: None,
            headlines: vec![],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            images: PmaxImageAssets::default(),
            youtube_videos: vec![],
            call_to_action: None,
            brand_guidelines: false,
            final_url_expansion: Some(false),
            text_asset_automation: None,
            term_exclusions: vec![],
            messaging_restrictions: vec![],
            search_themes: vec![],
            audience_ids: vec![],
            negative_keywords: vec![],
            url_exclusions: vec![],
            merchant_id: None,
            feed_label: None,
            start_date: None,
            end_date: None,
            start_paused: true,
        };
        assert!(create_pmax_campaign_full(&base).is_err());
        let retail = CreatePmaxFullParams { merchant_id: Some(123), ..base };
        let preview = create_pmax_campaign_full(&retail).unwrap();
        assert_eq!(preview["changes"]["merchant_id"], 123);
    }
}

// ── Search completeness: brand assets + audit ───────────────────────────

pub struct CampaignBrandAssetsParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    pub business_name: Option<&'a str>,
    /// Existing square IMAGE asset ids (1:1, >= 128px) used as business logo.
    pub business_logo_asset_ids: Vec<String>,
    /// Existing IMAGE asset ids used as image extensions (1:1 and 1.91:1).
    pub image_asset_ids: Vec<String>,
    /// Call extension: (country code, phone number).
    pub call: Option<(&'a str, &'a str)>,
}

/// Link business name, business logo, image and call assets to a campaign
/// (Search and other campaign types that read campaign-level assets).
pub fn add_campaign_brand_assets(p: &CampaignBrandAssetsParams) -> Result<Value> {
    check_blocked_operation("add_campaign_brand_assets", &p.config.safety)?;
    require_numeric("campaign_id", p.campaign_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let campaign_rn = format!("customers/{}/campaigns/{}", cid, p.campaign_id);
    let mut ops = Vec::new();
    let link = |ops: &mut Vec<Value>, asset: String, field: &str| {
        ops.push(json!({ "campaignAssetOperation": { "create": {
            "campaign": campaign_rn, "asset": asset, "fieldType": field
        }}}));
    };
    if let Some(b) = p.business_name {
        if b.trim().is_empty() || b.chars().count() > 25 {
            return Err(invalid("business_name must be 1-25 characters"));
        }
        let rn = format!("customers/{}/assets/-1", cid);
        ops.push(json!({ "assetOperation": { "create": { "resourceName": rn, "textAsset": { "text": b } } } }));
        link(&mut ops, rn, "BUSINESS_NAME");
    }
    for id in &p.business_logo_asset_ids {
        require_numeric("business_logo_asset_id", id)?;
        link(&mut ops, format!("customers/{}/assets/{}", cid, id), "BUSINESS_LOGO");
    }
    for id in &p.image_asset_ids {
        require_numeric("image_asset_id", id)?;
        link(&mut ops, format!("customers/{}/assets/{}", cid, id), "AD_IMAGE");
    }
    if let Some((country, phone)) = p.call {
        if country.len() != 2 || phone.trim().is_empty() {
            return Err(invalid("call needs a 2-letter country code and a phone number"));
        }
        let rn = format!("customers/{}/assets/-2", cid);
        ops.push(json!({ "assetOperation": { "create": {
            "resourceName": rn,
            "callAsset": { "countryCode": country.to_ascii_uppercase(), "phoneNumber": phone }
        }}}));
        link(&mut ops, rn, "CALL");
    }
    if ops.is_empty() {
        return Err(invalid("Nothing to add"));
    }
    let changes = json!({
        "campaign_id": p.campaign_id,
        "business_name": p.business_name,
        "business_logo_asset_ids": p.business_logo_asset_ids,
        "image_asset_ids": p.image_asset_ids,
        "call": p.call.map(|(c, n)| format!("{} {}", c, n)),
    });
    Ok(finish("add_campaign_brand_assets", "campaign_asset", p.campaign_id, cid, changes, false, ops))
}

/// Minimum asset counts a Search campaign must carry to be "complete".
pub const SEARCH_ASSET_MINIMUMS: &[(&str, usize)] = &[
    ("SITELINK", 4),
    ("CALLOUT", 4),
    ("STRUCTURED_SNIPPET", 1),
    ("AD_IMAGE", 1),
    ("BUSINESS_NAME", 1),
    ("BUSINESS_LOGO", 1),
];

/// Grade a Search campaign from GAQL rows: RSA rows (ad_group_ad) and the
/// field types of every enabled asset reachable from the campaign
/// (campaign, ad group and account level).
pub fn evaluate_search_completeness(ad_rows: &[Value], asset_field_types: &[String]) -> Value {
    let mut problems = Vec::new();
    let mut ads = Vec::new();
    for row in ad_rows {
        let ad = &row["adGroupAd"]["ad"];
        let rsa = &ad["responsiveSearchAd"];
        let h = rsa["headlines"].as_array().map(|a| a.len()).unwrap_or(0);
        let d = rsa["descriptions"].as_array().map(|a| a.len()).unwrap_or(0);
        let path1 = rsa["path1"].as_str().unwrap_or("");
        let id = ad["id"].as_str().map(str::to_string).unwrap_or_else(|| ad["id"].to_string());
        let mut missing = Vec::new();
        if h < 15 {
            missing.push(format!("{} headlines missing ({} / 15)", 15 - h, h));
        }
        if d < 4 {
            missing.push(format!("{} descriptions missing ({} / 4)", 4 - d, d));
        }
        if path1.is_empty() {
            missing.push("display path empty".to_string());
        }
        if !missing.is_empty() {
            problems.push(format!("Ad {}: {}", id, missing.join(", ")));
        }
        ads.push(json!({ "ad_id": id, "headlines": h, "descriptions": d, "path1": path1, "missing": missing }));
    }
    if ad_rows.is_empty() {
        problems.push("No enabled responsive search ad".to_string());
    }
    let mut assets = serde_json::Map::new();
    for (field, min) in SEARCH_ASSET_MINIMUMS {
        let n = asset_field_types.iter().filter(|f| f.as_str() == *field).count();
        assets.insert(field.to_string(), json!({ "count": n, "minimum": min }));
        if n < *min {
            problems.push(format!("{}: {} / {} minimum", field, n, min));
        }
    }
    json!({
        "complete": problems.is_empty(),
        "problems": problems,
        "ads": ads,
        "assets": assets
    })
}
