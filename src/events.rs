//! pump.fun `TradeEvent` decoding.
//!
//! This is what makes smart-money tracking possible at all. The account feed in
//! [`crate::feed`] tells you a curve moved; it does not tell you *who* moved it.
//! The trade event does, and it arrives in the same logs.
//!
//! ## Provenance
//!
//! The layout below was not taken from an IDL. It was located by searching real
//! mainnet event bytes for values already known from the transaction's own balance
//! deltas — the token amount, the mint, the fee payer's pubkey — and checking
//! where they landed. The offsets were identical across every transaction, and the
//! full layout was then validated on **60 of 60** captured events: `is_buy` is
//! always 0 or 1, timestamps are plausible, and reserves stay inside the curve's
//! real bounds.
//!
//! It also independently reproduces the fee model: a 3.440000000 SOL gross order
//! decodes to `sol_amount = 3.397531` here, matching [`crate::pumpfun::FEE_BPS`].
//!
//! Event length **varies** (a Borsh string sits at the end), so only the fixed
//! leading region is parsed and everything after offset 129 is ignored.

use anyhow::{bail, Result};
use solana_sdk::pubkey::Pubkey;

/// Anchor discriminator for the pump.fun `TradeEvent`.
pub const TRADE_EVENT_DISC: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];

/// Prefix Anchor puts on a self-CPI event instruction, before the real event
/// discriminator. Log-emitted events do not carry it; CPI-emitted ones do.
pub const CPI_EVENT_PREFIX: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

/// Bytes of fixed-layout fields we rely on.
const FIXED_LEN: usize = 129;

/// One decoded buy or sell on a pump.fun bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeEvent {
    pub mint: Pubkey,
    /// Lamports into (buy) or out of (sell) the curve, net of fees.
    pub sol_amount: u64,
    /// Base units of the token.
    pub token_amount: u64,
    pub is_buy: bool,
    /// The trader. This is the whole point of decoding the event.
    pub user: Pubkey,
    pub timestamp: i64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
}

impl TradeEvent {
    /// Decode from the bytes of a `Program data:` log line, or from a self-CPI
    /// instruction (the 8-byte Anchor prefix is stripped automatically).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = if bytes.len() >= 8 && bytes[..8] == CPI_EVENT_PREFIX {
            &bytes[8..]
        } else {
            bytes
        };
        if body.len() < FIXED_LEN {
            bail!("trade event too short: {} bytes", body.len());
        }
        if body[..8] != TRADE_EVENT_DISC {
            bail!("not a TradeEvent: disc {}", hex8(&body[..8]));
        }
        let u = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().unwrap());
        let is_buy = match body[56] {
            0 => false,
            1 => true,
            // A byte outside {0,1} means the layout drifted. Fail loudly rather
            // than silently mislabelling every trade as a sell.
            other => bail!("is_buy was {other}, layout has changed"),
        };
        Ok(Self {
            mint: Pubkey::new_from_array(body[8..40].try_into().unwrap()),
            sol_amount: u(40),
            token_amount: u(48),
            is_buy,
            user: Pubkey::new_from_array(body[57..89].try_into().unwrap()),
            timestamp: i64::from_le_bytes(body[89..97].try_into().unwrap()),
            virtual_sol_reserves: u(97),
            virtual_token_reserves: u(105),
            real_sol_reserves: u(113),
            real_token_reserves: u(121),
        })
    }

    /// Cheap plausibility check, using facts the protocol guarantees.
    ///
    /// Used to catch a layout drift after a pump.fun upgrade before it can poison
    /// the reputation store with garbage pubkeys.
    pub fn looks_sane(&self) -> bool {
        const MIN_TS: i64 = 1_600_000_000; // 2020
        const MAX_TS: i64 = 4_000_000_000; // 2096
        self.timestamp > MIN_TS
            && self.timestamp < MAX_TS
            && self.virtual_sol_reserves >= crate::pumpfun::INIT_VIRTUAL_SOL as u64
            && self.real_sol_reserves <= crate::pumpfun::graduation_lamports() + 1_000_000_000
            && self.token_amount > 0
    }
}

