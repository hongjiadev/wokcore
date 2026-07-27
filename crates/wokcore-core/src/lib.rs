pub mod config;
pub mod id;
pub mod secret;

pub use id::ClientId;

pub mod build {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct BuildInfo {
        pub product: &'static str,
        pub version: &'static str,
    }

    impl BuildInfo {
        pub const fn current() -> Self {
            Self {
                product: "WokCore",
                version: env!("CARGO_PKG_VERSION"),
            }
        }
    }
}
