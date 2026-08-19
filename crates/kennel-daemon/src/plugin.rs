pub trait Plugin: Send {
    fn manifest(&self) -> kennel_proto::ExtensionManifest;
    fn check(&mut self) -> kennel_proto::MonitorStatus;
    fn fix(&mut self);
}

pub type PluginFactory = Box<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;
