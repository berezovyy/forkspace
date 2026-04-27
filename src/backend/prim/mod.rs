#[cfg(target_os = "macos")]
pub mod clonefile;
pub mod copy;
#[cfg(target_os = "linux")]
pub mod reflink;

#[cfg(target_os = "macos")]
pub use clonefile::ClonefilePrim;
pub use copy::CopyPrim;
#[cfg(target_os = "linux")]
pub use reflink::ReflinkPrim;
