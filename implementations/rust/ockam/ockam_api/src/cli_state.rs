use crate::cloud::project::Project;
use crate::nodes::models::transport::{CreateTransportJson, TransportMode, TransportType};
use ockam_identity::change_history::{IdentityChangeHistory, IdentityHistoryComparison};
use ockam_identity::{Identity, IdentityIdentifier, SecureChannelRegistry};
use ockam_vault::{storage::FileStorage, Vault};
use rand::random;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use crate::lmdb::LmdbStorage;
use thiserror::Error;

type Result<T> = std::result::Result<T, CliStateError>;

#[derive(Debug, Error)]
pub enum CliStateError {
    #[error("IO error")]
    Io(#[from] std::io::Error),
    #[error("serde error")]
    Serde(#[from] serde_json::Error),
    #[error("ockam error")]
    Ockam(#[from] ockam_core::Error),
    #[error("`{0}` already exists")]
    AlreadyExists(String),
    #[error("`{0}` not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("invalid state version {0}")]
    InvalidVersion(String),
    #[error("unknown error")]
    Unknown,
}

impl From<CliStateError> for ockam_core::Error {
    fn from(e: CliStateError) -> Self {
        match e {
            CliStateError::Ockam(e) => e,
            _ => ockam_core::Error::new(
                ockam_core::errcode::Origin::Application,
                ockam_core::errcode::Kind::Internal,
                e,
            ),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CliState {
    pub vaults: VaultsState,
    pub identities: IdentitiesState,
    pub nodes: NodesState,
    pub projects: ProjectsState,
    dir: PathBuf,
}

impl CliState {
    pub fn new() -> Result<Self> {
        let dir = Self::dir()?;
        std::fs::create_dir_all(dir.join("defaults"))?;
        Ok(Self {
            vaults: VaultsState::new(&dir)?,
            identities: IdentitiesState::new(&dir)?,
            nodes: NodesState::new(&dir)?,
            projects: ProjectsState::new(&dir)?,
            dir,
        })
    }

    pub fn test() -> Result<Self> {
        let tests_dir = dirs::home_dir()
            .ok_or_else(|| CliStateError::NotFound("home dir".to_string()))?
            .join(".ockam")
            .join(".tests")
            .join(random_name());
        std::env::set_var("OCKAM_HOME", tests_dir);
        Self::new()
    }

    pub fn delete(&self, force: bool) -> Result<()> {
        for n in self.nodes.list()? {
            let _ = n.delete(force);
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn dir() -> Result<PathBuf> {
        Ok(match std::env::var("OCKAM_HOME") {
            Ok(dir) => PathBuf::from(&dir),
            Err(_) => dirs::home_dir()
                .ok_or_else(|| CliStateError::NotFound("home dir".to_string()))?
                .join(".ockam"),
        })
    }

    fn defaults_dir() -> Result<PathBuf> {
        Ok(Self::dir()?.join("defaults"))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct VaultsState {
    dir: PathBuf,
}

impl VaultsState {
    fn new(cli_path: &Path) -> Result<Self> {
        let dir = cli_path.join("vaults");
        std::fs::create_dir_all(dir.join("data"))?;
        Ok(Self { dir })
    }

    pub async fn create(&self, name: &str, config: VaultConfig) -> Result<VaultState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if path.exists() {
                return Err(CliStateError::AlreadyExists(format!("vault `{name}`")));
            }
            path
        };
        let contents = serde_json::to_string(&config)?;
        std::fs::write(&path, contents)?;
        if !self.default_path()?.exists() {
            self.set_default(name)?;
        }
        config.get().await?;
        Ok(VaultState { path, config })
    }

    pub fn get(&self, name: &str) -> Result<VaultState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("vault `{name}`")));
            }
            path
        };
        let contents = std::fs::read_to_string(&path)?;
        let config = serde_json::from_str(&contents)?;
        Ok(VaultState { path, config })
    }

    pub fn list(&self) -> Result<Vec<VaultState>> {
        let mut vaults = Vec::default();
        for entry in std::fs::read_dir(&self.dir)? {
            if let Ok(vault) = self.get(&file_stem(&entry?.path())?) {
                vaults.push(vault);
            }
        }
        Ok(vaults)
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        // Retrieve vault. If doesn't exist do nothing.
        let vault_state = match self.get(name) {
            Ok(v) => v,
            Err(CliStateError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };

        // Set default to another vault if it's the default
        if let Ok(default) = self.default() {
            if default.path == vault_state.path {
                let _ = std::fs::remove_file(self.default_path()?);
                let mut vaults = self.list()?;
                vaults.retain(|v| v.path != vault_state.path);
                if let Some(v) = vaults.first() {
                    let name = v.name()?;
                    println!("Setting default vault to '{}'", &name);
                    self.set_default(&name)?;
                }
            }
        }

        // Remove vault file
        std::fs::remove_file(vault_state.path)?;

        Ok(())
    }

    pub fn default_path(&self) -> Result<PathBuf> {
        Ok(CliState::defaults_dir()?.join("vault"))
    }

    pub fn default(&self) -> Result<VaultState> {
        let path = std::fs::canonicalize(self.default_path()?)?;
        let contents = std::fs::read_to_string(&path)?;
        let config = serde_json::from_str(&contents)?;
        Ok(VaultState { path, config })
    }

    pub fn set_default(&self, name: &str) -> Result<VaultState> {
        let original = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("vault `{name}`")));
            }
            path
        };
        let link = self.default_path()?;
        std::os::unix::fs::symlink(original, link)?;
        self.get(name)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct VaultState {
    pub path: PathBuf,
    pub config: VaultConfig,
}

impl VaultState {
    pub fn name(&self) -> Result<String> {
        self.path
            .file_stem()
            .and_then(|s| s.to_os_string().into_string().ok())
            .ok_or_else(|| {
                CliStateError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "failed to parse the vault name",
                ))
            })
    }
}

