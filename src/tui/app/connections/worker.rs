use std::{fs::File, io::Read, path::Path, time::Duration};

use crate::{
    application::{ConnectionCredentialDraft, DestinationSetupService, SshConnectionDraft},
    config::{DestinationSettings, SshCredential},
};

use super::{
    Arc, CancellationToken, ConnectionDetails, ConnectionForm, ConnectionManagementService,
    ConnectionsPage, ConnectionsRequest, CredentialChoice, KeyDirectory, KeyEntry, SshField,
};

const MAX_CHOICES: usize = 1000;
const MAX_DISCOVERY_BYTES: u64 = 1024 * 1024;

pub(super) async fn run(
    service: &ConnectionManagementService,
    setup: &DestinationSetupService,
    home: Option<&Path>,
    request: ConnectionsRequest,
    cancellation: &CancellationToken,
) -> Result<ConnectionsPage, String> {
    cancelled(cancellation)?;
    match request {
        ConnectionsRequest::List => list(service),
        ConnectionsRequest::Form(key) => {
            let existing = key
                .map(|key| {
                    service.list_connections().map_err(error).and_then(|items| {
                        items
                            .into_iter()
                            .find(|item| item.key == key)
                            .ok_or_else(|| "Connection no longer exists.".into())
                    })
                })
                .transpose()?;
            let form = load_form(service, setup, home, existing, cancellation).await?;
            Ok(ConnectionsPage::Form(Arc::new(form)))
        }
        ConnectionsRequest::Keys { form, path } => Ok(ConnectionsPage::Keys {
            form,
            directory: Arc::new(read_directory(&path)?),
            cursor: 0,
        }),
        ConnectionsRequest::SelectKey { form, path } => select_key(form, &path),
        ConnectionsRequest::Capture(form) => {
            let draft = draft(&form)?;
            let preview = if let Some(existing) = &form.existing {
                service.preview_edit(&existing.key, draft)
            } else {
                service.preview_create(draft)
            }
            .map_err(error)?;
            if preview.details() != form.existing.as_ref() {
                return Err(
                    "Saved connection changed while editing; reload the form before confirming."
                        .into(),
                );
            }
            let confirmation = service
                .capture_identity(preview, cancellation)
                .await
                .map_err(error)?;
            Ok(ConnectionsPage::HostKey {
                form,
                confirmation: Arc::new(confirmation),
            })
        }
        ConnectionsRequest::Save(confirmation) => {
            let connection = service
                .confirm_and_save((*confirmation).clone(), cancellation)
                .await
                .map_err(error)?;
            Ok(ConnectionsPage::Detail { connection: Arc::new(connection), notice: Some("Connection saved after confirmed host-key authentication. No Project or remote deployment was changed.".into()) })
        }
        ConnectionsRequest::Verify(connection) => {
            require_current(service, &connection)?;
            service
                .verify_saved_connection(&connection.key, cancellation)
                .await
                .map_err(error)?;
            require_current(service, &connection)?;
            Ok(ConnectionsPage::Detail { connection, notice: Some("Saved host-key pin and credential verified. Read-only connection check; not a deployment or service-health result.".into()) })
        }
        ConnectionsRequest::RemovePreview(key) => service
            .preview_destination_removal(&key)
            .map(|preview| ConnectionsPage::Remove(Arc::new(preview)))
            .map_err(error),
        ConnectionsRequest::Remove(preview) => {
            service
                .remove_destination_with_cancellation((*preview).clone(), cancellation)
                .await
                .map_err(error)?;
            list(service).map_err(|_| "Connection registration was removed, but refreshing the list failed; reload current connections.".into())
        }
        ConnectionsRequest::ProjectPreview(root) => service
            .preview_project_removal(&root)
            .map(|preview| ConnectionsPage::ProjectRemove(Arc::new(preview)))
            .map_err(error),
        ConnectionsRequest::ProjectRemove(preview) => {
            service
                .remove_project_with_cancellation((*preview).clone(), cancellation)
                .await
                .map_err(error)?;
            service.list_projects().map(|projects| ConnectionsPage::ProjectRemoved(Arc::new(projects)))
                .map_err(|_| "Project was removed from recents, but refreshing recents failed. Project files and history were not deleted.".into())
        }
    }
}

