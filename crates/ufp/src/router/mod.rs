//! 路由：把一次请求映射到具体候选（条目 × key）上。

pub mod select;
pub mod session;

pub use select::{plan, Plan, RequestNeeds, SelectError};
pub use session::{extract_session_id, Sessions};