impl Display for VaultState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Name: {}",
            self.path.as_path().file_stem().unwrap().to_str().unwrap()
        )?;
        writeln!(
            f,
            "Type: {}",
            match self.config.is_aws() {
                true => "AWS KMS",
                false => "OCKAM",
            }
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct VaultConfig {
    path: PathBuf,

    #[serde(default)]
    aws_kms: bool,
}

impl VaultConfig {
    pub fn new(path: PathBuf, aws_kms: bool) -> Result<Self> {
        Ok(Self { path, aws_kms })
    }

    pub fn from_name(name: &str) -> Result<Self> {
        Ok(Self {
            path: Self::path(name)?,
            aws_kms: false,
        })
    }

    pub async fn get(&self) -> Result<Vault> {
        let vault_storage = FileStorage::create(self.path.clone()).await?;
        let mut vault = Vault::new(Some(Arc::new(vault_storage)));
        if self.aws_kms {
            vault.enable_aws_kms().await?
        }
        Ok(vault)
    }

    pub fn path(name: &str) -> Result<PathBuf> {
        let state = CliState::new()?;
        let path = state
            .vaults
            .dir
            .join("data")
            .join(format!("{name}-storage.json"));
        Ok(path)
    }

    pub fn is_aws(&self) -> bool {
        self.aws_kms
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IdentitiesState {
    dir: PathBuf,
}

impl IdentitiesState {
    fn new(cli_path: &Path) -> Result<Self> {
        let dir = cli_path.join("identities");
        std::fs::create_dir_all(dir.join("data"))?;
        Ok(Self { dir })
    }

    pub fn create(&self, name: &str, config: IdentityConfig) -> Result<IdentityState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if path.exists() {
                return Err(CliStateError::AlreadyExists(format!("identity `{name}`")));
            }
            path
        };
        let contents = serde_json::to_string(&config)?;
        std::fs::write(&path, contents)?;
        if !self.default_path()?.exists() {
            self.set_default(name)?;
        }
        Ok(IdentityState {
            name: name.to_string(),
            path,
            config,
        })
    }

    pub fn get(&self, name: &str) -> Result<IdentityState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("identity `{name}`")));
            }
            path
        };
        let contents = std::fs::read_to_string(&path)?;
        let config = serde_json::from_str(&contents)?;
        Ok(IdentityState {
            name: name.to_string(),
            path,
            config,
        })
    }

    pub fn list(&self) -> Result<Vec<IdentityState>> {
        let mut identities: Vec<IdentityState> = vec![];
        for entry in std::fs::read_dir(&self.dir)? {
            if let Ok(identity) = self.get(&file_stem(&entry?.path())?) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        // Retrieve identity. If doesn't exist do nothing.
        let identity = match self.get(name) {
            Ok(i) => i,
            Err(CliStateError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };

        // Abort if identity is being used by some running node.
        identity.in_use()?;

        // Set default to another identity if it's the default
        if let Ok(default) = self.default() {
            if default.path == identity.path {
                let _ = std::fs::remove_file(self.default_path()?);
                let mut idts = self.list()?;
                idts.retain(|i| i.path != identity.path);
                if let Some(idt) = idts.first() {
                    println!("Setting default identity to `{}`", idt.name);
                    self.set_default(&idt.name)?;
                }
            }
        }

        // Remove identity file
        std::fs::remove_file(identity.path)?;

        Ok(())
    }

    pub fn default_path(&self) -> Result<PathBuf> {
        Ok(CliState::defaults_dir()?.join("identity"))
    }

    pub fn default(&self) -> Result<IdentityState> {
        let path = std::fs::canonicalize(self.default_path()?)?;
        let name = file_stem(&path)?;
        let contents = std::fs::read_to_string(&path)?;
        let config = serde_json::from_str(&contents)?;
        Ok(IdentityState { name, path, config })
    }

    pub fn set_default(&self, name: &str) -> Result<IdentityState> {
        let original = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("identity `{name}`")));
            }
            path
        };
        let link = self.default_path()?;
        std::os::unix::fs::symlink(original, link)?;
        self.get(name)
    }

    pub async fn authenticated_storage(&self) -> Result<LmdbStorage> {
        Ok(LmdbStorage::new(self.dir.join("data").join("authenticated_storage.lmdb")).await?)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IdentityState {
    pub name: String,
    pub path: PathBuf,
    pub config: IdentityConfig,
}

impl IdentityState {
    fn persist(&self) -> Result<()> {
        let contents = serde_json::to_string(&self.config)?;
        std::fs::write(&self.path, contents)?;
        Ok(())
    }

    fn in_use(&self) -> Result<()> {
        self.in_use_by(&CliState::new()?.nodes.list()?)
    }

    fn in_use_by(&self, nodes: &[NodeState]) -> Result<()> {
        for node in nodes {
            if node.config.identity_config()?.identifier == self.config.identifier {
                return Err(CliStateError::Invalid(format!(
                    "Can't delete identity '{}' because is currently in use by node '{}'",
                    &self.name, &node.config.name
                )));
            }
        }
        Ok(())
    }

    pub fn set_enrollment_status(&mut self) -> Result<()> {
        self.config.enrollment_status = Some(EnrollmentStatus::enrolled());
        self.persist()
    }
}

impl Display for IdentityState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Name: {}",
            self.path.as_path().file_stem().unwrap().to_str().unwrap()
        )?;
        writeln!(f, "State Path: {}", self.path.clone().to_str().unwrap())?;
        writeln!(f, "Config Identifier: {}", self.config.identifier)?;
        match &self.config.enrollment_status {
            Some(enrollment) => {
                writeln!(f, "Enrollment Status:")?;
                for line in enrollment.to_string().lines() {
                    writeln!(f, "{:2}{}", "", line)?;
                }
            }
            None => (),
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IdentityConfig {
    pub identifier: IdentityIdentifier,
    pub change_history: IdentityChangeHistory,
    pub enrollment_status: Option<EnrollmentStatus>,
}

impl IdentityConfig {
    pub async fn new(identity: &Identity<Vault, LmdbStorage>) -> Self {
        let identifier = identity.identifier().clone();
        let change_history = identity.change_history().await;
        Self {
            identifier,
            change_history,
            enrollment_status: None,
        }
    }

    pub async fn get(
        &self,
        ctx: &ockam::Context,
        vault: &Vault,
    ) -> Result<Identity<Vault, LmdbStorage>> {
        let data = self.change_history.export()?;
        let storage = CliState::new()?.identities.authenticated_storage().await?;
        Ok(
            Identity::import_ext(ctx, &data, &storage, &SecureChannelRegistry::new(), vault)
                .await?,
        )
    }
}

impl PartialEq for IdentityConfig {
    fn eq(&self, other: &Self) -> bool {
        self.identifier == other.identifier
            && self.change_history.compare(&other.change_history)
                == IdentityHistoryComparison::Equal
    }
}

impl Eq for IdentityConfig {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnrollmentStatus {
    pub is_enrolled: bool,
    pub created_at: SystemTime,
}

impl EnrollmentStatus {
    pub fn enrolled() -> EnrollmentStatus {
        EnrollmentStatus {
            is_enrolled: true,
            created_at: SystemTime::now(),
        }
    }
}

impl Display for EnrollmentStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.is_enrolled {
            writeln!(f, "Enrolled: yes")?;
        } else {
            writeln!(f, "Enrolled: no")?;
        }

        // match OffsetDateTime::from(self.created_at).format(&Iso8601::DEFAULT) {
        //     Ok(time_str) => writeln!(f, "Timestamp: {}", time_str)?,
        //     Err(err) => writeln!(
        //         f,
        //         "Error formatting OffsetDateTime as Iso8601 String: {}",
        //         err
        //     )?,
        // }

        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NodesState {
    pub dir: PathBuf,
}

impl NodesState {
    fn new(cli_path: &Path) -> Result<Self> {
        let dir = cli_path.join("nodes");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn default_path(&self) -> Result<PathBuf> {
        Ok(CliState::defaults_dir()?.join("node"))
    }

    pub fn default(&self) -> Result<NodeState> {
        let path = std::fs::canonicalize(self.default_path()?)?;
        let name = file_stem(&path)?;
        self.get(&name)
    }

    pub fn set_default(&self, name: &str) -> Result<NodeState> {
        let original = {
            let mut path = self.dir.clone();
            path.push(name);
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("node `{name}`")));
            }
            path
        };
        let link = self.default_path()?;
        std::os::unix::fs::symlink(original, link)?;
        self.get(name)
    }

    pub fn create(&self, name: &str, mut config: NodeConfig) -> Result<NodeState> {
        config.name = name.to_string();
        let path = {
            let mut path = self.dir.clone();
            path.push(name);
            std::fs::create_dir_all(&path)?;
            path
        };
        let state = NodeState::new(path, config);
        std::fs::write(state.path.join("version"), state.config.version.to_string())?;
        state.set_setup(&state.config.setup)?;
        std::fs::File::create(state.socket())?;
        std::fs::File::create(state.stdout_log())?;
        std::fs::File::create(state.stderr_log())?;
        std::os::unix::fs::symlink(
            &state.config.default_vault,
            state.path.join("default_vault"),
        )?;
        std::os::unix::fs::symlink(
            &state.config.default_identity,
            state.path.join("default_identity"),
        )?;
        if !self.default_path()?.exists() {
            self.set_default(name)?;
        }
        Ok(state)
    }

    pub fn list(&self) -> Result<Vec<NodeState>> {
        let mut nodes = vec![];
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().into_string().map_err(|_| {
                    CliStateError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "node's directory has an invalid name",
                    ))
                })?;
                nodes.push(self.get(&name)?);
            }
        }
        Ok(nodes)
    }

    pub fn get(&self, name: &str) -> Result<NodeState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(name);
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("node `{name}`")));
            }
            path
        };
        let config = NodeConfig::try_from(&path)?;
        Ok(NodeState::new(path, config))
    }

    pub fn delete(&self, name: &str, sigkill: bool) -> Result<()> {
        // Retrieve node. If doesn't exist do nothing.
        let node = match self.get(name) {
            Ok(node) => node,
            Err(CliStateError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };

        // Set default to another node if it's the default
        if let Ok(default) = self.default() {
            if default.path == node.path {
                let _ = std::fs::remove_file(self.default_path()?);
                let mut nodes = self.list()?;
                nodes.retain(|n| n.path != node.path);
                if let Some(node) = nodes.first() {
                    self.set_default(&node.config.name)?;
                }
            }
        }

        // Remove node directory
        node.delete(sigkill)?;

        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NodeState {
    pub path: PathBuf,
    pub config: NodeConfig,
}

impl NodeState {
    fn new(path: PathBuf, config: NodeConfig) -> Self {
        Self { path, config }
    }

    pub fn socket(&self) -> PathBuf {
        self.path.join("socket")
    }

    pub fn setup(&self) -> Result<NodeSetupConfig> {
        NodeSetupConfig::try_from(&self.path.join("setup.json"))
    }

    pub fn set_setup(&self, setup: &NodeSetupConfig) -> Result<()> {
        let contents = serde_json::to_string(setup)?;
        std::fs::write(self.path.join("setup.json"), contents)?;
        Ok(())
    }

    pub fn pid(&self) -> Result<Option<i32>> {
        let path = self.path.join("pid");
        if self.path.join("pid").exists() {
            let pid = std::fs::read_to_string(path)?
                .parse::<i32>()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(pid))
        } else {
            Ok(None)
        }
    }

    pub fn set_pid(&self, pid: i32) -> Result<()> {
        std::fs::write(self.path.join("pid"), pid.to_string())?;
        Ok(())
    }

    pub fn stdout_log(&self) -> PathBuf {
        self.path.join("stdout.log")
    }

    pub fn stderr_log(&self) -> PathBuf {
        self.path.join("stderr.log")
    }

    pub fn policies_storage_path(&self) -> PathBuf {
        self.path.join("policies_storage.lmdb")
    }

    pub fn kill_process(&self, sigkill: bool) -> Result<()> {
        if let Some(pid) = self.pid()? {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                if sigkill {
                    nix::sys::signal::Signal::SIGKILL
                } else {
                    nix::sys::signal::Signal::SIGTERM
                },
            )
            .map_err(|e| {
                CliStateError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to stop PID `{pid}` with error `{e}`"),
                ))
            })?;
            std::fs::remove_file(self.path.join("pid"))?;
        }
        Ok(())
    }

    fn delete(&self, sigkill: bool) -> Result<()> {
        self.kill_process(sigkill)?;
        std::fs::remove_dir_all(&self.path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NodeConfig {
    pub name: String,
    version: NodeConfigVersion,
    default_vault: PathBuf,
    default_identity: PathBuf,
    setup: NodeSetupConfig,
    // TODO
    // authorities: AuthoritiesConfig,
}

impl NodeConfig {
    pub fn try_default() -> Result<Self> {
        let cli_state = CliState::new()?;
        Ok(Self {
            name: random_name(),
            version: NodeConfigVersion::latest(),
            default_vault: cli_state.vaults.default()?.path,
            default_identity: cli_state.identities.default()?.path,
            setup: NodeSetupConfig::default(),
        })
    }

    pub async fn vault(&self) -> Result<Vault> {
        let path = std::fs::canonicalize(&self.default_vault)?;
        let config: VaultConfig = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        config.get().await
    }

    pub fn identity_config(&self) -> Result<IdentityConfig> {
        let path = std::fs::canonicalize(&self.default_identity)?;
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub async fn identity(&self, ctx: &ockam::Context) -> Result<Identity<Vault, LmdbStorage>> {
        let vault = self.vault().await?;
        let path = std::fs::canonicalize(&self.default_identity)?;
        let config: IdentityConfig = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        config.get(ctx, &vault).await
    }
}

impl TryFrom<&PathBuf> for NodeConfig {
    type Error = CliStateError;

    fn try_from(path: &PathBuf) -> std::result::Result<Self, Self::Error> {
        let name = {
            let err = || {
                CliStateError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "node's directory has an invalid name",
                ))
            };
            path.file_name()
                .ok_or_else(err)?
                .to_str()
                .ok_or_else(err)?
                .to_string()
        };
        Ok(Self {
            name,
            version: NodeConfigVersion::load(path)?,
            default_vault: std::fs::canonicalize(path.join("default_vault"))?,
            default_identity: std::fs::canonicalize(path.join("default_identity"))?,
            setup: NodeSetupConfig::try_from(&path.join("setup.json"))?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct NodeConfigBuilder {
    vault: Option<PathBuf>,
    identity: Option<PathBuf>,
}

impl NodeConfigBuilder {
    pub fn vault(mut self, path: PathBuf) -> Self {
        self.vault = Some(path);
        self
    }

    pub fn identity(mut self, path: PathBuf) -> Self {
        self.identity = Some(path);
        self
    }

    pub fn build(self, cli_state: &CliState) -> Result<NodeConfig> {
        let vault = match self.vault {
            Some(path) => path,
            None => cli_state.vaults.default()?.path,
        };
        let identity = match self.identity {
            Some(path) => path,
            None => cli_state.identities.default()?.path,
        };
        Ok(NodeConfig {
            default_vault: vault,
            default_identity: identity,
            ..NodeConfig::try_default()?
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeConfigVersion {
    V1,
}

impl NodeConfigVersion {
    fn latest() -> Self {
        Self::V1
    }

    fn load(_path: &Path) -> Result<Self> {
        Ok(Self::V1)
    }
}

impl Display for NodeConfigVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NodeConfigVersion::V1 => "1",
        })
    }
}

impl FromStr for NodeConfigVersion {
    type Err = CliStateError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "1" => Ok(Self::V1),
            _ => Err(CliStateError::InvalidVersion(s.to_string())),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, Eq, PartialEq)]
pub struct NodeSetupConfig {
    pub verbose: u8,
    transports: Vec<CreateTransportJson>,
    // TODO
    // secure_channels: ?,
    // inlets: ?,
    // outlets: ?,
    // services: ?,
}

impl NodeSetupConfig {
    pub fn set_verbose(mut self, verbose: u8) -> Self {
        self.verbose = verbose;
        self
    }

    pub fn default_tcp_listener(&self) -> Result<&CreateTransportJson> {
        self.transports
            .iter()
            .find(|t| t.tt == TransportType::Tcp && t.tm == TransportMode::Listen)
            .ok_or_else(|| CliStateError::NotFound("default tcp transport".to_string()))
    }

    pub fn add_transport(mut self, transport: CreateTransportJson) -> Self {
        self.transports.push(transport);
        self
    }
}

impl TryFrom<&PathBuf> for NodeSetupConfig {
    type Error = CliStateError;

    fn try_from(path: &PathBuf) -> std::result::Result<Self, Self::Error> {
        let contents = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&contents)?)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectsState {
    dir: PathBuf,
}

impl ProjectsState {
    fn new(cli_path: &Path) -> Result<Self> {
        let dir = cli_path.join("projects");
        std::fs::create_dir_all(dir.join("data"))?;
        Ok(Self { dir })
    }

    pub async fn create(&self, name: &str, config: Project<'_>) -> Result<ProjectState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if path.exists() {
                return Err(CliStateError::AlreadyExists(format!("project `{name}`")));
            }
            path
        };
        let contents = serde_json::to_string(&config)?;
        std::fs::write(&path, contents)?;
        if !self.default_path()?.exists() {
            self.set_default(name)?;
        }

        Ok(ProjectState { path })
    }

    pub fn get(&self, name: &str) -> Result<ProjectState> {
        let path = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("project `{name}`")));
            }
            path
        };
        Ok(ProjectState { path })
    }

    pub fn default_path(&self) -> Result<PathBuf> {
        Ok(CliState::defaults_dir()?.join("project"))
    }

    pub fn default(&self) -> Result<ProjectState> {
        let path = std::fs::canonicalize(self.default_path()?)?;
        Ok(ProjectState { path })
    }

    pub fn set_default(&self, name: &str) -> Result<ProjectState> {
        let original = {
            let mut path = self.dir.clone();
            path.push(format!("{}.json", name));
            if !path.exists() {
                return Err(CliStateError::NotFound(format!("project `{name}`")));
            }
            path
        };
        let link = self.default_path()?;
        std::os::unix::fs::symlink(original, link)?;
        self.get(name)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectState {
    pub path: PathBuf,
}

impl ProjectState {
    pub fn name(&self) -> Result<String> {
        self.path
            .file_stem()
            .and_then(|s| s.to_os_string().into_string().ok())
            .ok_or_else(|| {
                CliStateError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "failed to parse the project name",
                ))
            })
    }
}

