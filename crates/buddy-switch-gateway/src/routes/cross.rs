//! 跨作用域执行：按**目标作用域**把一次请求交给对应产品线的 relay。
//!
//! ## 它解决什么
//!
//! 降级链的备选可以指向别的作用域（`wb-cn:glm` → `wb-global:glm` 或 `wb-cn` → `trae-cn`）。
//! 同平台跨区域只是「换 region 参数」，但**跨平台**要换整条 relay —— 本模块就是那层分派。
//!
//! ## 为什么能直接调各产品线的入口函数
//!
//! 四条产品线在**同一进程**里（`shared_state()` 是进程级单例），且**每条产品线都
//! 已经实现了三种协议入口**（chat / messages / responses）。所以跨平台降级只需要
//! 「把请求交给目标产品线**同协议**的那个入口」，协议转换仍由目标入口自己做。
//!
//! ## 2026-10-31 的真机事故：协议被写死成 OpenAI
//!
//! 旧实现无论入口是什么协议，都只调目标产品线的 **`/v1/chat/completions`** 入口。
//! 于是一个 `/v1/messages`（Anthropic）请求跨作用域降级后，客户端收到的是
//! **HTTP 200 + OpenAI `chat.completion` 正文**：
//!
//! ```text
//! API Error: API returned an empty or malformed response (HTTP 200)
//!   — body is JSON but not a Message, 2135 bytes
//! ```
//!
//! 流式更糟：Anthropic 客户端只认 `message_start` / `content_block_*`，收到
//! `data: {"choices":…}` 会解析失败，表现成「只收到 1 个流事件就没了」。
//! 真机复现（`qoder-global:dfmodel` 排队 → 降级到 `wb-global`）见
//! `docs/specs/MODEL-FALLBACK.md`「跨作用域」一节。
//!
//! 因此本模块现在**按入口协议分派**（见 [`CrossProtocol`]），并把这一条写进契约：
//! 目标产品线收到什么协议，就返回什么协议。
//!
//! ## 一条刻意的限制
//!
//! **不把调用方的 Key 带过去**（`key: None`）。配额与白名单在**入口**已经判过一次，
//! 再判一次会让一次降级消耗两份配额；而目标平台的账本按它自己的账号记，与入口 Key 无关。
//!
//! ## 判据的代价（必须知道）
//!
//! 目标产品线的入口返回 `Response`，拿不到「上游事件类型」⇒ 外层只能按**状态码**判
//! 是否继续降级（5xx / 429）。这与 Trae / Qoder 自身中继的判据同一档，不是新增缺陷。

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use buddy_switch_core::modules::product::{Product, Scope};
use buddy_switch_core::modules::qoder::region::QoderRegion;
use buddy_switch_core::modules::trae::region::TraeRegion;
use buddy_switch_core::modules::trae::variant::{all_specs, TraeVariant};

tokio::task_local! {
    /// 跨平台降级的**嵌套深度**。
    ///
    /// ## 为什么需要它
    /// `A 入口 → cross → B 入口 → B 的降级链 → cross → A 入口 → …` 会无限递归。
    /// 编译器只能要求 `Box::pin`，管不了运行时。
    ///
    /// ## 为什么用 `task_local` 而不是参数
    /// 入口函数（`chat_for_region` / `chat_for_variant`）的签名已经很长，为这个
    /// 内部约束再加一个参数会污染四个入口与统一网关。`task_local` **跟随任务**
    /// （不是线程）—— tokio 在 await 点迁移任务时它仍然正确，这正是 `thread_local` 做不到的。
    static CROSS_DEPTH: u8;
}

/// 允许的跨平台嵌套层数。
///
/// 1 = 「A 降级到 B」可以做，但 B 内部**不能再跨平台**（B 自己的降级链只走同作用域）。
/// 这个上限把「无限递归」变成「最多两层」，同时保留双向跨平台的能力。
pub const MAX_CROSS_DEPTH: u8 = 1;

/// 当前跨平台嵌套深度（不在跨平台调用里时为 0）。
pub fn cross_depth() -> u8 {
    CROSS_DEPTH.try_with(|depth| *depth).unwrap_or(0)
}

