//! Demand Gen campaigns (YouTube / Discover / Gmail / Display).
//!
//! Built to keep Demand Gen traffic tight by default:
//! - channels are explicit (`selected_channels`), never "all channels";
//! - optimized targeting is OFF unless the caller turns it on;
//! - geo + language are set on the ad group (where Demand Gen reads them);
//! - audiences are built from custom segments (what people searched on
//!   Google, or sites they browse), not broad interests.
//!
//! Every function returns a preview plan to confirm with `confirm_and_apply`.
//!
//! Shapes follow the Google Ads API v25 protos (`AdGroup.DemandGenAdGroupSettings`,
//! `DemandGenVideoResponsiveAdInfo`, `CustomAudience`, `Audience`) and the
//! official `add_demand_gen_campaign` example.

use serde_json::json;

use crate::client::GoogleAdsClient;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::{check_blocked_operation, check_budget_cap};
use crate::safety::preview::{store_plan, ChangePlan, PlanDispatch};

fn micros(amount: f64) -> i64 {
    (amount * 1_000_000.0).round() as i64
}

fn require_numeric(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "Invalid {} '{}': must be numeric",
            label, value
        )));
    }
    Ok(())
}

fn check_len(kind: &str, items: &[String], min: usize, max: usize, max_chars: usize) -> Result<()> {
    if items.len() < min || items.len() > max {
        return Err(McpGoogleAdsError::Validation(format!(
            "{} needs {}-{} items, got {}",
            kind,
            min,
            max,
            items.len()
        )));
    }
    for item in items {
        let n = item.chars().count();
        if n == 0 || n > max_chars {
            return Err(McpGoogleAdsError::Validation(format!(
                "{} '{}' must be 1-{} characters ({} given)",
                kind, item, max_chars, n
            )));
        }
    }
    Ok(())
}

/// Extract an 11-char YouTube video id from an id or any youtube URL
/// (`watch?v=`, `youtu.be/`, `shorts/`, `embed/`).
pub fn parse_youtube_id(input: &str) -> Result<String> {
    let s = input.trim();
    let is_id = |c: &str| {
        c.len() == 11
            && c
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    };
    if is_id(s) {
        return Ok(s.to_string());
    }
    let candidates = ["v=", "youtu.be/", "shorts/", "embed/", "live/"];
    for marker in candidates {
        if let Some(pos) = s.find(marker) {
            let rest = &s[pos + marker.len()..];
            let id: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
                .collect();
            if is_id(&id) {
                return Ok(id);
            }
        }
    }
    Err(McpGoogleAdsError::Validation(format!(
        "Could not read a YouTube video id from '{}'",
        input
    )))
}

// ── 1. Campaign ──────────────────────────────────────────────────────────

pub struct CreateDemandGenCampaignParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_name: &'a str,
    pub daily_budget: f64,
    /// MAXIMIZE_CLICKS, MAXIMIZE_CONVERSIONS, TARGET_CPA,
    /// MAXIMIZE_CONVERSION_VALUE, TARGET_ROAS.
    pub bidding_strategy: &'a str,
    pub target_cpa: Option<f64>,
    pub target_roas: Option<f64>,
    /// Only for MAXIMIZE_CLICKS: max CPC ceiling in account currency.
    pub max_cpc: Option<f64>,
    pub start_paused: bool,
}

