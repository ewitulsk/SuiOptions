//! Which side of the book the RFQ flow is on.
//!
//! `Writer`: retail is writing the call; the MM counterparty is a Trader MM
//! (buys the call from retail).
//! `Trader`: retail is buying the call; the MM counterparty is a Writer MM
//! (writes the call to retail).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Writer,
    Trader,
}

impl Side {
    /// The MM role on the opposite side of a retail RFQ.
    pub fn counterparty_mm(self) -> MmRole {
        match self {
            Side::Writer => MmRole::TraderMm,
            Side::Trader => MmRole::WriterMm,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MmRole {
    /// Buys options from retail writers (bid-side).
    TraderMm,
    /// Writes options to retail traders (ask-side).
    WriterMm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetailRole {
    Writer,
    Trader,
}

impl RetailRole {
    pub fn side(self) -> Side {
        match self {
            RetailRole::Writer => Side::Writer,
            RetailRole::Trader => Side::Trader,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&Side::Writer).unwrap(), "\"writer\"");
        assert_eq!(serde_json::to_string(&Side::Trader).unwrap(), "\"trader\"");
    }

    #[test]
    fn counterparty_pairing() {
        assert_eq!(Side::Writer.counterparty_mm(), MmRole::TraderMm);
        assert_eq!(Side::Trader.counterparty_mm(), MmRole::WriterMm);
    }
}