/// 入口协议 —— 决定**目标产品线用哪个入口**，从而决定回来的响应形状。
///
/// 目标产品线的三条入口各自完成「入站转换 + 出站转换」，所以只要选对入口，
/// 协议就天然一致，本模块**不需要**再做任何流式/非流式的反向转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossProtocol {
    /// `/v1/chat/completions`：OpenAI chat 形态（SSE 与 JSON 都是）。
    OpenAi,
    /// `/v1/messages`：Anthropic 形态。
    Anthropic,
    /// `/v1/responses`：OpenAI Responses API。
    Responses,
}

/// 把请求交给 `target` 作用域对应的产品线，返回它的响应。
///
/// 调用方负责判断响应是否成功（以及是否继续降级）。
///
/// `protocol` **必须**是入口协议：给错了，客户端会收到 HTTP 200 + 另一种协议的正文
/// （真机事故见模块文档）。
pub async fn relay_on_scope(
    target: Scope,
    headers: HeaderMap,
    body: Bytes,
    key: Option<Arc<crate::apikey::KeyActor>>,
    protocol: CrossProtocol,
) -> Response {
    // ★ 限深：已经跨出去过一次的请求，目标平台内部**不许再跨**。
    //   没有这一条，`A → B → A` 会一直递归下去（编译器只挡得住「future 大小」，
    //   挡不住运行时递归）。
    let depth = cross_depth();
    if depth >= MAX_CROSS_DEPTH {
        return depth_exceeded(target);
    }
    CROSS_DEPTH
        .scope(
            depth + 1,
            relay_on_scope_inner(target, headers, body, key, protocol),
        )
        .await
}

async fn relay_on_scope_inner(
    target: Scope,
    headers: HeaderMap,
    body: Bytes,
    key: Option<Arc<crate::apikey::KeyActor>>,
    protocol: CrossProtocol,
) -> Response {
    match target.product() {
        Product::Workbuddy => {
            let state = <crate::WbGateway as crate::GatewayProduct>::shared_state();
            let region = crate::model_fallback::region_of(target);
            let policy = crate::routes::chat::PrefixPolicy::AlreadyResolved;
            match protocol {
                CrossProtocol::OpenAi => {
                    crate::routes::chat::chat_for_region(state, region, headers, body, policy, key)
                        .await
                }
                CrossProtocol::Anthropic => {
                    crate::routes::messages::messages_for_region(
                        state, region, headers, body, policy, key,
                    )
                    .await
                }
                CrossProtocol::Responses => {
                    crate::routes::responses::responses_for_region(
                        state, region, headers, body, policy, key,
                    )
                    .await
                }
            }
        }
        Product::Trae => {
            let Some(variant) = trae_variant_of(target) else {
                return unsupported(target);
            };
            let state = <crate::trae::TraeGateway as crate::GatewayProduct>::shared_state();
            match protocol {
                CrossProtocol::OpenAi => {
                    crate::trae::routes::chat_for_variant(state, variant, body, key).await
                }
                CrossProtocol::Anthropic => {
                    crate::trae::routes::messages_for_variant(state, variant, body, key).await
                }
                CrossProtocol::Responses => {
                    crate::trae::routes::responses_for_variant(state, variant, body, key).await
                }
            }
        }
        Product::Qoder => {
            let state = <crate::qoder::QoderGateway as crate::GatewayProduct>::shared_state();
            crate::qoder::routes::chat_for_region(
                state,
                qoder_region_of(target),
                body,
                qoder_protocol_of(protocol),
                key,
            )
            .await
        }
    }
}

/// 跨作用域协议 → Qoder 产品线的协议枚举。
///
/// 单独抽成函数只为一件事：让「一一对应」这条映射**可被单测钉住**
/// （见本模块 `every_cross_protocol_maps_to_its_own_upstream_protocol`）。
fn qoder_protocol_of(protocol: CrossProtocol) -> crate::qoder::routes::Protocol {
    match protocol {
        CrossProtocol::OpenAi => crate::qoder::routes::Protocol::OpenAi,
        CrossProtocol::Anthropic => crate::qoder::routes::Protocol::Anthropic,
        CrossProtocol::Responses => crate::qoder::routes::Protocol::Responses,
    }
}