/// Budget (temp -1) + Demand Gen campaign (temp -2). Ad groups and ads are
/// added with the other tools once the campaign id is known.
pub fn create_demand_gen_campaign(p: &CreateDemandGenCampaignParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_demand_gen_campaign", &p.config.safety)?;
    check_budget_cap(p.daily_budget, &p.config.safety)?;
    if p.campaign_name.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation("campaign_name is required".into()));
    }

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let budget_rn = format!("customers/{}/campaignBudgets/-1", cid);
    let campaign_rn = format!("customers/{}/campaigns/-2", cid);
    let status = if p.start_paused { "PAUSED" } else { "ENABLED" };

    let mut campaign = json!({
        "resourceName": campaign_rn,
        "name": p.campaign_name,
        "status": status,
        "advertisingChannelType": "DEMAND_GEN",
        "campaignBudget": budget_rn,
        "containsEuPoliticalAdvertising": "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING",
    });
    let obj = campaign.as_object_mut().expect("object");
    match p.bidding_strategy.to_ascii_uppercase().as_str() {
        "MAXIMIZE_CLICKS" | "TARGET_SPEND" => {
            // Google rejects a CPC ceiling on Demand Gen
            // (OPERATION_NOT_PERMITTED_FOR_CONTEXT on targetSpend.cpcBidCeilingMicros).
            if p.max_cpc.is_some() {
                return Err(McpGoogleAdsError::Validation(
                    "Demand Gen does not accept a max CPC ceiling: use MAXIMIZE_CLICKS without max_cpc \
                     and control spend with the daily budget, or bid on conversions (TARGET_CPA)."
                        .into(),
                ));
            }
            obj.insert("targetSpend".into(), json!({}));
        }
        "MAXIMIZE_CONVERSIONS" => {
            let mut mc = serde_json::Map::new();
            if let Some(v) = p.target_cpa {
                mc.insert("targetCpaMicros".into(), json!(micros(v).to_string()));
            }
            obj.insert("maximizeConversions".into(), serde_json::Value::Object(mc));
        }
        "TARGET_CPA" => {
            let v = p.target_cpa.ok_or_else(|| {
                McpGoogleAdsError::Validation("TARGET_CPA needs target_cpa".into())
            })?;
            obj.insert(
                "maximizeConversions".into(),
                json!({ "targetCpaMicros": micros(v).to_string() }),
            );
        }
        "MAXIMIZE_CONVERSION_VALUE" | "TARGET_ROAS" => {
            let mut mv = serde_json::Map::new();
            if let Some(r) = p.target_roas {
                mv.insert("targetRoas".into(), json!(r));
            }
            obj.insert("maximizeConversionValue".into(), serde_json::Value::Object(mv));
        }
        other => {
            return Err(McpGoogleAdsError::Validation(format!(
                "Unsupported Demand Gen bidding_strategy '{}'. Use MAXIMIZE_CLICKS, \
                 MAXIMIZE_CONVERSIONS, TARGET_CPA, MAXIMIZE_CONVERSION_VALUE or TARGET_ROAS.",
                other
            )))
        }
    }

    let operations = vec![
        json!({ "campaignBudgetOperation": { "create": {
            "resourceName": budget_rn,
            "name": format!("{} Budget", p.campaign_name),
            "amountMicros": micros(p.daily_budget).to_string(),
            "deliveryMethod": "STANDARD",
            "explicitlyShared": false
        }}}),
        json!({ "campaignOperation": { "create": campaign } }),
    ];

    let changes = json!({
        "campaign_name": p.campaign_name,
        "channel_type": "DEMAND_GEN",
        "daily_budget": p.daily_budget,
        "bidding_strategy": p.bidding_strategy,
        "max_cpc": p.max_cpc,
        "target_cpa": p.target_cpa,
        "target_roas": p.target_roas,
        "status": status,
        "next_steps": "Read the campaign id from responses[1], then create_demand_gen_ad_group and create_demand_gen_video_ad."
    });
    let plan = ChangePlan::new(
        "create_demand_gen_campaign".into(),
        "campaign".into(),
        "new".into(),
        cid,
        changes,
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

// ── 2. Ad group ──────────────────────────────────────────────────────────

/// Which Demand Gen channels the ad group serves on.
#[derive(Debug, Clone, Default)]
pub struct DemandGenChannels {
    pub youtube_in_stream: bool,
    pub youtube_in_feed: bool,
    pub youtube_shorts: bool,
    pub discover: bool,
    pub gmail: bool,
    pub display: bool,
    pub maps: bool,
}

impl DemandGenChannels {
    fn any(&self) -> bool {
        self.youtube_in_stream
            || self.youtube_in_feed
            || self.youtube_shorts
            || self.discover
            || self.gmail
            || self.display
            || self.maps
    }
}

pub struct CreateDemandGenAdGroupParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    pub ad_group_name: &'a str,
    pub channels: DemandGenChannels,
    pub optimized_targeting: bool,
    pub geo_target_ids: Vec<String>,
    pub language_ids: Vec<String>,
    /// Audience ids (customers/{cid}/audiences/{id}) to target.
    pub audience_ids: Vec<String>,
    pub start_paused: bool,
}

