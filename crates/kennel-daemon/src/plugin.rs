use std::sync::Arc;

pub trait Plugin: Send {
    fn manifest(&self) -> kennel_proto::ExtensionManifest;
    fn check(&mut self) -> kennel_proto::MonitorStatus;
    fn fix(&mut self);
}

// `Arc`, not `Box`, so `Registry::make_plugin` can clone the factory out from
// under its mutex and call it with the lock released -- a factory that panics
// (or blocks on I/O) must not be able to poison/hold the registry's lock and
// take every scheduler thread and socket handler down with it.
pub type PluginFactory = Arc<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;
