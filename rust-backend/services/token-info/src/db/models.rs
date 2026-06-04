//! Diesel row types for the catalog + conversion to the wire DTO.

use chrono::{DateTime, Utc};
use diesel::prelude::*;

use token_info_client::SupportedToken;

use super::schema::supported_tokens;

/// One catalog row as read from Postgres.
#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = supported_tokens)]
#[diesel(primary_key(coin_type))]
pub struct TokenRow {
    pub coin_type: String,
    pub ticker: String,
    pub name: String,
    pub logo_uri: Option<String>,
    pub decimals: i16,
    pub pyth_feed_id: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TokenRow {
    /// Convert to the public wire DTO served by `GET /tokens`.
    pub fn into_dto(self) -> SupportedToken {
        SupportedToken {
            coin_type: self.coin_type,
            ticker: self.ticker,
            name: self.name,
            logo_uri: self.logo_uri,
            decimals: self.decimals.max(0) as u8,
            pyth_feed_id: self.pyth_feed_id,
            enabled: self.enabled,
        }
    }
}

/// Insert/upsert payload. `created_at` / `updated_at` use DB defaults on
/// insert; the repo bumps `updated_at` explicitly on update.
#[derive(Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = supported_tokens)]
pub struct UpsertToken {
    pub coin_type: String,
    pub ticker: String,
    pub name: String,
    pub logo_uri: Option<String>,
    pub decimals: i16,
    pub pyth_feed_id: Option<String>,
    pub enabled: bool,
    pub source: String,
}