pub fn create_demand_gen_ad_group(p: &CreateDemandGenAdGroupParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_demand_gen_ad_group", &p.config.safety)?;
    require_numeric("campaign_id", p.campaign_id)?;
    if !p.channels.any() {
        return Err(McpGoogleAdsError::Validation(
            "Select at least one channel (youtube_in_stream, youtube_in_feed, youtube_shorts, \
             discover, gmail, display, maps)"
                .into(),
        ));
    }
    if p.geo_target_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "geo_target_ids is required: an untargeted Demand Gen ad group serves worldwide".into(),
        ));
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

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let ad_group_rn = format!("customers/{}/adGroups/-1", cid);
    let status = if p.start_paused { "PAUSED" } else { "ENABLED" };
    let c = &p.channels;

    let mut operations = vec![json!({ "adGroupOperation": { "create": {
        "resourceName": ad_group_rn,
        "name": p.ad_group_name,
        "campaign": format!("customers/{}/campaigns/{}", cid, p.campaign_id),
        "status": status,
        "optimizedTargetingEnabled": p.optimized_targeting,
        "demandGenAdGroupSettings": { "channelControls": { "selectedChannels": {
            "youtubeInStream": c.youtube_in_stream,
            "youtubeInFeed": c.youtube_in_feed,
            "youtubeShorts": c.youtube_shorts,
            "discover": c.discover,
            "gmail": c.gmail,
            "display": c.display,
            "maps": c.maps
        }}}
    }}})];

    for g in &p.geo_target_ids {
        operations.push(json!({ "adGroupCriterionOperation": { "create": {
            "adGroup": ad_group_rn,
            "location": { "geoTargetConstant": format!("geoTargetConstants/{}", g) }
        }}}));
    }
    for l in &p.language_ids {
        operations.push(json!({ "adGroupCriterionOperation": { "create": {
            "adGroup": ad_group_rn,
            "language": { "languageConstant": format!("languageConstants/{}", l) }
        }}}));
    }
    for a in &p.audience_ids {
        operations.push(json!({ "adGroupCriterionOperation": { "create": {
            "adGroup": ad_group_rn,
            "audience": { "audience": format!("customers/{}/audiences/{}", cid, a) }
        }}}));
    }

    let changes = json!({
        "campaign_id": p.campaign_id,
        "ad_group_name": p.ad_group_name,
        "status": status,
        "optimized_targeting": p.optimized_targeting,
        "channels": {
            "youtube_in_stream": c.youtube_in_stream,
            "youtube_in_feed": c.youtube_in_feed,
            "youtube_shorts": c.youtube_shorts,
            "discover": c.discover,
            "gmail": c.gmail,
            "display": c.display,
            "maps": c.maps
        },
        "geo_target_ids": p.geo_target_ids,
        "language_ids": p.language_ids,
        "audience_ids": p.audience_ids,
    });
    let plan = ChangePlan::new(
        "create_demand_gen_ad_group".into(),
        "ad_group".into(),
        "new".into(),
        cid,
        changes,
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

// ── 3. Custom segment (CustomAudienceService) ───────────────────────────

pub struct CreateCustomAudienceParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    /// AUTO or SEARCH. SEARCH = people who searched these terms on Google
    /// (keywords only). INTEREST / PURCHASE_INTENT can no longer be created.
    pub audience_type: &'a str,
    pub keywords: Vec<String>,
    /// Domains or URLs: people who browse sites like these (AUTO only).
    pub urls: Vec<String>,
}

pub fn create_custom_audience(p: &CreateCustomAudienceParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_custom_audience", &p.config.safety)?;
    let kind = p.audience_type.to_ascii_uppercase();
    if kind != "AUTO" && kind != "SEARCH" {
        return Err(McpGoogleAdsError::Validation(format!(
            "audience_type must be AUTO or SEARCH (got '{}')",
            p.audience_type
        )));
    }
    if kind == "SEARCH" && !p.urls.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "A SEARCH custom segment only takes keywords. Put URLs in a separate AUTO segment."
                .into(),
        ));
    }
    if p.keywords.is_empty() && p.urls.is_empty() {
        return Err(McpGoogleAdsError::Validation("Give keywords and/or urls".into()));
    }
    if p.name.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation("name is required".into()));
    }

    let mut members = Vec::new();
    for k in &p.keywords {
        let k = k.trim();
        if k.is_empty() || k.chars().count() > 80 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Keyword '{}' must be 1-80 characters",
                k
            )));
        }
        members.push(json!({ "memberType": "KEYWORD", "keyword": k }));
    }
    for u in &p.urls {
        let u = u.trim();
        if u.is_empty() || u.contains(' ') {
            return Err(McpGoogleAdsError::Validation(format!("Invalid url '{}'", u)));
        }
        members.push(json!({ "memberType": "URL", "url": u }));
    }

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let mut create = json!({ "name": p.name, "type": kind, "members": members });
    if let Some(d) = p.description {
        create["description"] = json!(d);
    }
    let operation = json!({ "create": create });

    let changes = json!({
        "name": p.name,
        "type": kind,
        "keywords": p.keywords,
        "urls": p.urls,
        "next_steps": "Use the returned customAudiences/{id} in create_demand_gen_audience."
    });
    let mut plan = ChangePlan::new(
        "create_custom_audience".into(),
        "custom_audience".into(),
        "new".into(),
        cid,
        changes,
        false,
        Vec::new(),
    );
    plan.dispatch = PlanDispatch::CustomAudienceMutate {
        operations: vec![operation],
    };
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

