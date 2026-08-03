//! 模型登记表与分派路由（#10 #11 #12；P1-2/P1-6）。
//!
//! - 登记表是 Phase 0 实测知识的机器可读沉淀：quantization 记**格式+档位**而非
//!   位数（MLX-int4 死循环、AWQ-int4 假完成+盲改、MLX-8bit 稳、NVFP4/bf16 全过
//!   ——「4bit」三种格式三种命运）；「量化损伤」与「家族短板」分列（已可实验区分）。
//! - 持久化为 `<CODEX_HOME>/supervisor.models.toml`（顶层 *.toml → 随 repo 版本化，
//!   手工可编辑）。
//! - 路由（#12）：MCP 侧无 profile，分派输出显式 `model + model_provider`
//!   （provider 可嵌套临时定义，继承 codex `config` 能力）。

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::paths::StoreError;
use crate::paths::SupervisorHome;

pub const MODELS_SCHEMA_VERSION: u32 = 1;

/// 南向 provider 引用：`id` 指向 codex config 里已定义的 provider；
/// `base_url`/`wire_api` 存在时可在逐任务 config 里嵌套完整定义（#12）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRef {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_api: Option<String>,
}

/// 量化档案（#10）：格式与档位是一等属性，比「位数」更接近因果
/// （NVFP4 4bit ≠ MLX int4 4bit，实测命运迥异）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quantization {
    /// 如 `NVFP4` / `MLX-int4` / `MLX-8bit` / `AWQ-int4` / `bf16`。
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<u8>,
}

/// serve 引擎与版本（#10）：假完成的 vLLM 0.21 工具通道嫌疑并存，
/// 引擎版本是缺陷归因的必要维度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineInfo {
    /// 如 `vllm` / `omlx` / `ds4.c`。
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// thinking 缺省档（#11）：thinking 默认开的 serve 简单任务耗时 2×（实测
/// 26s→54s），分派时必须知情。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingDefault {
    On,
    Off,
    Unknown,
}

/// 延迟档（#11 分派三维之一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LatencyClass {
    Fast,
    Medium,
    Slow,
}

/// 写任务可靠性（#10）：量化档位限制任务类型的执行依据
/// （gemma 4bit 两种格式写任务 0/2 存活）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteTaskSupport {
    Reliable,
    Unreliable,
    Broken,
}

/// 中间层参数注入（#14；P1-7 消费）：codex 的 Responses 请求从不携带采样参数
/// （代理逐请求实证），服务端合规（DSpark 要求显式 temp=0）由中间层保证。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InjectParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

/// 登记表条目。字段顺序有意为「标量/数组在前、子表在后」——TOML 序列化的
/// 硬约束（值必须先于子表出现）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// 服务端 model 名（分派输出 `model` 字段的传值）。
    pub served_name: String,
    pub thinking: ThinkingDefault,
    pub latency: LatencyClass,
    pub write_tasks: WriteTaskSupport,
    /// 量化损伤记录（#10 分列——换量化档可消除的缺陷）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quant_damage: Vec<String>,
    /// 家族短板记录（#10 分列——量化无关的认知特质，如 26B-A4B 可达性翻车）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub family_traits: Vec<String>,
    /// preflight 探测 URL（#7/#16）——**裸 IP**（.local mDNS 会间歇挂死本机 curl）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_probe: Option<String>,
    /// escalation 规范说明（#11：on-request 依赖模型自觉，不教不会——
    /// 注入任务模板）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub provider: ProviderRef,
    pub engine: EngineInfo,
    pub quantization: Quantization,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject: Option<InjectParams>,
}

