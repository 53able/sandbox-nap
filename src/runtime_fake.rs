//! メモリ上の偽の実行基盤(テスト用)。安全装置は本物と同じ規則で動く。

use crate::runtime::{
    is_archive_of, normalize_image_name, ContainerInfo, ContainerRuntime, ContainerState, CreateSpec, RtResult,
    RuntimeError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct FakeContainer {
    pub state: ContainerState,
    pub managed: bool,
}

#[derive(Default)]
pub struct FakeState {
    pub containers: BTreeMap<String, FakeContainer>,
    pub images: BTreeSet<String>,
    pub network_ready: bool,
    /// 実行した操作の記録(例: "stop a")。
    pub ops: Vec<String>,
    /// true のとき commit が失敗する。
    pub fail_commit: bool,
}

#[derive(Default)]
pub struct FakeRuntime {
    pub state: Mutex<FakeState>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_unmanaged(&self, name: &str, state: ContainerState) {
        self.state.lock().unwrap().containers.insert(
            name.into(),
            FakeContainer { state, managed: false },
        );
    }

    pub fn ops(&self) -> Vec<String> {
        self.state.lock().unwrap().ops.clone()
    }

    pub fn state_of(&self, name: &str) -> Option<ContainerState> {
        self.state.lock().unwrap().containers.get(name).map(|c| c.state)
    }

    fn require_managed(s: &FakeState, name: &str) -> RtResult<()> {
        match s.containers.get(name) {
            Some(c) if c.managed => Ok(()),
            Some(_) => Err(RuntimeError::NotManaged(name.into())),
            None => Err(RuntimeError::NotFound(name.into())),
        }
    }
}

impl ContainerRuntime for FakeRuntime {
    fn list(&self) -> RtResult<Vec<ContainerInfo>> {
        let s = self.state.lock().unwrap();
        Ok(s.containers
            .iter()
            .map(|(id, c)| ContainerInfo { id: id.clone(), state: c.state, managed: c.managed })
            .collect())
    }

    fn create(&self, spec: &CreateSpec) -> RtResult<()> {
        let mut s = self.state.lock().unwrap();
        if s.containers.contains_key(&spec.name) {
            return Err(RuntimeError::Command { args: vec!["create".into()], code: 1, stderr: "exists".into() });
        }
        let image = normalize_image_name(&spec.image);
        s.containers.insert(
            spec.name.clone(),
            FakeContainer { state: ContainerState::Stopped, managed: true },
        );
        s.ops.push(format!("create {} {}", spec.name, image));
        Ok(())
    }

    fn start(&self, name: &str) -> RtResult<()> {
        let mut s = self.state.lock().unwrap();
        Self::require_managed(&s, name)?;
        s.containers.get_mut(name).unwrap().state = ContainerState::Running;
        s.ops.push(format!("start {name}"));
        Ok(())
    }

    fn stop(&self, name: &str, _timeout: Option<u64>) -> RtResult<()> {
        let mut s = self.state.lock().unwrap();
        Self::require_managed(&s, name)?;
        s.containers.get_mut(name).unwrap().state = ContainerState::Stopped;
        s.ops.push(format!("stop {name}"));
        Ok(())
    }

    fn remove(&self, name: &str) -> RtResult<()> {
        let mut s = self.state.lock().unwrap();
        Self::require_managed(&s, name)?;
        s.containers.remove(name);
        s.ops.push(format!("rm {name}"));
        Ok(())
    }

    fn commit(&self, name: &str, reference: &str) -> RtResult<()> {
        let mut s = self.state.lock().unwrap();
        Self::require_managed(&s, name)?;
        if !is_archive_of(name, reference) {
            return Err(RuntimeError::ImageRefRejected(reference.into()));
        }
        if s.fail_commit {
            return Err(RuntimeError::Command { args: vec!["commit".into()], code: 1, stderr: "injected".into() });
        }
        s.images.insert(normalize_image_name(reference));
        s.ops.push(format!("commit {name} {reference}"));
        Ok(())
    }

    fn image_list(&self) -> RtResult<Vec<String>> {
        Ok(self.state.lock().unwrap().images.iter().cloned().collect())
    }

    fn image_remove(&self, sandbox: &str, reference: &str) -> RtResult<()> {
        if !is_archive_of(sandbox, reference) {
            return Err(RuntimeError::ImageRefRejected(reference.into()));
        }
        let mut s = self.state.lock().unwrap();
        if !s.images.remove(&normalize_image_name(reference)) {
            return Err(RuntimeError::NotFound(reference.into()));
        }
        s.ops.push(format!("image-rm {reference}"));
        Ok(())
    }

    fn ensure_network(&self) -> RtResult<()> {
        self.state.lock().unwrap().network_ready = true;
        Ok(())
    }
}