// ── 4. Audience (custom segments + optional age) ─────────────────────────

pub struct CreateDemandGenAudienceParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub name: &'a str,
    pub custom_audience_ids: Vec<String>,
    /// Age ranges to KEEP, e.g. [(25, Some(34)), (35, Some(44)), (65, None)].
    /// Empty = all ages.
    pub age_ranges: Vec<(i32, Option<i32>)>,
    pub include_undetermined_age: bool,
}

pub fn create_demand_gen_audience(p: &CreateDemandGenAudienceParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_demand_gen_audience", &p.config.safety)?;
    if p.custom_audience_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "custom_audience_ids is required".into(),
        ));
    }
    for id in &p.custom_audience_ids {
        require_numeric("custom_audience_id", id)?;
    }
    const MINS: [i32; 6] = [18, 25, 35, 45, 55, 65];
    const MAXS: [i32; 5] = [24, 34, 44, 54, 64];
    for (min, max) in &p.age_ranges {
        if !MINS.contains(min) {
            return Err(McpGoogleAdsError::Validation(format!(
                "min_age {} not allowed (18, 25, 35, 45, 55, 65)",
                min
            )));
        }
        if let Some(m) = max {
            if !MAXS.contains(m) || m <= min {
                return Err(McpGoogleAdsError::Validation(format!(
                    "max_age {} not allowed for min_age {} (24, 34, 44, 54, 64)",
                    m, min
                )));
            }
        }
    }

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let segments: Vec<serde_json::Value> = p
        .custom_audience_ids
        .iter()
        .map(|id| {
            json!({ "customAudience": {
                "customAudience": format!("customers/{}/customAudiences/{}", cid, id)
            }})
        })
        .collect();

    let mut dimensions = vec![json!({ "audienceSegments": { "segments": segments } })];
    if !p.age_ranges.is_empty() {
        let ranges: Vec<serde_json::Value> = p
            .age_ranges
            .iter()
            .map(|(min, max)| match max {
                Some(m) => json!({ "minAge": min, "maxAge": m }),
                None => json!({ "minAge": min }),
            })
            .collect();
        dimensions.push(json!({ "age": {
            "ageRanges": ranges,
            "includeUndetermined": p.include_undetermined_age
        }}));
    }

    let operation = json!({ "audienceOperation": { "create": {
        "name": p.name,
        "dimensions": dimensions
    }}});
    let changes = json!({
        "name": p.name,
        "custom_audience_ids": p.custom_audience_ids,
        "age_ranges": p.age_ranges.iter().map(|(a, b)| json!({"min": a, "max": b})).collect::<Vec<_>>(),
        "include_undetermined_age": p.include_undetermined_age,
        "next_steps": "Pass the returned audience id to create_demand_gen_ad_group (audience_ids)."
    });
    let plan = ChangePlan::new(
        "create_demand_gen_audience".into(),
        "audience".into(),
        "new".into(),
        cid,
        changes,
        false,
        vec![operation],
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

// ── 5. Video responsive ad ───────────────────────────────────────────────

pub struct CreateDemandGenVideoAdParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub ad_group_id: &'a str,
    pub ad_name: &'a str,
    /// YouTube ids or URLs (watch, shorts, youtu.be). 1-5.
    pub videos: Vec<String>,
    /// Existing IMAGE asset id for the square logo (1:1).
    pub logo_asset_id: &'a str,
    pub business_name: &'a str,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub final_url: &'a str,
    pub breadcrumb1: Option<&'a str>,
    pub breadcrumb2: Option<&'a str>,
    pub start_paused: bool,
}

