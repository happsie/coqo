// SPDX-License-Identifier: Apache-2.0

//! Building the JSON-RPC errors the workspace methods return.

use std::io;
use std::path::Path;

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_storage::StorageError;

pub(crate) fn unknown_workspace(id: &WorkspaceId) -> ErrorObject {
    ErrorObject::new(codes::UNKNOWN_WORKSPACE, format!("unknown workspace {id}"))
}

pub(crate) fn unknown_root(id: &WorkspaceId, root: RootId) -> ErrorObject {
    ErrorObject::new(
        codes::UNKNOWN_ROOT,
        format!("workspace {id} has no root {root}"),
    )
}

pub(crate) fn outside(path: &str) -> ErrorObject {
    ErrorObject::new(
        codes::PATH_OUTSIDE_WORKSPACE,
        format!("{path:?} is not a path inside the workspace"),
    )
}

pub(crate) fn not_a_directory(path: &str) -> ErrorObject {
    ErrorObject::new(
        codes::NOT_A_DIRECTORY,
        format!("{path:?} is not a directory"),
    )
}

pub(crate) fn not_a_file(path: &str) -> ErrorObject {
    ErrorObject::new(codes::NOT_A_FILE, format!("{path:?} is not a regular file"))
}

pub(crate) fn exists(path: &str) -> ErrorObject {
    ErrorObject::new(codes::ALREADY_EXISTS, format!("{path:?} already exists"))
}

pub(crate) fn closed(id: &WorkspaceId) -> ErrorObject {
    ErrorObject::new(
        codes::WORKSPACE_CLOSED,
        format!("workspace {id} is not open"),
    )
}

/// An I/O error on `path` (as the client named it).
pub(crate) fn io(path: impl AsRef<Path>, e: &io::Error) -> ErrorObject {
    let path = path.as_ref().display();
    match e.kind() {
        io::ErrorKind::NotFound => ErrorObject::new(codes::NOT_FOUND, format!("{path}: not found")),
        io::ErrorKind::NotADirectory => {
            ErrorObject::new(codes::NOT_A_DIRECTORY, format!("{path}: not a directory"))
        }
        io::ErrorKind::AlreadyExists => {
            ErrorObject::new(codes::ALREADY_EXISTS, format!("{path} already exists"))
        }
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => ErrorObject::new(
            codes::PERMISSION_DENIED,
            format!("{path}: permission denied"),
        ),
        _ => ErrorObject::new(codes::IO_ERROR, format!("{path}: {e}")),
    }
}

pub(crate) fn storage(e: impl Into<StorageError>) -> ErrorObject {
    let e = e.into();
    tracing::error!(error = %e, "workspace storage error");
    ErrorObject::internal(format!("storage error: {e}"))
}

pub(crate) fn join(e: tokio::task::JoinError) -> ErrorObject {
    ErrorObject::internal(format!("workspace task failed: {e}"))
}
