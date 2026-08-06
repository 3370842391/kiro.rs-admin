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
/// Kiro Drop 的购买接口接受 `region`（us / eu / us-east-1 / eu-central-1），
/// webhook 也带区域，且缺货时客户端会自动改打另一个区，所以三种模式都支持。
/// 原先标成 `OMIT_ONLY`——那时既不传 region 也不读响应里的 region，
/// 买到的欧区号一路按美区落库。
const DROP_REGION_MODES: &[PurchaseRegionMode] = &[
    PurchaseRegionMode::Fixed,
    PurchaseRegionMode::Webhook,
    PurchaseRegionMode::BestAvailable,
];

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
                region_modes: DROP_REGION_MODES,
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

        for kind in [SupplierKind::KiroRs, SupplierKind::KiroApp] {
            let capabilities = SupplierCapabilities::for_kind(kind);
            assert_eq!(capabilities.region_modes, &[PurchaseRegionMode::Omit]);
        }

        // Kiro Drop 的购买接口接受 region、webhook 带区域、缺货还会自动换区，
        // 所以它和 kiro.ceo 一样支持三种模式（原先被标成仅 Omit）。
        let drop = SupplierCapabilities::for_kind(SupplierKind::KiroDrop);
        assert_eq!(
            drop.region_modes,
            &[
                PurchaseRegionMode::Fixed,
                PurchaseRegionMode::Webhook,
                PurchaseRegionMode::BestAvailable,
            ]
        );
        assert!(!drop.supports_region_mode(PurchaseRegionMode::Omit));
        assert!(!drop.supports_region_mode(PurchaseRegionMode::Batch));
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
