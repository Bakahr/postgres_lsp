use crate::diagnostics::LspError;
use crate::documents::Document;
use crate::utils;
use anyhow::Result;
use biome_deserialize::Merge;
use pg_configuration::{ConfigurationPathHint, PartialConfiguration};
use pg_console::markup;
use pg_diagnostics::{DiagnosticExt, Error, PrintDescription};
use pg_fs::{PgLspPath, FileSystem};
use pg_lsp_converters::{negotiated_encoding, PositionEncoding, WideEncoding};
use pg_workspace_new::configuration::{
    load_configuration, LoadedConfiguration,
};
use pg_workspace_new::settings::Settings;
use pg_workspace_new::workspace::UpdateSettingsParams;
use pg_workspace_new::Workspace;
use pg_workspace_new::{DynRef, WorkspaceError};
use futures::stream::futures_unordered::FuturesUnordered;
use futures::StreamExt;
use rustc_hash::FxHashMap;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Notify;
use tokio::sync::OnceCell;
use tower_lsp::lsp_types;
use tower_lsp::lsp_types::{Diagnostic, Url};
use tower_lsp::lsp_types::{MessageType, Registration};
use tower_lsp::lsp_types::{Unregistration, WorkspaceFolder};
use tracing::{error, info};

pub(crate) struct ClientInformation {
    /// The name of the client
    pub(crate) name: String,

    /// The version of the client
    pub(crate) version: Option<String>,
}

/// Key, uniquely identifying a LSP session.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub(crate) struct SessionKey(pub u64);

/// Represents the state of an LSP server session.
pub(crate) struct Session {
    /// The unique key identifying this session.
    pub(crate) key: SessionKey,

    /// The LSP client for this session.
    pub(crate) client: tower_lsp::Client,

    /// The parameters provided by the client in the "initialize" request
    initialize_params: OnceCell<InitializeParams>,

    pub(crate) workspace: Arc<dyn Workspace>,

    configuration_status: AtomicU8,

    /// A flag to notify a message to the user when the configuration is broken, and the LSP attempts
    /// to update the diagnostics
    notified_broken_configuration: AtomicBool,

    /// File system to read files inside the workspace
    pub(crate) fs: DynRef<'static, dyn FileSystem>,

    documents: RwLock<FxHashMap<lsp_types::Url, Document>>,

    pub(crate) cancellation: Arc<Notify>,

    pub(crate) config_path: Option<PathBuf>,
}

/// The parameters provided by the client in the "initialize" request
struct InitializeParams {
    /// The capabilities provided by the client as part of [`lsp_types::InitializeParams`]
    client_capabilities: lsp_types::ClientCapabilities,
    client_information: Option<ClientInformation>,
    root_uri: Option<Url>,
    #[allow(unused)]
    workspace_folders: Option<Vec<WorkspaceFolder>>,
}

#[repr(u8)]
pub(crate) enum ConfigurationStatus {
    /// The configuration file was properly loaded
    Loaded = 0,
    /// The configuration file does not exist
    Missing = 1,
    /// The configuration file exists but could not be loaded
    Error = 2,
    /// Currently loading the configuration
    Loading = 3,
}

impl ConfigurationStatus {
    pub(crate) const fn is_error(&self) -> bool {
        matches!(self, ConfigurationStatus::Error)
    }

    pub(crate) const fn is_loaded(&self) -> bool {
        matches!(self, ConfigurationStatus::Loaded)
    }
}

impl TryFrom<u8> for ConfigurationStatus {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, ()> {
        match value {
            0 => Ok(Self::Loaded),
            1 => Ok(Self::Missing),
            2 => Ok(Self::Error),
            3 => Ok(Self::Loading),
            _ => Err(()),
        }
    }
}

pub(crate) type SessionHandle = Arc<Session>;

/// Holds the set of capabilities supported by the Language Server
/// instance and whether they are enabled or not
#[derive(Default)]
pub(crate) struct CapabilitySet {
    registry: FxHashMap<&'static str, (&'static str, CapabilityStatus)>,
}

/// Represents whether a capability is enabled or not, optionally holding the
/// configuration associated with the capability
pub(crate) enum CapabilityStatus {
    Enable(Option<Value>),
    Disable,
}

impl CapabilitySet {
    /// Insert a capability in the set
    pub(crate) fn add_capability(
        &mut self,
        id: &'static str,
        method: &'static str,
        status: CapabilityStatus,
    ) {
        self.registry.insert(id, (method, status));
    }
}

