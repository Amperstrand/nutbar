//! invoice.rs — local BOLT11 metadata extraction. No mint interaction:
//! parsing happens before any money moves so the UI can preview safely.

use anyhow::{bail, Context, Result};
use lightning_invoice::Bolt11Invoice;
use std::str::FromStr;

pub struct InvoiceMeta {
    pub amount_sats: u64,
    pub description: String,
    pub expiry_unix: u64,
    pub expired: bool,
}

pub fn invoice_metadata(invoice: &str) -> Result<InvoiceMeta> {
    let inv = Bolt11Invoice::from_str(invoice.trim()).context("invalid BOLT11 invoice")?;
    let amount_sats = inv
        .amount_milli_satoshis()
        .map(|msat| msat / 1000)
        .context("amountless invoices are not supported yet")?;
    if amount_sats == 0 {
        bail!("invoice carries a zero amount");
    }
    let expiry_unix = inv
        .expires_at()
        .map(|t| t.as_secs() as u64)
        .unwrap_or_default();
    let expired = expiry_unix != 0
        && std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() > expiry_unix)
            .unwrap_or(false);
    Ok(InvoiceMeta {
        amount_sats,
        description: inv.description().to_string(),
        expiry_unix,
        expired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real testnut-generated fixture: 21 sats, "unit test fixture".
    const VALID: &str = "lnbc210n1p4tdc77dquw4hxjapqw3jhxapqve5hsar4wfjspp5zl9cdvju26cpa7qnajs5c9h0v28c0hwmlk9h06rz62drkyw3wp9ssp59g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4q9qrsgqcqzysas254rusw63frxaeu56qn0awe82fvwl5cvavylhpwz2vwty7vu5jnr0qnqt4hn44563puwghe84auyq3a7y2409ecghqrzy94a95k9spsxy3l6";

    #[test]
    fn parses_amount() {
        let meta = match invoice_metadata(VALID) {
            Ok(m) => m,
            Err(_) => {
                // testnut invoice strings embed timestamps; if this specific
                // fixture ever fails to parse, regenerate rather than loosen.
                panic!("fixture no longer parses — regenerate from /invoice");
            }
        };
        assert_eq!(meta.amount_sats, 21);
        assert_eq!(meta.description, "unit test fixture");
        // expiry naturally drifts on a real-invoice fixture — the expired
        // flag is exercised by the pay_invoice handler's rejection path.
    }

    #[test]
    fn rejects_garbage() {
        assert!(invoice_metadata("lnbc123").is_err());
        assert!(invoice_metadata("").is_err());
        assert!(invoice_metadata("not-an-invoice-at-all").is_err());
    }
}
