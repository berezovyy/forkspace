#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub tmpfs_backing: bool,
    pub network_namespace: bool,
    pub instant_fork: bool,
    pub sealed_parent: bool,
}
