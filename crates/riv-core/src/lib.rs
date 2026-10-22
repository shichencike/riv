//! riv-core：riv 版本控制系统的核心库。
//!
//! 分层结构：
//! - **基础层**：`oid`（内容寻址）、`object`（对象格式）、`store`（对象库）、
//!   `index`（暂存区）、`refs`（引用）、`config`（配置）、`error`（错误）。
//! - **仓库层**：`repo`（仓库定位与初始化）、`snapshot`（工作区扫描 / add / 树构建）。
//! - **操作层**：`status`（三方状态）、`commit`（提交）、`log`（历史）、
//!   `branch`（分支管理）、`checkout`（检出/切换）、`treediff`（树间差异）、
//!   `revparse`（修订解析）、`filediff`（文件级差异）。
//! - **凭证层**：`auth`（token 管理：环境变量 > 系统钥匙串 > 本地文件）。
//!
//! riv 使用完全独立的存储格式（`.riv` 目录、`RIVOBJ` 对象格式），
//! 不与 git 互操作。

pub mod auth;
pub mod branch;
pub mod checkout;
pub mod commit;
pub mod config;
pub mod error;
pub mod filediff;
pub mod index;
pub mod log;
pub mod object;
pub mod oid;
pub mod refs;
pub mod repo;
pub mod revparse;
pub mod snapshot;
pub mod status;
pub mod store;
pub mod treediff;

// ---- 常用类型再导出，方便下游 crate 使用 ----
pub use error::{Error, Result};
pub use object::{Commit, Object, ObjectType, Signature, Tree, TreeEntry};
pub use oid::Oid;
pub use repo::Repo;
pub use store::ObjectStore;
