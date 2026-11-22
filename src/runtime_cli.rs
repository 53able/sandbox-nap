//! `container` CLI を子プロセスとして呼ぶ実装。

use crate::model::{NetworkMode, INTERNAL_NETWORK, MANAGED_LABEL, MANAGED_VALUE};
use crate::runtime::{
    is_archive_of, normalize_image_name, ContainerInfo, ContainerRuntime, ContainerState, CreateSpec, RtResult,
    RuntimeError,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// `container` コマンドの実行器。テストでは偽物に差し替える。
pub trait Runner: Send + Sync {
    fn run(&self, args: &[String]) -> std::io::Result<Output>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, args: &[String]) -> std::io::Result<Output> {
        let out = Command::new("container").args(args).output()?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

pub struct CliRuntime<R: Runner> {
    runner: R,
}

impl CliRuntime<SystemRunner> {
    pub fn system() -> Self {
        CliRuntime::new(SystemRunner)
    }
}

// --- JSON(未知フィールドは無視する) ---

#[derive(Deserialize)]
struct JsonContainer {
    configuration: JsonContainerConfig,
    status: JsonContainerStatus,
}

#[derive(Deserialize)]
struct JsonContainerConfig {
    id: String,
    #[serde(default)]
    labels: HashMap<String, String>,
}

#[derive(Deserialize)]
struct JsonContainerStatus {
    state: String,
}

#[derive(Deserialize)]
struct JsonImage {
    configuration: JsonImageConfig,
}

#[derive(Deserialize)]
struct JsonImageConfig {
    name: String,
}

#[derive(Deserialize)]
struct JsonNetwork {
    id: String,
    #[serde(default)]
    configuration: JsonNetworkConfig,
}

#[derive(Deserialize, Default)]
struct JsonNetworkConfig {
    #[serde(default)]
    labels: HashMap<String, String>,
}

pub fn parse_containers(json: &str) -> RtResult<Vec<ContainerInfo>> {
    let raw: Vec<JsonContainer> = serde_json::from_str(json).map_err(|e| RuntimeError::Parse(e.to_string()))?;
    Ok(raw
        .into_iter()
        .map(|c| ContainerInfo {
            managed: c.configuration.labels.get(MANAGED_LABEL).map(String::as_str) == Some(MANAGED_VALUE),
            id: c.configuration.id,
            state: match c.status.state.as_str() {
                "running" => ContainerState::Running,
                "stopped" => ContainerState::Stopped,
                _ => ContainerState::Other,
            },
        })
        .collect())
}

impl<R: Runner> CliRuntime<R> {
    pub fn new(runner: R) -> Self {
        CliRuntime { runner }
    }

    fn exec(&self, args: &[&str]) -> RtResult<Output> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let out = self.runner.run(&args).map_err(|e| RuntimeError::Command {
            args: args.clone(),
            code: -1,
            stderr: format!("cannot run container: {e}"),
        })?;
        if out.code != 0 {
            return Err(RuntimeError::Command { args, code: out.code, stderr: out.stderr });
        }
        Ok(out)
    }

    /// 破壊的操作の直前に、対象が asb の管理物であることを再取得して確認する。
    fn require_managed(&self, name: &str) -> RtResult<ContainerInfo> {
        match self.find(name)? {
            Some(c) if c.managed => Ok(c),
            Some(_) => Err(RuntimeError::NotManaged(name.to_string())),
            None => Err(RuntimeError::NotFound(name.to_string())),
        }
    }

    /// `container --version` が 1.7 系か。違えば警告文を返す。
    pub fn version_warning(&self) -> Option<String> {
        match self.exec(&["--version"]) {
            Ok(o) if o.stdout.contains("version 1.7.") => None,
            Ok(o) => Some(format!("warning: asb supports container 1.7.x only; found: {}", o.stdout.trim())),
            Err(e) => Some(format!("warning: cannot determine container version: {e}")),
        }
    }
}

impl<R: Runner> ContainerRuntime for CliRuntime<R> {
    fn list(&self) -> RtResult<Vec<ContainerInfo>> {
        let out = self.exec(&["ls", "-a", "--format", "json"])?;
        parse_containers(&out.stdout)
    }

    fn create(&self, spec: &CreateSpec) -> RtResult<()> {
        let label = format!("{MANAGED_LABEL}={MANAGED_VALUE}");
        let memory = format!("{}M", spec.memory_mb);
        let cpus = spec.cpus.map(|c| c.to_string());
        let mut args: Vec<&str> = vec!["create", "--name", &spec.name, "--label", &label, "--init", "--entrypoint", "sleep"];
        args.extend(["--memory", &memory]);
        if let Some(c) = &cpus {
            args.extend(["--cpus", c]);
        }
        if spec.network == NetworkMode::Internal {
            args.extend(["--network", INTERNAL_NETWORK]);
        }
        args.extend([spec.image.as_str(), "infinity"]);
        self.exec(&args).map(|_| ())
    }

    fn start(&self, name: &str) -> RtResult<()> {
        self.require_managed(name)?;
        self.exec(&["start", name]).map(|_| ())
    }

    fn stop(&self, name: &str, timeout_secs: Option<u64>) -> RtResult<()> {
        self.require_managed(name)?;
        let t = timeout_secs.map(|t| t.to_string());
        let mut args = vec!["stop"];
        if let Some(t) = &t {
            args.extend(["--time", t]);
        }
        args.push(name);
        self.exec(&args).map(|_| ())
    }

    fn remove(&self, name: &str) -> RtResult<()> {
        let c = self.require_managed(name)?;
        if c.state != ContainerState::Stopped {
            self.exec(&["stop", name])?;
        }
        self.exec(&["rm", name]).map(|_| ())
    }

    fn commit(&self, name: &str, reference: &str) -> RtResult<()> {
        self.require_managed(name)?;
        if !is_archive_of(name, reference) {
            return Err(RuntimeError::ImageRefRejected(reference.to_string()));
        }
        self.exec(&["commit", name, reference]).map(|_| ())
    }

    fn image_list(&self) -> RtResult<Vec<String>> {
        let out = self.exec(&["image", "ls", "--format", "json"])?;
        let raw: Vec<JsonImage> = serde_json::from_str(&out.stdout).map_err(|e| RuntimeError::Parse(e.to_string()))?;
        Ok(raw.into_iter().map(|i| normalize_image_name(&i.configuration.name)).collect())
    }

    fn image_remove(&self, sandbox: &str, reference: &str) -> RtResult<()> {
        if !is_archive_of(sandbox, reference) {
            return Err(RuntimeError::ImageRefRejected(reference.to_string()));
        }
        self.exec(&["image", "rm", reference]).map(|_| ())
    }

    fn ensure_network(&self) -> RtResult<()> {
        let out = self.exec(&["network", "ls", "--format", "json"])?;
        let nets: Vec<JsonNetwork> = serde_json::from_str(&out.stdout).map_err(|e| RuntimeError::Parse(e.to_string()))?;
        if let Some(n) = nets.iter().find(|n| n.id == INTERNAL_NETWORK) {
            return if n.configuration.labels.get(MANAGED_LABEL).map(String::as_str) == Some(MANAGED_VALUE) {
                Ok(())
            } else {
                Err(RuntimeError::NetworkConflict(INTERNAL_NETWORK.to_string()))
            };
        }
        let label = format!("{MANAGED_LABEL}={MANAGED_VALUE}");
        self.exec(&["network", "create", "--internal", "--label", &label, INTERNAL_NETWORK]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 呼び出しを記録し、引数に応じて固定の応答を返す偽の実行器。
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        containers_json: String,
        images_json: String,
        networks_json: String,
    }

    impl FakeRunner {
        fn new(containers_json: &str) -> Self {
            FakeRunner {
                calls: Mutex::new(vec![]),
                containers_json: containers_json.into(),
                images_json: "[]".into(),
                networks_json: "[]".into(),
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
        fn mutating_calls(&self) -> Vec<Vec<String>> {
            self.calls().into_iter().filter(|c| !(c[0] == "ls" || (c.len() > 1 && c[1] == "ls"))).collect()
        }
    }

    impl Runner for &FakeRunner {
        fn run(&self, args: &[String]) -> std::io::Result<Output> {
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                ["ls", ..] => self.containers_json.clone(),
                ["image", "ls", ..] => self.images_json.clone(),
                ["network", "ls", ..] => self.networks_json.clone(),
                _ => String::new(),
            };
            Ok(Output { code: 0, stdout, stderr: String::new() })
        }
    }

    const LS: &str = r#"[
      {"configuration":{"id":"buildkit","labels":{"com.apple.container.plugin":"x"},"extra":1},"status":{"state":"running","startedDate":"2026"}},
      {"configuration":{"id":"web","labels":{}},"status":{"state":"running"}},
      {"configuration":{"id":"mine","labels":{"asb.managed":"true"}},"status":{"state":"running"}},
      {"configuration":{"id":"fake","labels":{"asb.managed":"yes"}},"status":{"state":"stopped"}}
    ]"#;

    #[test]
    fn parses_and_ignores_unknown_fields() {
        let cs = parse_containers(LS).unwrap();
        assert_eq!(cs.len(), 4);
        assert!(!cs[0].managed && !cs[1].managed && cs[2].managed && !cs[3].managed);
        assert_eq!(cs[3].state, ContainerState::Stopped);
    }

    #[test]
    fn destructive_ops_refuse_unmanaged_without_calling_cli() {
        let f = FakeRunner::new(LS);
        let rt = CliRuntime::new(&f);
        for name in ["buildkit", "web", "fake"] {
            assert_eq!(rt.stop(name, None), Err(RuntimeError::NotManaged(name.into())));
            assert_eq!(rt.remove(name), Err(RuntimeError::NotManaged(name.into())));
            assert_eq!(rt.start(name), Err(RuntimeError::NotManaged(name.into())));
            assert!(matches!(rt.commit(name, &format!("asb-archive/{name}:g1")), Err(RuntimeError::NotManaged(_))));
        }
        assert!(f.mutating_calls().is_empty(), "unexpected calls: {:?}", f.mutating_calls());
    }

    #[test]
    fn destructive_ops_on_missing_container_are_not_found() {
        let f = FakeRunner::new(LS);
        let rt = CliRuntime::new(&f);
        assert_eq!(rt.stop("nope", None), Err(RuntimeError::NotFound("nope".into())));
        assert!(f.mutating_calls().is_empty());
    }

    #[test]
    fn managed_stop_and_remove_call_cli() {
        let f = FakeRunner::new(LS);
        let rt = CliRuntime::new(&f);
        rt.stop("mine", Some(7)).unwrap();
        rt.remove("mine").unwrap();
        assert_eq!(
            f.mutating_calls(),
            vec![
                vec!["stop".to_string(), "--time".into(), "7".into(), "mine".into()],
                vec!["stop".to_string(), "mine".into()],
                vec!["rm".to_string(), "mine".into()],
            ]
        );
    }

    #[test]
    fn image_remove_requires_exact_repo() {
        let f = FakeRunner::new(LS);
        let rt = CliRuntime::new(&f);
        assert!(matches!(rt.image_remove("foo", "asb-archive/foo-bar:g1"), Err(RuntimeError::ImageRefRejected(_))));
        assert!(matches!(rt.image_remove("foo", "library/alpine:3.22"), Err(RuntimeError::ImageRefRejected(_))));
        assert!(f.mutating_calls().is_empty());
        rt.image_remove("foo", "docker.io/asb-archive/foo:g3").unwrap();
        assert_eq!(f.mutating_calls(), vec![vec!["image".to_string(), "rm".into(), "docker.io/asb-archive/foo:g3".into()]]);
    }

    #[test]
    fn commit_requires_own_archive_ref() {
        let f = FakeRunner::new(LS);
        let rt = CliRuntime::new(&f);
        assert!(matches!(rt.commit("mine", "asb-archive/other:g1"), Err(RuntimeError::ImageRefRejected(_))));
        assert!(f.mutating_calls().is_empty());
        rt.commit("mine", "asb-archive/mine:g1").unwrap();
    }

    #[test]
    fn create_args() {
        let f = FakeRunner::new("[]");
        let rt = CliRuntime::new(&f);
        rt.create(&CreateSpec { name: "a".into(), image: "alpine:3.22".into(), cpus: Some(2), memory_mb: 512, network: NetworkMode::Internal })
            .unwrap();
        rt.create(&CreateSpec { name: "b".into(), image: "alpine:3.22".into(), cpus: None, memory_mb: 256, network: NetworkMode::Internet })
            .unwrap();
        let calls = f.calls();
        assert_eq!(
            calls[0].join(" "),
            "create --name a --label asb.managed=true --init --entrypoint sleep --memory 512M --cpus 2 --network asb-internal alpine:3.22 infinity"
        );
        assert_eq!(
            calls[1].join(" "),
            "create --name b --label asb.managed=true --init --entrypoint sleep --memory 256M alpine:3.22 infinity"
        );
    }

    #[test]
    fn ensure_network_creates_or_rejects_conflict() {
        let mut f = FakeRunner::new("[]");
        {
            let rt = CliRuntime::new(&f);
            rt.ensure_network().unwrap();
            assert_eq!(
                f.calls().last().unwrap().join(" "),
                "network create --internal --label asb.managed=true asb-internal"
            );
        }
        f.networks_json = r#"[{"id":"asb-internal","configuration":{"labels":{}}}]"#.into();
        {
            let rt = CliRuntime::new(&f);
            assert_eq!(rt.ensure_network(), Err(RuntimeError::NetworkConflict("asb-internal".into())));
        }
        f.networks_json = r#"[{"id":"asb-internal","configuration":{"labels":{"asb.managed":"true"}}}]"#.into();
        let before = f.calls().len();
        let rt = CliRuntime::new(&f);
        rt.ensure_network().unwrap();
        assert_eq!(f.calls().len(), before + 1, "only network ls should run");
    }

    #[test]
    fn image_list_normalizes() {
        let mut f = FakeRunner::new("[]");
        f.images_json = r#"[{"configuration":{"name":"docker.io/asb-archive/a:g1","descriptor":{}},"id":"x"}]"#.into();
        let rt = CliRuntime::new(&f);
        assert_eq!(rt.image_list().unwrap(), vec!["asb-archive/a:g1".to_string()]);
        assert!(rt.image_exists("asb-archive/a:g1").unwrap());
        assert!(!rt.image_exists("asb-archive/a:g2").unwrap());
    }
}
