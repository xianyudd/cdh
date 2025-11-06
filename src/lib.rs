//! cdh library entry: re-export modules and public APIs.

pub mod frecency;
pub mod recommend;
pub mod picker;
pub mod controller; // ← 新增

pub use frecency::{Frecency, FrecencyIndex, FrecencyState};
pub use recommend::{recommend, recommend_paths, RecommendOpt, Recommendation};