fn require_current(
    service: &ConnectionManagementService,
    connection: &ConnectionDetails,
) -> Result<(), String> {
    if service
        .list_connections()
        .map_err(error)?
        .iter()
        .any(|current| current == connection)
    {
        Ok(())
    } else {
        Err("Saved connection changed; refresh details before verifying it.".into())
    }
}

fn list(service: &ConnectionManagementService) -> Result<ConnectionsPage, String> {
    let items = service.list_connections().map_err(error)?;
    if items.len() > MAX_CHOICES {
        return Err("Connection list exceeds the bounded display limit.".into());
    }
    Ok(ConnectionsPage::List {
        items: Arc::new(items),
        cursor: 0,
    })
}

async fn load_form(
    service: &ConnectionManagementService,
    setup: &DestinationSetupService,
    home: Option<&Path>,
    existing: Option<ConnectionDetails>,
    cancellation: &CancellationToken,
) -> Result<ConnectionForm, String> {
    let mut credentials = service
        .list_credentials()
        .map_err(error)?
        .into_iter()
        .map(|summary| CredentialChoice::Saved {
            handle: summary.handle,
            label: format!(
                "Saved · {}{}",
                summary.label,
                if summary.available {
                    ""
                } else {
                    " (unavailable)"
                }
            ),
        })
        .collect::<Vec<_>>();
    let mut notices = Vec::new();
    match tokio::time::timeout(
        Duration::from_secs(10),
        setup.discover_local_identities(cancellation),
    )
    .await
    {
        Ok(Ok(identities)) => {
            if identities.len() > MAX_CHOICES {
                return Err("SSH Agent returned too many identities.".into());
            }
            credentials.extend(
                identities
                    .into_iter()
                    .map(|identity| CredentialChoice::Agent {
                        fingerprint: identity.reference,
                        label: identity.label,
                    }),
            );
        }
        _ => notices.push(
            "SSH Agent identities unavailable; choose a saved identity or browse a key.".into(),
        ),
    }
    cancelled(cancellation)?;
    let hosts = home.map_or_else(Vec::new, |home| {
        discovery(home, &mut credentials, &mut notices)
    });
    super::super::deduplicate_credentials(&mut credentials);
    if credentials.len() > MAX_CHOICES || hosts.len() > MAX_CHOICES {
        return Err("Local connection discovery exceeds the bounded choice limit.".into());
    }
    let mut form = ConnectionForm {
        existing,
        host: String::new(),
        user: String::new(),
        port: "22".into(),
        field: SshField::Host,
        credentials,
        credential_cursor: 0,
        hosts,
        host_cursor: 0,
        notices,
    };
    if let Some(connection) = &form.existing {
        let DestinationSettings::LinuxSsh {
            host,
            user,
            port,
            credential,
            ..
        } = &connection.current.settings;
        form.host.clone_from(host);
        form.user.clone_from(user);
        form.port = port.to_string();
        form.credential_cursor = form.credentials.iter().position(|choice| matches!(choice, CredentialChoice::Saved { handle, .. } if handle == credential)).unwrap_or(0);
        if !form.credentials.iter().any(|choice| matches!(choice, CredentialChoice::Saved { handle, .. } if handle == credential)) {
            form.notices.push("The prior identity is missing; choose another identity explicitly before saving.".into());
            form.credential_cursor = form.credentials.len();
        }
    } else {
        super::next_host(&mut form);
    }
    Ok(form)
}

fn discovery(
    home: &Path,
    credentials: &mut Vec<CredentialChoice>,
    notices: &mut Vec<String>,
) -> Vec<super::SshCandidate> {
    let path = home.join(".ssh").join("config");
    let mut bytes = Vec::new();
    let contents = match File::open(&path)
        .and_then(|file| file.take(MAX_DISCOVERY_BYTES + 1).read_to_end(&mut bytes))
    {
        Ok(_) if bytes.len() as u64 <= MAX_DISCOVERY_BYTES => String::from_utf8(bytes).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        _ => None,
    };
    let hosts = contents.map_or_else(
        || {
            notices.push(
                "Local SSH host suggestions are unavailable or exceed the bounded file limit."
                    .into(),
            );
            Vec::new()
        },
        |contents| crate::config::discover_ssh_candidates(&contents),
    );
    let candidates = hosts
        .iter()
        .flat_map(|host| host.identity_files.iter().cloned())
        .chain([home.join(".ssh/id_ed25519"), home.join(".ssh/id_ecdsa")]);
    for path in candidates.take(MAX_CHOICES + 1) {
        let path = if path.is_absolute() {
            path
        } else {
            let text = path.to_string_lossy();
            if let Some(relative) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
                home.join(relative)
            } else {
                continue;
            }
        };
        if path.is_file() {
            credentials.push(CredentialChoice::IdentityFile {
                label: file_label(&path),
                path,
            });
        }
    }
    hosts
}

