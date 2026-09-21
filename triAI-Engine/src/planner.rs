//! Ressourcenplanung für genau einen lokalen Modell-Worker.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    pub vram_free_mb: u64,
    pub ram_free_mb: u64,
    pub ssd_free_mb: u64,
    pub vram_reserve_mb: u64,
    pub ram_reserve_mb: u64,
    pub ssd_reserve_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub model: String,
    pub file_size_mb: u64,
    pub estimated_gpu_mb: u64,
    pub estimated_ram_mb: u64,
    pub layers: u32,
    pub context_tokens: u32,
    pub requested_context_tokens: u32,
}

/// Runtime-Anteile, die nicht aus der GGUF-Dateigröße hervorgehen.  Der
/// Default ist bewusst konservativ und bleibt für bestehende Aufrufer von
/// [`plan`] kompatibel.  Der Worker muss den tatsächlich gewählten Wert an
/// `plan_with_runtime` weiterreichen, bevor er GPU-Parameter startet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeMemoryConfig {
    pub parallel_slots: u32,
    pub kv_bytes_per_token: u64,
    pub compute_overhead_mb: u64,
}

impl RuntimeMemoryConfig {
    pub const F16_KV_BYTES_PER_TOKEN: u64 = 64 * 1024;
    pub const Q8_KV_BYTES_PER_TOKEN: u64 = 32 * 1024;
}

