//! cdh library entry: re-export modules and public APIs.

pub mod controller;
pub mod frecency;
pub mod picker;
pub mod recommend; // ← 新增

pub use frecency::{Frecency, FrecencyIndex, FrecencyState};
pub use recommend::{recommend, recommend_paths, RecommendOpt, Recommendation};
