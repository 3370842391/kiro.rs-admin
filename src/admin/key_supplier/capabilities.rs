use serde::{Deserialize, Serialize};

use crate::model::config::{PurchaseRegionMode, SupplierKind};

const OMIT_ONLY: &[PurchaseRegionMode] = &[PurchaseRegionMode::Omit];
const CEO_REGION_MODES: &[PurchaseRegionMode] = &[
    PurchaseRegionMode::Fixed,
    PurchaseRegionMode::Webhook,
    PurchaseRegionMode::BestAvailable,
];
const IO_REGION_MODES: &[PurchaseRegionMode] =
    &[PurchaseRegionMode::Fixed, PurchaseRegionMode::Batch];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SupplierCapabilities {
    pub region_modes: &'static [PurchaseRegionMode],
    pub supports_webhook_registration: bool,
    pub purchase_is_idempotent: bool,
    pub supports_price: bool,
}

impl SupplierCapabilities {
    pub const fn for_kind(kind: SupplierKind) -> Self {
        match kind {
            SupplierKind::KiroCeo => Self {
                region_modes: CEO_REGION_MODES,
                supports_webhook_registration: true,
                purchase_is_idempotent: true,
                supports_price: true,
            },
            SupplierKind::KiroAppIo => Self {
                region_modes: IO_REGION_MODES,
                supports_webhook_registration: false,
                purchase_is_idempotent: true,
                supports_price: true,
            },
            SupplierKind::KiroApp => Self {
                region_modes: OMIT_ONLY,
                supports_webhook_registration: false,
                purchase_is_idempotent: false,
                supports_price: true,
            },
            SupplierKind::KiroDrop => Self {
                region_modes: OMIT_ONLY,
                supports_webhook_registration: true,
                purchase_is_idempotent: true,
                supports_price: true,
            },
            SupplierKind::KiroRs => Self {
                region_modes: OMIT_ONLY,
                supports_webhook_registration: true,
                purchase_is_idempotent: true,
                supports_price: false,
            },
        }
    }

    pub fn supports_region_mode(self, mode: PurchaseRegionMode) -> bool {
        self.region_modes.contains(&mode)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RegionSource {
    PurchaseResponse,
    Webhook,
    Request,
    ConfigFallback,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{PurchaseRegionMode, SupplierKind, SupplierRegion};

    #[test]
    fn supplier_capability_matrix_matches_protocol_contracts() {
        let ceo = SupplierCapabilities::for_kind(SupplierKind::KiroCeo);
        assert!(ceo.supports_region_mode(PurchaseRegionMode::Fixed));
        assert!(ceo.supports_region_mode(PurchaseRegionMode::Webhook));
        assert!(ceo.supports_region_mode(PurchaseRegionMode::BestAvailable));
        assert!(!ceo.supports_region_mode(PurchaseRegionMode::Omit));

        let io = SupplierCapabilities::for_kind(SupplierKind::KiroAppIo);
        assert!(io.supports_region_mode(PurchaseRegionMode::Fixed));
        assert!(io.supports_region_mode(PurchaseRegionMode::Batch));
        assert!(!io.supports_region_mode(PurchaseRegionMode::BestAvailable));

        for kind in [
            SupplierKind::KiroRs,
            SupplierKind::KiroApp,
            SupplierKind::KiroDrop,
        ] {
            let capabilities = SupplierCapabilities::for_kind(kind);
            assert_eq!(capabilities.region_modes, &[PurchaseRegionMode::Omit]);
        }
    }

    #[test]
    fn normalized_regions_convert_strictly_to_wire_and_kiro_values() {
        assert_eq!(SupplierRegion::Us.as_wire(), "us");
        assert_eq!(SupplierRegion::Us.as_api_region(), "us-east-1");
        assert_eq!(SupplierRegion::Eu.as_wire(), "eu");
        assert_eq!(SupplierRegion::Eu.as_api_region(), "eu-central-1");
        assert_eq!(
            "us-east-1".parse::<SupplierRegion>().unwrap(),
            SupplierRegion::Us
        );
        assert_eq!("eu".parse::<SupplierRegion>().unwrap(), SupplierRegion::Eu);
        assert!("ap-southeast-1".parse::<SupplierRegion>().is_err());
    }
}