fn draft(form: &ConnectionForm) -> Result<SshConnectionDraft, String> {
    let choice = form
        .credentials
        .get(form.credential_cursor)
        .ok_or_else(|| "Select an SSH identity; F3 opens the private-key browser.".to_owned())?;
    let credential = match choice {
        CredentialChoice::Password(input) => {
            ConnectionCredentialDraft::New(SshCredential::Password {
                protected: input.protect().map_err(str::to_owned)?,
            })
        }
        CredentialChoice::Saved { handle, .. } => ConnectionCredentialDraft::Saved(handle.clone()),
        CredentialChoice::Agent { fingerprint, .. } => {
            ConnectionCredentialDraft::New(SshCredential::Agent {
                fingerprint: fingerprint.clone(),
            })
        }
        CredentialChoice::IdentityFile { path, .. } => {
            ConnectionCredentialDraft::New(SshCredential::IdentityFile { path: path.clone() })
        }
    };
    Ok(SshConnectionDraft {
        host: form.host.clone(),
        user: form.user.clone(),
        port: form
            .port
            .parse()
            .map_err(|_| "SSH port must be between 1 and 65535.".to_owned())?,
        credential,
    })
}

fn select_key(mut form: Arc<ConnectionForm>, path: &Path) -> Result<ConnectionsPage, String> {
    let path = path
        .canonicalize()
        .map_err(|_| "Selected identity cannot be accessed.".to_owned())?;
    if !path.is_file() {
        return Err("Select a regular private-key file.".into());
    }
    let form_mut = Arc::make_mut(&mut form);
    if form_mut.credentials.len() >= MAX_CHOICES {
        return Err("Too many identity choices; choose an existing saved reference.".into());
    }
    let choice = CredentialChoice::IdentityFile {
        label: file_label(&path),
        path,
    };
    let identity = choice.identity();
    if let Some(index) = form_mut
        .credentials
        .iter()
        .position(|item| item.identity() == identity)
    {
        form_mut.credential_cursor = index;
    } else {
        form_mut.credentials.push(choice);
        form_mut.credential_cursor = form_mut.credentials.len() - 1;
    }
    form_mut.field = SshField::Credential;
    Ok(ConnectionsPage::Form(form))
}

fn read_directory(path: &Path) -> Result<KeyDirectory, String> {
    let path = path
        .canonicalize()
        .map_err(|_| "Cannot open the selected key directory.".to_owned())?;
    let listing = std::fs::read_dir(&path)
        .map_err(|_| "Cannot read the selected key directory.".to_owned())?;
    let mut entries = Vec::new();
    for item in listing.take(MAX_CHOICES + 1) {
        let item = item.map_err(|_| "Some key-directory entries could not be read.".to_owned())?;
        let metadata = item
            .file_type()
            .map_err(|_| "Some key-directory entries could not be inspected.".to_owned())?;
        entries.push(KeyEntry {
            path: item.path(),
            directory: metadata.is_dir(),
        });
    }
    if entries.len() > MAX_CHOICES {
        return Err("Key directory has more than 1000 entries; choose a smaller directory.".into());
    }
    entries
        .sort_by(|left, right| (!left.directory, &left.path).cmp(&(!right.directory, &right.path)));
    Ok(KeyDirectory { path, entries })
}

fn file_label(path: &Path) -> String {
    format!(
        "Identity file · {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

fn cancelled(cancellation: &CancellationToken) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("Operation cancelled before starting; nothing was saved.".into())
    } else {
        Ok(())
    }
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