impl Session {
    pub(crate) fn new(
        key: SessionKey,
        client: tower_lsp::Client,
        workspace: Arc<dyn Workspace>,
        cancellation: Arc<Notify>,
        fs: DynRef<'static, dyn FileSystem>,
    ) -> Self {
        let documents = Default::default();
        Self {
            key,
            client,
            initialize_params: OnceCell::default(),
            workspace,
            configuration_status: AtomicU8::new(ConfigurationStatus::Missing as u8),
            documents,
            fs,
            cancellation,
            config_path: None,
            notified_broken_configuration: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_config_path(&mut self, path: PathBuf) {
        self.config_path = Some(path);
    }

    /// Initialize this session instance with the incoming initialization parameters from the client
    pub(crate) fn initialize(
        &self,
        client_capabilities: lsp_types::ClientCapabilities,
        client_information: Option<ClientInformation>,
        root_uri: Option<Url>,
        workspace_folders: Option<Vec<WorkspaceFolder>>,
    ) {
        let result = self.initialize_params.set(InitializeParams {
            client_capabilities,
            client_information,
            root_uri,
            workspace_folders,
        });

        if let Err(err) = result {
            error!("Failed to initialize session: {err}");
        }
    }

    /// Register a set of capabilities with the client
    pub(crate) async fn register_capabilities(&self, capabilities: CapabilitySet) {
        let mut registrations = Vec::new();
        let mut unregistrations = Vec::new();

        let mut register_methods = String::new();
        let mut unregister_methods = String::new();

        for (id, (method, status)) in capabilities.registry {
            unregistrations.push(Unregistration {
                id: id.to_string(),
                method: method.to_string(),
            });

            if !unregister_methods.is_empty() {
                unregister_methods.push_str(", ");
            }

            unregister_methods.push_str(method);

            if let CapabilityStatus::Enable(register_options) = status {
                registrations.push(Registration {
                    id: id.to_string(),
                    method: method.to_string(),
                    register_options,
                });

                if !register_methods.is_empty() {
                    register_methods.push_str(", ");
                }

                register_methods.push_str(method);
            }
        }

        if let Err(e) = self.client.unregister_capability(unregistrations).await {
            error!(
                "Error unregistering {unregister_methods:?} capabilities: {}",
                e
            );
        } else {
            info!("Unregister capabilities {unregister_methods:?}");
        }

        if let Err(e) = self.client.register_capability(registrations).await {
            error!("Error registering {register_methods:?} capabilities: {}", e);
        } else {
            info!("Register capabilities {register_methods:?}");
        }
    }

    /// Get a [`Document`] matching the provided [`lsp_types::Url`]
    ///
    /// If document does not exist, result is [WorkspaceError::NotFound]
    pub(crate) fn document(&self, url: &lsp_types::Url) -> Result<Document, Error> {
        self.documents
            .read()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| WorkspaceError::not_found().with_file_path(url.to_string()))
    }

    /// Set the [`Document`] for the provided [`lsp_types::Url`]
    ///
    /// Used by [`handlers::text_document] to synchronize documents with the client.
    pub(crate) fn insert_document(&self, url: lsp_types::Url, document: Document) {
        self.documents.write().unwrap().insert(url, document);
    }

    /// Remove the [`Document`] matching the provided [`lsp_types::Url`]
    pub(crate) fn remove_document(&self, url: &lsp_types::Url) {
        self.documents.write().unwrap().remove(url);
    }

    pub(crate) fn file_path(&self, url: &lsp_types::Url) -> Result<PgLspPath> {
        let path_to_file = match url.to_file_path() {
            Err(_) => {
                // If we can't create a path, it's probably because the file doesn't exist.
                // It can be a newly created file that it's not on disk
                PathBuf::from(url.path())
            }
            Ok(path) => path,
        };

        Ok(PgLspPath::new(path_to_file))
    }

    /// True if the client supports dynamic registration of "workspace/didChangeConfiguration" requests
    pub(crate) fn can_register_did_change_configuration(&self) -> bool {
        self.initialize_params
            .get()
            .and_then(|c| c.client_capabilities.workspace.as_ref())
            .and_then(|c| c.did_change_configuration)
            .and_then(|c| c.dynamic_registration)
            == Some(true)
    }

    /// Get the current workspace folders
    pub(crate) fn get_workspace_folders(&self) -> Option<&Vec<WorkspaceFolder>> {
        self.initialize_params
            .get()
            .and_then(|c| c.workspace_folders.as_ref())
    }

    /// Returns the base path of the workspace on the filesystem if it has one
    pub(crate) fn base_path(&self) -> Option<PathBuf> {
        let initialize_params = self.initialize_params.get()?;

        let root_uri = initialize_params.root_uri.as_ref()?;
        match root_uri.to_file_path() {
            Ok(base_path) => Some(base_path),
            Err(()) => {
                error!(
                    "The Workspace root URI {root_uri:?} could not be parsed as a filesystem path"
                );
                None
            }
        }
    }

    /// Returns a reference to the client information for this session
    pub(crate) fn client_information(&self) -> Option<&ClientInformation> {
        self.initialize_params.get()?.client_information.as_ref()
    }

    /// This function attempts to read the `pglsp.toml` configuration file from
    /// the root URI and update the workspace settings accordingly
    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) async fn load_workspace_settings(&self) {
        // Providing a custom configuration path will not allow to support workspaces
        if let Some(config_path) = &self.config_path {
            let base_path = ConfigurationPathHint::FromUser(config_path.clone());
            let status = self.load_pglsp_configuration_file(base_path).await;
            self.set_configuration_status(status);
        } else if let Some(folders) = self.get_workspace_folders() {
            info!("Detected workspace folder.");
            self.set_configuration_status(ConfigurationStatus::Loading);
            for folder in folders {
                info!("Attempt to load the configuration file in {:?}", folder.uri);
                let base_path = folder.uri.to_file_path();
                match base_path {
                    Ok(base_path) => {
                        let status = self
                            .load_pglsp_configuration_file(ConfigurationPathHint::FromWorkspace(
                                base_path,
                            ))
                            .await;
                        self.set_configuration_status(status);
                    }
                    Err(_) => {
                        error!(
                            "The Workspace root URI {:?} could not be parsed as a filesystem path",
                            folder.uri
                        );
                    }
                }
            }
        } else {
            let base_path = match self.base_path() {
                None => ConfigurationPathHint::default(),
                Some(path) => ConfigurationPathHint::FromLsp(path),
            };
            let status = self.load_pglsp_configuration_file(base_path).await;
            self.set_configuration_status(status);
        }
    }