/// 该作用域能不能作为**跨作用域**的降级目标（当前进程里有没有这条产品线）。
///
/// 只判「变体存在」这类**结构性**问题；账号有没有、模型有没有由各产品线自己回答
/// （它们会给出比这里更准的错误）。
pub fn is_reachable(target: Scope) -> bool {
    match target.product() {
        Product::Workbuddy | Product::Qoder => true,
        Product::Trae => trae_variant_of(target).is_some(),
    }
}

/// 作用域 → Trae 的**合并版**变体（按区域）。
///
/// 取 `all_specs()`（合并版两条）而不是 `all_known_specs()`（含退役变体）：
/// 跨作用域降级要的是「这个区域当前在用的那一个」，不是历史变体。
pub fn trae_variant_of(scope: Scope) -> Option<TraeVariant> {
    let region = if scope.as_str().ends_with("-global") {
        TraeRegion::Global
    } else {
        TraeRegion::Cn
    };
    all_specs()
        .iter()
        .find(|spec| spec.variant.region() == region)
        .map(|spec| spec.variant)
}

pub fn qoder_region_of(scope: Scope) -> QoderRegion {
    if scope.as_str().ends_with("-global") {
        QoderRegion::Global
    } else {
        QoderRegion::Cn
    }
}

/// 跨平台嵌套超限（`A → B → A`）。
///
/// 返回 502 而不是 400：这是**网关自己的**能力边界，不是客户端请求有问题。
fn depth_exceeded(target: Scope) -> Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(serde_json::json!({
            "error": {
                "message": format!(
                    "降级目标 {} 需要再次跨平台，超过允许的嵌套层数（{}）",
                    target.as_str(),
                    MAX_CROSS_DEPTH
                ),
                "type": "server_error",
                "code": "fallback_cross_depth_exceeded",
            }
        })),
    )
        .into_response()
}

fn unsupported(target: Scope) -> Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({
            "error": {
                "message": format!(
                    "降级目标 {} 不可用：该平台的变体未在当前构建中启用",
                    target.as_str()
                ),
                "type": "invalid_request_error",
                "code": "fallback_target_unavailable",
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入口协议必须**原样**传到目标产品线 —— 这是 2026-10-31 真机事故的回归钉。
    ///
    /// 事故形态：`/v1/messages`（Anthropic）请求在 `qoder-global:dfmodel` 排队后跨作用域
    /// 降级到 `wb-global`，而旧实现**只调目标产品线的 chat 入口**，于是客户端收到
    /// `HTTP 200` + `{"object":"chat.completion",…}`：
    ///
    /// ```text
    /// API Error: API returned an empty or malformed response (HTTP 200)
    ///   — body is JSON but not a Message
    /// ```
    ///
    /// 可证伪：把 `relay_on_scope_inner` 里任一 `CrossProtocol::*` 分支改成常量
    /// （例如 Anthropic 也走 chat），本用例**不会**直接变红（它只校验映射表），
    /// 所以这里同时钉住「映射表覆盖全部协议」—— 少一个分支会编译失败（`match` 穷尽性），
    /// 而把分支接错则靠下面 `qoder_protocol_of` 的逐项断言。
    #[test]
    fn every_cross_protocol_maps_to_its_own_upstream_protocol() {
        use crate::qoder::routes::Protocol;

        // Qoder 侧：三种协议一一对应（这条映射错位 ⇒ 出站形状错）。
        for (cross, expected) in [
            (CrossProtocol::OpenAi, Protocol::OpenAi),
            (CrossProtocol::Anthropic, Protocol::Anthropic),
            (CrossProtocol::Responses, Protocol::Responses),
        ] {
            assert_eq!(
                qoder_protocol_of(cross),
                expected,
                "跨作用域协议 {cross:?} 必须映射到 Qoder 的同名协议"
            );
        }
    }

    /// Anthropic 与 Responses **不是** OpenAI —— 这三者混用的后果各不相同，
    /// 所以不能靠「反正都是 JSON」糊过去。
    #[test]
    fn cross_protocols_are_distinct() {
        assert_ne!(CrossProtocol::Anthropic, CrossProtocol::OpenAi);
        assert_ne!(CrossProtocol::Responses, CrossProtocol::OpenAi);
        assert_ne!(CrossProtocol::Responses, CrossProtocol::Anthropic);
    }
}
