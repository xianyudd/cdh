//! cdh library entry: re-export modules and public APIs.

pub mod controller;
pub mod frecency;
pub mod picker;
pub mod recommend; 
pub mod paths;
pub mod app;
pub mod config;
pub mod history;

pub use frecency::{Frecency, FrecencyIndex, FrecencyState};
pub use recommend::{recommend, recommend_paths, RecommendOpt, Recommendation};
pub use paths::Paths;
pub use app::AppContext;
pub use config::EffectiveConfig;