impl Default for RuntimeMemoryConfig {
    fn default() -> Self {
        Self {
            parallel_slots: 1,
            kv_bytes_per_token: Self::F16_KV_BYTES_PER_TOKEN,
            compute_overhead_mb: 256,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    GpuFast,
    RamBalanced,
    CpuSafe,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub plan_id: String,
    pub resource_generation: String,
    pub model: String,
    pub lane: Lane,
    pub gpu_layers: u32,
    pub ctx_size: u32,
    pub estimated_vram_mb: u64,
    pub estimated_ram_mb: u64,
    pub reason: String,
    pub safe: bool,
}

pub fn plan(profile: &ModelProfile, resources: &Resources) -> Plan {
    plan_with_runtime(profile, resources, RuntimeMemoryConfig::default())
}

pub fn plan_with_runtime(
    profile: &ModelProfile,
    resources: &Resources,
    runtime: RuntimeMemoryConfig,
) -> Plan {
    let resources = normalize_resources(resources);
    let resource_generation_hash = resource_generation_from_normalized(&resources);
    let resource_generation = format!("resources-v1-{resource_generation_hash:016x}");
    let usable_vram = resources
        .vram_free_mb
        .saturating_sub(resources.vram_reserve_mb);
    let usable_ram = resources
        .ram_free_mb
        .saturating_sub(resources.ram_reserve_mb);
    let ctx = profile
        .requested_context_tokens
        .min(profile.context_tokens)
        .max(512);
    // KV wächst pro Token, Slot und Dtype.  Die Schätzung ist absichtlich
    // monoton/saturierend: nie eine scheinbar passende GPU durch Overflow.
    let kv_overhead = u64::from(ctx)
        .saturating_mul(u64::from(runtime.parallel_slots.max(1)))
        .saturating_mul(runtime.kv_bytes_per_token)
        .div_ceil(1_048_576);
    let gpu_overhead = kv_overhead.saturating_add(runtime.compute_overhead_mb);
    let ram_need = profile.estimated_ram_mb.saturating_add(kv_overhead);

    if profile.layers == 0 || profile.layers == 999 {
        return identified_plan(
            profile,
            resource_generation_hash,
            Plan {
                plan_id: String::new(),
                resource_generation,
                model: profile.model.clone(),
                lane: Lane::Reject,
                gpu_layers: 0,
                ctx_size: ctx,
                estimated_vram_mb: 0,
                estimated_ram_mb: ram_need,
                reason: "reale GGUF-Layerzahl erforderlich; 999 ist nur ein llama.cpp-Sentinel"
                    .into(),
                safe: false,
            },
        );
    }

    if profile.estimated_gpu_mb.saturating_add(gpu_overhead) <= usable_vram {
        return identified_plan(
            profile,
            resource_generation_hash,
            Plan {
                plan_id: String::new(),
                resource_generation,
                model: profile.model.clone(),
                lane: Lane::GpuFast,
                gpu_layers: profile.layers,
                ctx_size: ctx,
                estimated_vram_mb: profile.estimated_gpu_mb.saturating_add(gpu_overhead),
                estimated_ram_mb: ram_need,
                reason: "Modell passt mit Reserve in den GPU-Slot".into(),
                safe: true,
            },
        );
    }
    if ram_need <= usable_ram
        && profile.file_size_mb
            <= resources
                .ssd_free_mb
                .saturating_sub(resources.ssd_reserve_mb)
    {
        let layer_ratio = usable_vram as f64 / profile.estimated_gpu_mb.max(1) as f64;
        let gpu_layers = ((profile.layers as f64) * layer_ratio).floor() as u32;
        return identified_plan(
            profile,
            resource_generation_hash,
            Plan {
                plan_id: String::new(),
                resource_generation,
                model: profile.model.clone(),
                lane: Lane::RamBalanced,
                gpu_layers: gpu_layers.min(profile.layers.saturating_sub(1)),
                ctx_size: ctx,
                estimated_vram_mb: usable_vram
                    .min(profile.estimated_gpu_mb.saturating_add(gpu_overhead)),
                estimated_ram_mb: ram_need,
                reason: "GPU-Slot reicht nicht vollständig; RAM/SSD-Puffer verfügbar".into(),
                safe: true,
            },
        );
    }
    if profile.file_size_mb
        <= resources
            .ssd_free_mb
            .saturating_sub(resources.ssd_reserve_mb)
        && usable_ram >= kv_overhead
    {
        return identified_plan(
            profile,
            resource_generation_hash,
            Plan {
                plan_id: String::new(),
                resource_generation,
                model: profile.model.clone(),
                lane: Lane::CpuSafe,
                gpu_layers: 0,
                ctx_size: ctx,
                estimated_vram_mb: kv_overhead,
                estimated_ram_mb: ram_need,
                reason: "Nur CPU-Safe-Lane innerhalb der harten Speichergrenzen".into(),
                safe: true,
            },
        );
    }
    identified_plan(
        profile,
        resource_generation_hash,
        Plan {
            plan_id: String::new(),
            resource_generation,
            model: profile.model.clone(),
            lane: Lane::Reject,
            gpu_layers: 0,
            ctx_size: ctx,
            estimated_vram_mb: 0,
            estimated_ram_mb: 0,
            reason: "Keine sichere Lane: Speicherreserven würden verletzt".into(),
            safe: false,
        },
    )
}

/// Liefert dieselbe Generation, die `plan()` in den Plan schreibt.
///
/// Freie Kapazitaeten werden in 64-MiB-Schritten nach unten normalisiert.
/// Das ist konservativ und verhindert, dass kleine Messschwankungen einen
/// unmittelbar zuvor erzeugten Plan unbrauchbar machen. Reserven bleiben
/// exakt Bestandteil der Generation.
pub fn resource_generation(resources: &Resources) -> String {
    format!(
        "resources-v1-{:016x}",
        resource_generation_from_normalized(&normalize_resources(resources))
    )
}

fn normalize_resources(resources: &Resources) -> Resources {
    const BUCKET_MB: u64 = 64;
    let floor = |value: u64| value / BUCKET_MB * BUCKET_MB;
    Resources {
        vram_free_mb: floor(resources.vram_free_mb),
        ram_free_mb: floor(resources.ram_free_mb),
        ssd_free_mb: floor(resources.ssd_free_mb),
        vram_reserve_mb: resources.vram_reserve_mb,
        ram_reserve_mb: resources.ram_reserve_mb,
        ssd_reserve_mb: resources.ssd_reserve_mb,
    }
}

fn resource_generation_from_normalized(resources: &Resources) -> u64 {
    let mut hash = StableHash::new();
    hash.write_bytes(b"tri-ai-resource-generation-v1");
    hash.write_u64(resources.vram_free_mb);
    hash.write_u64(resources.ram_free_mb);
    hash.write_u64(resources.ssd_free_mb);
    hash.write_u64(resources.vram_reserve_mb);
    hash.write_u64(resources.ram_reserve_mb);
    hash.write_u64(resources.ssd_reserve_mb);
    hash.finish()[0]
}

fn identified_plan(profile: &ModelProfile, resource_generation: u64, mut plan: Plan) -> Plan {
    let mut hash = StableHash::new();
    hash.write_bytes(b"tri-ai-plan-v1");
    hash.write_str(&profile.model);
    hash.write_u64(profile.file_size_mb);
    hash.write_u64(profile.estimated_gpu_mb);
    hash.write_u64(profile.estimated_ram_mb);
    hash.write_u64(u64::from(profile.layers));
    hash.write_u64(u64::from(profile.context_tokens));
    hash.write_u64(u64::from(profile.requested_context_tokens));
    hash.write_u64(resource_generation);
    hash.write_u64(match plan.lane {
        Lane::GpuFast => 1,
        Lane::RamBalanced => 2,
        Lane::CpuSafe => 3,
        Lane::Reject => 4,
    });
    hash.write_u64(u64::from(plan.gpu_layers));
    hash.write_u64(u64::from(plan.ctx_size));
    hash.write_u64(plan.estimated_vram_mb);
    hash.write_u64(plan.estimated_ram_mb);
    hash.write_u64(u64::from(plan.safe));
    let digest = hash.finish();
    plan.plan_id = format!(
        "plan-v1-{:016x}{:016x}{:016x}{:016x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    plan
}

/// Kleine, plattformunabhaengige ID-Funktion mit explizitem Byteformat.
/// Plan-IDs sind Staleness-Tokens und keine Authentifizierungsgeheimnisse.
struct StableHash {
    state: [u64; 4],
}

impl StableHash {
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self {
            state: [
                0xcbf2_9ce4_8422_2325,
                0x8422_2325_cbf2_9ce4,
                0x9e37_79b9_7f4a_7c15,
                0x517c_c1b7_2722_0a95,
            ],
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        for (index, state) in self.state.iter_mut().enumerate() {
            for byte in bytes {
                *state ^= u64::from(*byte).wrapping_add(index as u64);
                *state = state.wrapping_mul(Self::PRIME);
            }
        }
    }

    fn write_str(&mut self, value: &str) {
        self.write_bytes(value.as_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        for (index, state) in self.state.iter_mut().enumerate() {
            for byte in value.to_le_bytes() {
                *state ^= u64::from(byte).wrapping_add(index as u64);
                *state = state.wrapping_mul(Self::PRIME);
            }
        }
    }

    fn finish(self) -> [u64; 4] {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ModelProfile {
        ModelProfile {
            model: "m.gguf".into(),
            file_size_mb: 5000,
            estimated_gpu_mb: 6000,
            estimated_ram_mb: 7000,
            layers: 32,
            context_tokens: 8192,
            requested_context_tokens: 4096,
        }
    }

    fn resources(vram_free_mb: u64, ram_free_mb: u64, ssd_free_mb: u64) -> Resources {
        Resources {
            vram_free_mb,
            ram_free_mb,
            ssd_free_mb,
            vram_reserve_mb: 1000,
            ram_reserve_mb: 1000,
            ssd_reserve_mb: 1000,
        }
    }

    #[test]
    fn prefers_gpu_when_it_fits() {
        assert_eq!(
            plan(&profile(), &resources(9000, 16000, 100000)).lane,
            Lane::GpuFast
        );
    }

    #[test]
    fn falls_back_to_ram() {
        assert_eq!(
            plan(&profile(), &resources(3000, 16000, 100000)).lane,
            Lane::RamBalanced
        );
    }

    #[test]
    fn rejects_when_all_budgets_are_too_small() {
        assert_eq!(
            plan(&profile(), &resources(100, 100, 100)).lane,
            Lane::Reject
        );
    }

    #[test]
    fn plan_and_resource_generation_are_deterministic() {
        let profile = profile();
        let resources = resources(9000, 16000, 100000);
        let first = plan(&profile, &resources);
        let second = plan(&profile, &resources);
        assert_eq!(first, second);
        let generation = resource_generation(&resources);
        assert_eq!(first.resource_generation, generation);
        assert!(first.plan_id.starts_with("plan-v1-"));
        assert_eq!(first.plan_id.len(), 72);
    }

    #[test]
    fn insignificant_resource_jitter_stays_in_same_generation() {
        let first = resources(9000, 16000, 100000);
        let mut second = first.clone();
        second.vram_free_mb += 1;
        second.ram_free_mb += 1;
        second.ssd_free_mb += 1;
        assert_eq!(resource_generation(&first), resource_generation(&second));
        assert_eq!(
            plan(&profile(), &first).plan_id,
            plan(&profile(), &second).plan_id
        );
    }

    #[test]
    fn profile_or_plan_change_changes_plan_id() {
        let resources = resources(9000, 16000, 100000);
        let first = plan(&profile(), &resources);
        let mut changed = profile();
        changed.requested_context_tokens = 8192;
        let second = plan(&changed, &resources);
        assert_ne!(first.plan_id, second.plan_id);
    }

    #[test]
    fn material_resource_change_changes_generation_and_plan_id() {
        let first = plan(&profile(), &resources(9000, 16000, 100000));
        let second = plan(&profile(), &resources(8000, 16000, 100000));
        assert_ne!(first.resource_generation, second.resource_generation);
        assert_ne!(first.plan_id, second.plan_id);
    }

    #[test]
    fn f16_kv_requires_more_vram_than_q8_kv() {
        let resources = resources(20_000, 20_000, 100_000);
        let f16 = plan_with_runtime(
            &profile(),
            &resources,
            RuntimeMemoryConfig {
                parallel_slots: 1,
                kv_bytes_per_token: RuntimeMemoryConfig::F16_KV_BYTES_PER_TOKEN,
                compute_overhead_mb: 256,
            },
        );
        let q8 = plan_with_runtime(
            &profile(),
            &resources,
            RuntimeMemoryConfig {
                parallel_slots: 1,
                kv_bytes_per_token: RuntimeMemoryConfig::Q8_KV_BYTES_PER_TOKEN,
                compute_overhead_mb: 256,
            },
        );
        assert!(f16.estimated_vram_mb > q8.estimated_vram_mb);
    }

    #[test]
    fn kv_estimate_scales_with_context_and_parallel_slots() {
        let resources = resources(50_000, 50_000, 100_000);
        let mut short = profile();
        short.requested_context_tokens = 1024;
        let mut long = short.clone();
        long.requested_context_tokens = 8192;
        let runtime = RuntimeMemoryConfig::default();
        let short_plan = plan_with_runtime(&short, &resources, runtime);
        let long_plan = plan_with_runtime(&long, &resources, runtime);
        let parallel_plan = plan_with_runtime(
            &long,
            &resources,
            RuntimeMemoryConfig {
                parallel_slots: 2,
                ..runtime
            },
        );
        assert!(long_plan.estimated_vram_mb > short_plan.estimated_vram_mb);
        assert!(parallel_plan.estimated_vram_mb > long_plan.estimated_vram_mb);
    }

    #[test]
    fn rejects_sentinel_layer_count_instead_of_starting_999_layers() {
        let mut sentinel = profile();
        sentinel.layers = 999;
        let result = plan(&sentinel, &resources(20_000, 20_000, 100_000));
        assert_eq!(result.lane, Lane::Reject);
        assert!(!result.safe);
    }

    #[test]
    fn rejects_full_offload_when_kv_and_reserve_exhaust_vram() {
        let mut model = profile();
        model.estimated_gpu_mb = 6_000;
        model.estimated_ram_mb = 50_000;
        model.requested_context_tokens = 8192;
        let result = plan_with_runtime(
            &model,
            &resources(7_000, 100, 100),
            RuntimeMemoryConfig::default(),
        );
        assert_eq!(result.lane, Lane::Reject);
    }
}
