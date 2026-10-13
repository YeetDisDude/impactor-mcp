//! Tool registry. Each submodule exposes:
//!   `pub fn definitions() -> Vec<serde_json::Value>`
//!   `pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult>`
//! where `call` returns `None` for names it does not own.

pub mod accounts;
pub mod device;
pub mod macho;
pub mod portal;
pub mod refresh;
pub mod sign;

use serde_json::Value;

use crate::common::{Ctx, ToolResult};

pub fn definitions() -> Vec<Value> {
    let mut all = Vec::new();
    all.extend(accounts::definitions());
    all.extend(portal::definitions());
    all.extend(device::definitions());
    all.extend(macho::definitions());
    all.extend(sign::definitions());
    all.extend(refresh::definitions());
    all
}

pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult> {
    if let Some(r) = accounts::call(name, args, ctx).await {
        return Some(r);
    }
    if let Some(r) = portal::call(name, args, ctx).await {
        return Some(r);
    }
    if let Some(r) = device::call(name, args, ctx).await {
        return Some(r);
    }
    if let Some(r) = macho::call(name, args, ctx).await {
        return Some(r);
    }
    if let Some(r) = sign::call(name, args, ctx).await {
        return Some(r);
    }
    refresh::call(name, args, ctx).await
}
