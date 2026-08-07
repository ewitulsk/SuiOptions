use crate::order::Order;
use crate::types::{ObjectId, TypeTagStr};
use serde::{Deserialize, Serialize};

/// A market is identified by its on-chain `SettlementRegistry` object ID.
pub type MarketId = ObjectId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Maker sells Quote for Base (an order to buy Base).
    Bid,
    /// Maker sells Base for Quote (an order to sell Base).
    Ask,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Off-chain view of one trading pair, mirroring the on-chain
/// `SettlementRegistry<Base, Quote>` config.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    /// Human symbol, e.g. "SUI/USDC".
    pub symbol: String,
    pub registry_id: MarketId,
    /// Canonical base coin type string.
    pub base: TypeTagStr,
    /// Canonical quote coin type string.
    pub quote: TypeTagStr,
    /// Price grid in quote units per `lot_size` base units.
    pub tick_size: u64,
    /// Minimum order size in base units.
    pub min_size: u64,
    /// Base units per price tick denomination.
    pub lot_size: u64,
    /// Current default fee in bps (mirrored from chain; per-fill actual is
    /// `min(order.max_fee_bps, this-or-account-tier)`).
    pub current_fee_bps: u64,
}

impl Market {
    /// Which side of this market an order is on, or `None` if its token pair
    /// doesn't match the market. Token strings must already be canonical.
    pub fn side_of(&self, order: &Order) -> Option<Side> {
        if order.maker_token == self.base && order.taker_token == self.quote {
            Some(Side::Ask)
        } else if order.maker_token == self.quote && order.taker_token == self.base {
            Some(Side::Bid)
        } else {
            None
        }
    }
}