/// 模型登记表：key 为登记名（分派输入 `model_key`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistry {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            schema_version: MODELS_SCHEMA_VERSION,
            models: BTreeMap::new(),
        }
    }

    /// 分派解析（#12）：登记名 → 条目；不存在即拒绝，绝不静默回落缺省模型
    /// （`exec resume` 静默回落 base 缺省的语义差是 #3 的立项实测之一）。
    pub fn resolve(&self, model_key: &str) -> Result<&ModelEntry, RegistryError> {
        self.models
            .get(model_key)
            .ok_or_else(|| RegistryError::UnknownModel {
                model_key: model_key.to_string(),
                known: self.models.keys().cloned().collect(),
            })
    }

    pub fn load(home: &SupervisorHome) -> Result<Self, StoreError> {
        let path = home.models_path();
        let raw = std::fs::read_to_string(&path).map_err(|e| StoreError::Io {
            path: path.clone(),
            source: e,
        })?;
        toml::from_str(&raw).map_err(|e| StoreError::Decode {
            path,
            source: Box::new(e),
        })
    }

    pub fn save(&self, home: &SupervisorHome) -> Result<(), StoreError> {
        let path = home.models_path();
        let raw = toml::to_string_pretty(self).map_err(|e| StoreError::Encode {
            path: path.clone(),
            source: Box::new(e),
        })?;
        codex_utils_path::write_atomically(&path, &raw).map_err(|e| StoreError::Io {
            path,
            source: e,
        })
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown model key {model_key:?}; registered: {known:?}")]
    UnknownModel {
        model_key: String,
        known: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn sample_entry() -> ModelEntry {
        ModelEntry {
            served_name: "nvidia/Qwen3.6-35B-A3B-NVFP4".to_string(),
            thinking: ThinkingDefault::On,
            latency: LatencyClass::Medium,
            write_tasks: WriteTaskSupport::Reliable,
            quant_damage: Vec::new(),
            family_traits: vec!["深 bug 后果推演偶带臆测".to_string()],
            endpoint_probe: Some("http://192.168.3.1:8000/v1/models".to_string()),
            escalation_note: None,
            notes: None,
            provider: ProviderRef {
                id: "rdos-vllm".to_string(),
                base_url: Some("http://192.168.3.1:8000/v1".to_string()),
                wire_api: Some("responses".to_string()),
            },
            engine: EngineInfo {
                kind: "vllm".to_string(),
                version: Some("0.26.1rc1.dev30".to_string()),
            },
            quantization: Quantization {
                format: "NVFP4".to_string(),
                bits: Some(4),
            },
            inject: None,
        }
    }

    #[test]
    fn registry_toml_roundtrip_via_disk() {
        let home = TempDir::new().expect("tempdir");
        let sup = SupervisorHome::new(home.path());
        let mut registry = ModelRegistry::new();
        registry
            .models
            .insert("qwen36-nvfp4".to_string(), sample_entry());

        registry.save(&sup).expect("save");
        let loaded = ModelRegistry::load(&sup).expect("load");
        assert_eq!(loaded, registry);
    }

    #[test]
    fn hand_written_toml_parses() {
        // 手工编辑形态的守门测试：P1-6 入库的登记表就长这样。
        let raw = r#"
schema_version = 1

[models.deepseek-flash]
served_name = "deepseek-v4-flash"
thinking = "unknown"
latency = "slow"
write_tasks = "reliable"
notes = "三家最强：T1 限长版唯一双杀；DSpark 要求显式 temp=0"

[models.deepseek-flash.provider]
id = "rdos-dsflash"
base_url = "http://192.168.3.3:8000/v1"
wire_api = "responses"

[models.deepseek-flash.engine]
kind = "ds4.c"

[models.deepseek-flash.quantization]
format = "unknown"

[models.deepseek-flash.inject]
temperature = 0.0
"#;
        let registry: ModelRegistry = toml::from_str(raw).expect("parse");
        let entry = registry.resolve("deepseek-flash").expect("resolve");
        assert_eq!(entry.served_name, "deepseek-v4-flash");
        assert_eq!(
            entry.inject,
            Some(InjectParams {
                temperature: Some(0.0)
            })
        );
    }

    #[test]
    fn resolve_unknown_key_is_loud() {
        let registry = ModelRegistry::new();
        let err = registry.resolve("nope").expect_err("must fail");
        assert!(matches!(err, RegistryError::UnknownModel { .. }));
        assert!(err.to_string().contains("nope"));
    }
}
