pub mod claude;
pub mod claude_quota;
pub mod claude_refresh;
pub mod codex;
pub mod kiro;
pub mod kiro_quota;
pub mod kiro_refresh;
pub mod kiro_sso;
pub mod quota;
pub mod refresher;

pub use codex::CodexTokenStorage;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// 按账号 id 去重正在进行的 token 刷新（single-flight）。
///
/// 后台扫描循环和 401 触发的 `spawn_refresh` 可能并发刷新同一个账号，而 refresh_token
/// 是一次性轮换的：并发刷新里只有一个会成功，其余会拿着已失效的旧 token 失败，进而把
/// 一个其实已经刷新成功的健康账号标记成 auth_dead / 冷却。用这个集合保证"同一账号同一
/// 时刻只有一次刷新在飞"，其余调用方直接跳过。
#[derive(Clone, Default)]
pub struct InFlight(Arc<Mutex<HashSet<String>>>);

impl InFlight {
    /// 尝试占用某账号的刷新槽位。返回 `Some(guard)` 表示占用成功（调用方应在持有 guard
    /// 期间完成刷新，guard drop 时自动释放）；返回 `None` 表示已有刷新在进行，应跳过。
    pub fn try_acquire(&self, id: &str) -> Option<InFlightGuard> {
        let mut set = self.0.lock().unwrap();
        if set.contains(id) {
            None
        } else {
            set.insert(id.to_string());
            Some(InFlightGuard {
                set: self.0.clone(),
                id: id.to_string(),
            })
        }
    }
}

/// RAII：drop 时把账号 id 从"刷新中"集合移除。
pub struct InFlightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.id);
        }
    }
}
