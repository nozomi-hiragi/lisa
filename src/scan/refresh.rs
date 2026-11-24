use std::path::{Path, PathBuf};

/// ローカルドライブ内の更新範囲。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshTarget {
    pub drive_id: String,
    pub path: PathBuf,
    pub subtree: bool,
}

impl RefreshTarget {
    pub fn contains(&self, path: &Path) -> bool {
        path == self.path || (self.subtree && path.starts_with(&self.path))
    }

    pub fn covers(&self, other: &Self) -> bool {
        self.drive_id == other.drive_id
            && self.contains(&other.path)
            && (self.subtree || !other.subtree)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RefreshRequest {
    pub target: RefreshTarget,
    pub generation: u64,
    pub created: Option<(PathBuf, std::sync::mpsc::Sender<Option<String>>)>,
}