fn hex8(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Pull every decodable trade event out of a transaction's log lines.
pub fn from_logs(logs: &[String]) -> Vec<TradeEvent> {
    use base64::Engine;
    logs.iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .filter_map(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .filter_map(|bytes| TradeEvent::decode(&bytes).ok())
        .filter(TradeEvent::looks_sane)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// A real buy captured from mainnet: signature
    /// 3fDRifgehh72Ch4cBJwM6MPNL1sfWTRcSaML1TAZbVdXGJMkg49YMcE4pFKbZw7oDkw8HBzCWJtnicuMyTsRAiMJ
    const REAL_BUY: &str = "vdt/007mYe5J2xeifXbK0kNll1XTqPoN8jrpkwAmwkM3PgUyWcRhbxqnb4MCAAAAeMDf1l4aAAABC7ZE1aPAWzIDYZftO2ZYw4Gw47aSGHk/Ual4u/WBSSNfKZRqAAAAAAGE2sYaAAAAAJgSTJH+AAAB2LbKEwAAAAAAAAAAAAAAYIzMHfzpYbQ7d5wZFQWm4tO/RdWk20YYrXbILWF1RTVfAAAAAAAAADPWHAYAAAAAUcftLOqIHumUWDRh11Qf7cmlN54d3cmRGthICSnK+XQeAAAAAAAAALIo7gEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwAAAGJ1eQAAAAAAAAAAAAAAAAAAAAAAiBMAAAAAAAAZaw4DAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABqnb4MCAAAAAYTaxhoAAAAB2LbKEwAAAA==";

    fn real() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(REAL_BUY)
            .unwrap()
    }

    #[test]
    fn decodes_a_real_mainnet_buy() {
        let e = TradeEvent::decode(&real()).expect("should decode");
        assert!(e.is_buy, "this capture is a buy");
        assert!(e.looks_sane(), "{e:?}");
        // Reserves must be internally consistent with the curve invariant.
        assert!(e.virtual_sol_reserves > crate::pumpfun::INIT_VIRTUAL_SOL as u64);
        assert_eq!(
            e.virtual_sol_reserves - crate::pumpfun::INIT_VIRTUAL_SOL as u64,
            e.real_sol_reserves,
            "virtual_sol - 30 SOL must equal real_sol"
        );
        assert!(e.token_amount > 0);
        assert!(e.timestamp > 1_700_000_000);
    }

    #[test]
    fn the_real_capture_is_a_variable_length_event() {
        // 358 bytes, not a fixed 371 - a Borsh string trails the fixed fields, so
        // the decoder must never assume a total length.
        assert_eq!(real().len(), 358);
        assert!(real().len() > FIXED_LEN);
    }

    #[test]
    fn cpi_prefixed_events_decode_identically() {
        let mut wrapped = CPI_EVENT_PREFIX.to_vec();
        wrapped.extend_from_slice(&real());
        assert_eq!(
            TradeEvent::decode(&wrapped).unwrap(),
            TradeEvent::decode(&real()).unwrap()
        );
    }

    #[test]
    fn truncated_events_are_rejected() {
        let r = real();
        assert!(TradeEvent::decode(&r[..FIXED_LEN - 1]).is_err());
        assert!(TradeEvent::decode(&[]).is_err());
    }

    #[test]
    fn a_different_discriminator_is_rejected() {
        let mut r = real();
        r[0] ^= 0xff;
        assert!(TradeEvent::decode(&r).is_err());
    }

    #[test]
    fn a_drifted_is_buy_byte_fails_loudly() {
        // If pump.fun moves a field, is_buy stops being a bool. That must be an
        // error, not a silent "sell".
        let mut r = real();
        r[56] = 7;
        let err = TradeEvent::decode(&r).unwrap_err().to_string();
        assert!(err.contains("layout has changed"), "{err}");
    }

    #[test]
    fn implausible_events_fail_the_sanity_check() {
        let mut e = TradeEvent::decode(&real()).unwrap();
        assert!(e.looks_sane());
        e.timestamp = 1;
        assert!(!e.looks_sane());

        let mut e = TradeEvent::decode(&real()).unwrap();
        e.real_sol_reserves = u64::MAX;
        assert!(!e.looks_sane());

        let mut e = TradeEvent::decode(&real()).unwrap();
        e.virtual_sol_reserves = 0;
        assert!(!e.looks_sane());
    }

    #[test]
    fn from_logs_extracts_only_real_events() {
        let logs = vec![
            "Program log: Instruction: BuyExactSolIn".to_string(),
            format!("Program data: {REAL_BUY}"),
            "Program data: bm90LWFuLWV2ZW50".to_string(),
            "Program log: something else".to_string(),
        ];
        let found = from_logs(&logs);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_buy);
    }

    #[test]
    fn from_logs_on_empty_input_is_empty() {
        assert!(from_logs(&[]).is_empty());
    }
}