    async fn load_pglsp_configuration_file(
        &self,
        base_path: ConfigurationPathHint,
    ) -> ConfigurationStatus {
        match load_configuration(&self.fs, base_path.clone()) {
            Ok(loaded_configuration) => {
                    let LoadedConfiguration {
                        configuration: fs_configuration,
                        directory_path: configuration_path,
                        ..
                    } = loaded_configuration;
                    info!("Configuration loaded successfully from disk.");
                    info!("Update workspace settings.");

                    let fs = &self.fs;
                    let mut configuration = PartialConfiguration::default();

                    configuration.merge_with(fs_configuration);

                    let result =
                        configuration.retrieve_gitignore_matches(fs, configuration_path.as_deref());

                    match result {
                        Ok((vcs_base_path, gitignore_matches)) => {
                            let register_result =
                                if let ConfigurationPathHint::FromWorkspace(path) = &base_path {
                                    // We don't need the key
                                    self.workspace
                                        .register_project_folder(RegisterProjectFolderParams {
                                            path: Some(path.clone()),
                                            // This is naive, but we don't know if the user has a file already open or not, so we register every project as the current one.
                                            // The correct one is actually set when the LSP calls `textDocument/didOpen`
                                            set_as_current_workspace: true,
                                        })
                                        .err()
                                } else {
                                    self.workspace
                                        .register_project_folder(RegisterProjectFolderParams {
                                            path: fs.working_directory(),
                                            set_as_current_workspace: true,
                                        })
                                        .err()
                                };
                            if let Some(error) = register_result {
                                error!("Failed to register the project folder: {}", error);
                                self.client.log_message(MessageType::ERROR, &error).await;
                                return ConfigurationStatus::Error;
                            }
                            let result = self.workspace.update_settings(UpdateSettingsParams {
                                workspace_directory: fs.working_directory(),
                                configuration,
                            });

                            if let Err(error) = result {
                                error!("Failed to set workspace settings: {}", error);
                                self.client.log_message(MessageType::ERROR, &error).await;
                                ConfigurationStatus::Error
                            } else {
                                ConfigurationStatus::Loaded
                            }
                        }
                        Err(err) => {
                            error!("Couldn't load the configuration file, reason:\n {}", err);
                            self.client.log_message(MessageType::ERROR, &err).await;
                            ConfigurationStatus::Error
                        }
                    }
            }

            Err(err) => {
                error!("Couldn't load the configuration file, reason:\n {}", err);
                self.client.log_message(MessageType::ERROR, &err).await;
                ConfigurationStatus::Error
            }
        }
    }

    /// Broadcast a shutdown signal to all active connections
    pub(crate) fn broadcast_shutdown(&self) {
        self.cancellation.notify_one();
    }

    /// Retrieves information regarding the configuration status
    pub(crate) fn configuration_status(&self) -> ConfigurationStatus {
        self.configuration_status
            .load(Ordering::Relaxed)
            .try_into()
            .unwrap()
    }

    /// Updates the status of the configuration
    fn set_configuration_status(&self, status: ConfigurationStatus) {
        self.notified_broken_configuration
            .store(false, Ordering::Relaxed);
        self.configuration_status
            .store(status as u8, Ordering::Relaxed);
    }

    fn notified_broken_configuration(&self) -> bool {
        self.notified_broken_configuration.load(Ordering::Relaxed)
    }
    fn set_notified_broken_configuration(&self) {
        self.notified_broken_configuration
            .store(true, Ordering::Relaxed);
    }

    pub fn position_encoding(&self) -> PositionEncoding {
        self.initialize_params
            .get()
            .map_or(PositionEncoding::Wide(WideEncoding::Utf16), |params| {
                negotiated_encoding(&params.client_capabilities)
            })
    }
}