pub fn random_name() -> String {
    hex::encode(random::<[u8; 4]>())
}

fn file_stem(path: &Path) -> Result<String> {
    path.file_stem()
        .ok_or_else(|| CliStateError::NotFound(format!("name for {path:?}")))?
        .to_str()
        .map(|name| name.to_string())
        .ok_or_else(|| CliStateError::NotFound(format!("name for {path:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // This tests way too many different things
    #[ockam_macros::test(crate = "ockam")]
    async fn integration(ctx: &mut ockam::Context) -> ockam::Result<()> {
        let sut = CliState::test()?;

        // Vaults
        let vault_name = {
            let name = hex::encode(rand::random::<[u8; 4]>());

            let path = VaultConfig::path(&name)?;
            let vault_storage = FileStorage::create(path.clone()).await?;
            Vault::new(Some(Arc::new(vault_storage)));

            let config = VaultConfig::from_name(&name)?;

            let state = sut.vaults.create(&name, config).await.unwrap();
            let got = sut.vaults.get(&name).unwrap();
            assert_eq!(got, state);

            let got = sut.vaults.default().unwrap();
            assert_eq!(got, state);

            name
        };

        // Identities
        let identity_name = {
            let name = hex::encode(rand::random::<[u8; 4]>());
            let vault_config = sut.vaults.get(&vault_name).unwrap().config;
            let vault = vault_config.get().await.unwrap();
            let identity =
                Identity::create_ext(ctx, &sut.identities.authenticated_storage().await?, &vault)
                    .await
                    .unwrap();
            let config = IdentityConfig::new(&identity).await;

            let state = sut.identities.create(&name, config).unwrap();
            let got = sut.identities.get(&name).unwrap();
            assert_eq!(got, state);

            let got = sut.identities.default().unwrap();
            assert_eq!(got, state);

            name
        };

        // Nodes
        let node_name = {
            let name = hex::encode(rand::random::<[u8; 4]>());
            let config = NodeConfig::try_default().unwrap();

            let state = sut.nodes.create(&name, config).unwrap();
            let got = sut.nodes.get(&name).unwrap();
            assert_eq!(got, state);

            let got = sut.nodes.default().unwrap();
            assert_eq!(got, state);

            name
        };

        // Check structure
        let mut expected_entries = vec![
            "vaults".to_string(),
            format!("vaults/{vault_name}.json"),
            "vaults/data".to_string(),
            format!("vaults/data/{vault_name}-storage.json"),
            "identities".to_string(),
            format!("identities/{identity_name}.json"),
            "identities/data/authenticated_storage.lmdb".to_string(),
            "nodes".to_string(),
            format!("nodes/{node_name}"),
            "projects".to_string(),
            "projects/data".to_string(),
            "defaults".to_string(),
            "defaults/vault".to_string(),
            "defaults/identity".to_string(),
            "defaults/node".to_string(),
        ];
        expected_entries.sort();
        let mut found_entries = vec![];
        sut.dir.read_dir().unwrap().for_each(|entry| {
            let entry = entry.unwrap();
            let dir_name = entry.file_name().into_string().unwrap();
            match dir_name.as_str() {
                "vaults" => {
                    assert!(entry.path().is_dir());
                    found_entries.push(dir_name.clone());
                    entry.path().read_dir().unwrap().for_each(|entry| {
                        let entry = entry.unwrap();
                        let entry_name = entry.file_name().into_string().unwrap();
                        found_entries.push(format!("{dir_name}/{entry_name}"));
                        if entry.path().is_dir() {
                            assert_eq!(entry_name, "data");
                            entry.path().read_dir().unwrap().for_each(|entry| {
                                let entry = entry.unwrap();
                                let file_name = entry.file_name().into_string().unwrap();
                                if !file_name.ends_with(".lock") {
                                    found_entries
                                        .push(format!("{dir_name}/{entry_name}/{file_name}"));
                                    assert_eq!(file_name, format!("{vault_name}-storage.json"));
                                }
                            });
                        } else {
                            assert_eq!(entry_name, format!("{vault_name}.json"));
                        }
                    });
                }
                "identities" => {
                    assert!(entry.path().is_dir());
                    found_entries.push(dir_name.clone());
                    entry.path().read_dir().unwrap().for_each(|entry| {
                        let entry = entry.unwrap();
                        let entry_name = entry.file_name().into_string().unwrap();
                        if entry.path().is_dir() {
                            assert_eq!(entry_name, "data");
                            entry.path().read_dir().unwrap().for_each(|entry| {
                                let entry = entry.unwrap();
                                let file_name = entry.file_name().into_string().unwrap();
                                if !file_name.ends_with("-lock") {
                                    found_entries
                                        .push(format!("{dir_name}/{entry_name}/{file_name}"));
                                    assert_eq!(file_name, format!("authenticated_storage.lmdb"));
                                }
                            })
                        } else {
                            assert!(entry.path().is_file());
                            let file_name = entry.file_name().into_string().unwrap();
                            found_entries.push(format!("{dir_name}/{file_name}"));
                        }
                    });
                }
                "nodes" => {
                    assert!(entry.path().is_dir());
                    found_entries.push(dir_name.clone());
                    entry.path().read_dir().unwrap().for_each(|entry| {
                        let entry = entry.unwrap();
                        assert!(entry.path().is_dir());
                        let file_name = entry.file_name().into_string().unwrap();
                        found_entries.push(format!("{dir_name}/{file_name}"));
                    });
                }
                "defaults" => {
                    assert!(entry.path().is_dir());
                    found_entries.push(dir_name.clone());
                    entry.path().read_dir().unwrap().for_each(|entry| {
                        let entry = entry.unwrap();
                        let entry_name = entry.file_name().into_string().unwrap();
                        found_entries.push(format!("{dir_name}/{entry_name}"));
                    });
                }
                "projects" => {
                    assert!(entry.path().is_dir());
                    found_entries.push(dir_name.clone());
                    entry.path().read_dir().unwrap().for_each(|entry| {
                        let entry = entry.unwrap();
                        assert!(entry.path().is_dir());
                        let file_name = entry.file_name().into_string().unwrap();
                        found_entries.push(format!("{dir_name}/{file_name}"));
                    });
                }
                _ => panic!("unexpected file"),
            }
        });
        found_entries.sort();
        assert_eq!(expected_entries, found_entries);
        ctx.stop().await?;
        Ok(())
    }
}
