//! POST /v1/brand — extract brand identity (colors, fonts, logo) from a page.
//!
//! Pure DOM/CSS analysis — no LLM, no network beyond the page fetch itself.

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::{Value, json};
use webclaw_core::brand::extract_brand;

use crate::{error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct BrandRequest {
    pub url: String,
}

pub async fn brand(
    State(state): State<AppState>,
    Json(req): Json<BrandRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.url.trim().is_empty() {
        return Err(ApiError::bad_request("`url` is required"));
    }

    let fetched = state.fetch().fetch(&req.url).await?;
    let brand = extract_brand(&fetched.html, Some(&fetched.url));

    Ok(Json(json!({
        "url": req.url,
        "brand": brand,
    })))
}
