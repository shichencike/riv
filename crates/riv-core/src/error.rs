//! riv-core 的统一错误类型。

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a riv repository (or any parent): {0}")]
    NotARepo(PathBuf),

    #[error("repository already exists: {0}")]
    RepoExists(PathBuf),

    #[error("object not found: {0}")]
    ObjectNotFound(String),

    #[error("invalid object id: {0}")]
    InvalidOid(String),

    #[error("ambiguous object id prefix: {0}")]
    AmbiguousOid(String),

    #[error("corrupt object: {0}")]
    CorruptObject(String),

    #[error("corrupt index: {0}")]
    CorruptIndex(String),

    #[error("invalid reference: {0}")]
    InvalidRef(String),

    #[error("invalid revision: {0}")]
    InvalidRevision(String),

    #[error("branch not found: {0}")]
    BranchNotFound(String),

    #[error("branch already exists: {0}")]
    BranchExists(String),

    #[error("cannot delete the current branch: {0}")]
    DeleteCurrentBranch(String),

    #[error("path is outside the repository: {0}")]
    OutsideRepo(String),

    #[error("nothing to commit, working tree clean")]
    NothingToCommit,

    #[error("your local changes would be overwritten by checkout:\n{}", .0.join("\n"))]
    LocalChangesOverwritten(Vec<String>),

    #[error("untracked working tree files would be overwritten by checkout:\n{}", .0.join("\n"))]
    UntrackedOverwritten(Vec<String>),

    #[error("pathspec did not match any files: {0}")]
    PathspecNoMatch(String),

    #[error("empty ident name (for <{0}>) not allowed")]
    EmptyIdentName(String),

    #[error("name consists only of disallowed characters: {0}")]
    BadIdentName(String),

    #[error("the following paths are ignored by .rivignore:\n{}", .0.join("\n"))]
    PathIgnored(Vec<String>),

    #[error("config error: {0}")]
    Config(String),

    #[error("{0}")]
    Msg(String),
}

impl Error {
    pub fn msg<S: Into<String>>(s: S) -> Self {
        Error::Msg(s.into())
    }
}
