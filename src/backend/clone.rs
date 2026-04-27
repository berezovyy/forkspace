#![cfg(target_os = "macos")]

pub type CloneBackend = super::cow::CowDirBackend<super::prim::ClonefilePrim>;
