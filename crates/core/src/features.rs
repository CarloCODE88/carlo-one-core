//! Feature-Flags für Open-Core-Modell
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    // OPEN SOURCE
    BasicCodeGeneration,
    BasicDesignVariants,
    LocalModelSupport,
    // PREMIUM
    CanvasAI,
    AdvancedModels,
    CloudSync,
    PriorityQueue,
    MultiUser,
}

pub fn is_available(feature: Feature) -> bool {
    match feature {
        Feature::BasicCodeGeneration => true,
        Feature::BasicDesignVariants => true,
        Feature::LocalModelSupport => true,
        Feature::CanvasAI => has_premium_license(),
        Feature::AdvancedModels => has_premium_license(),
        Feature::CloudSync => has_premium_license(),
        Feature::PriorityQueue => has_premium_license(),
        Feature::MultiUser => has_premium_license(),
    }
}

fn has_premium_license() -> bool {
    std::env::var("CARLO_PREMIUM_LICENSE").is_ok()
}