pub fn create_demand_gen_video_ad(p: &CreateDemandGenVideoAdParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_demand_gen_video_ad", &p.config.safety)?;
    require_numeric("ad_group_id", p.ad_group_id)?;
    require_numeric("logo_asset_id", p.logo_asset_id)?;
    if p.videos.is_empty() || p.videos.len() > 5 {
        return Err(McpGoogleAdsError::Validation(format!(
            "A Demand Gen video ad takes 1-5 videos (got {})",
            p.videos.len()
        )));
    }
    check_len("Headline", &p.headlines, 1, 5, 40)?;
    check_len("Long headline", &p.long_headlines, 1, 5, 90)?;
    check_len("Description", &p.descriptions, 1, 5, 90)?;
    if p.business_name.trim().is_empty() || p.business_name.chars().count() > 25 {
        return Err(McpGoogleAdsError::Validation(
            "business_name must be 1-25 characters".into(),
        ));
    }
    if !(p.final_url.starts_with("https://") || p.final_url.starts_with("http://")) {
        return Err(McpGoogleAdsError::Validation("final_url must be an http(s) URL".into()));
    }
    for b in [p.breadcrumb1, p.breadcrumb2].into_iter().flatten() {
        if b.chars().count() > 15 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Breadcrumb '{}' exceeds 15 characters",
                b
            )));
        }
    }

    let mut video_ids = Vec::new();
    for v in &p.videos {
        let id = parse_youtube_id(v)?;
        if !video_ids.contains(&id) {
            video_ids.push(id);
        }
    }

    let cid = GoogleAdsClient::normalize_customer_id(p.customer_id);
    let mut operations = Vec::new();
    let mut video_links = Vec::new();
    for (i, id) in video_ids.iter().enumerate() {
        let rn = format!("customers/{}/assets/-{}", cid, 10 + i);
        operations.push(json!({ "assetOperation": { "create": {
            "resourceName": rn,
            "name": format!("YouTube {}", id),
            "type": "YOUTUBE_VIDEO",
            "youtubeVideoAsset": { "youtubeVideoId": id }
        }}}));
        video_links.push(json!({ "asset": rn }));
    }

    let texts = |items: &[String]| -> Vec<serde_json::Value> {
        items.iter().map(|t| json!({ "text": t })).collect()
    };
    let mut ad_info = json!({
        "headlines": texts(&p.headlines),
        "longHeadlines": texts(&p.long_headlines),
        "descriptions": texts(&p.descriptions),
        "videos": video_links,
        "logoImages": [{ "asset": format!("customers/{}/assets/{}", cid, p.logo_asset_id) }],
        "businessName": { "text": p.business_name }
    });
    if let Some(b) = p.breadcrumb1 {
        ad_info["breadcrumb1"] = json!(b);
    }
    if let Some(b) = p.breadcrumb2 {
        ad_info["breadcrumb2"] = json!(b);
    }

    let status = if p.start_paused { "PAUSED" } else { "ENABLED" };
    operations.push(json!({ "adGroupAdOperation": { "create": {
        "adGroup": format!("customers/{}/adGroups/{}", cid, p.ad_group_id),
        "status": status,
        "ad": {
            "name": p.ad_name,
            "finalUrls": [p.final_url],
            "demandGenVideoResponsiveAd": ad_info
        }
    }}}));

    let changes = json!({
        "ad_group_id": p.ad_group_id,
        "ad_name": p.ad_name,
        "videos": video_ids,
        "logo_asset_id": p.logo_asset_id,
        "business_name": p.business_name,
        "headlines": p.headlines,
        "long_headlines": p.long_headlines,
        "descriptions": p.descriptions,
        "final_url": p.final_url,
        "status": status,
    });
    let plan = ChangePlan::new(
        "create_demand_gen_video_ad".into(),
        "ad".into(),
        "new".into(),
        cid,
        changes,
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

    #[test]
    fn parses_youtube_ids() {
        assert_eq!(parse_youtube_id("https://www.youtube.com/watch?v=m1IsWylkons").unwrap(), "m1IsWylkons");
        assert_eq!(parse_youtube_id("https://www.youtube.com/shorts/1ryKDjfYIC8").unwrap(), "1ryKDjfYIC8");
        assert_eq!(parse_youtube_id("https://youtu.be/YtO5GjXzGWM?t=3").unwrap(), "YtO5GjXzGWM");
        assert_eq!(parse_youtube_id("gWQQYe2CVGE").unwrap(), "gWQQYe2CVGE");
        assert!(parse_youtube_id("https://example.com/x").is_err());
    }

    #[test]
    fn ad_group_requires_channel_and_geo() {
        let cfg = Config::default();
        let base = CreateDemandGenAdGroupParams {
            config: &cfg,
            customer_id: "123-456-7890",
            campaign_id: "1",
            ad_group_name: "x",
            channels: DemandGenChannels::default(),
            optimized_targeting: false,
            geo_target_ids: vec!["2840".into()],
            language_ids: vec![],
            audience_ids: vec![],
            start_paused: true,
        };
        assert!(create_demand_gen_ad_group(&base).is_err());
        let ok = CreateDemandGenAdGroupParams {
            channels: DemandGenChannels { youtube_in_feed: true, ..Default::default() },
            ..base
        };
        let preview = create_demand_gen_ad_group(&ok).unwrap();
        assert_eq!(preview["changes"]["optimized_targeting"], false);
    }
}
