# ShipForge Domain

ShipForge describes delivery from a local project to infrastructure or managed deployment services. This glossary defines the language used by requirements, code, logs, and the TUI.

## Language

**Project**:
A deployable product with a stable identity, described by one `shipforge.yaml` and containing one or more Components.
_Avoid_: App, repository

**Component**:
An independently buildable part of a Project, such as a frontend, backend, or worker.
_Avoid_: Destination, server

**Environment**:
A named release stage, such as staging or production, that gives each Component its Destination and deployment settings.
_Avoid_: Destination, server, environment variables

**Destination**:
A reusable, Project-independent connection identified by an immutable key and backed by versioned connection settings. Multiple Projects and Components may reference it.
_Avoid_: Environment, Component

**Deployment Marker**:
Static metadata at a Linux Component root that identifies its Project, Environment, Component, and generation for later safety checks. It is not read by application services and does not imply ongoing control of the Destination.

**Driver**:
An implementation of deployment behavior for a Destination kind, such as `linux-ssh` or `vercel`.
_Avoid_: Destination, provider account

**Deployment**:
A recorded attempt to deploy or roll back one or more Component Releases in an Environment, with a separate result for each selected Component.
_Avoid_: Job, publish, task

**Release**:
A versioned, immutable `tar.gz` package created exactly once by the core Packager for one Component, with a SHA-256 digest and minimal manifest. Drivers consume this Release and do not create a second core format.
_Avoid_: Artifact, Deployment, Environment snapshot, multi-Component Release

**Artifact**:
The single non-empty file or directory path produced by a Component build. Its type is detected automatically and the core Packager normalizes it into one Release before deployment. Artifact never names the packaged `tar.gz`.
_Avoid_: Release, archive, deployment package
_Avoid_: Bundle, binary

**Activation**:
The driver-specific operation that makes a Component Release current and activates its service.
_Avoid_: Publish, install

**Service Command**:
A Component-specific instruction for starting, updating, restoring, or stopping its service in an Environment. It is separate from local build instructions and reusable Destination connection settings.
_Avoid_: Build command, Deployment Driver

**Service Preset**:
A reusable starting point for a Component's Service Commands and health rules, such as systemd. A preset does not define a separate kind of Destination or Deployment.
_Avoid_: Driver, server type

**Environment Observation**:
The current observed Release, or absence of one, for each configured Component in an Environment.
_Avoid_: Release, local deployment intent

**Rollback**:
A user-requested Deployment linked to an earlier Deployment that activates a selected historical healthy Release, or restores `not_deployed`, for each selected Component. Automatic failure recovery is Compensation, not a Rollback Deployment.
_Avoid_: Undo, downgrade

**Compensation**:
Automatic recovery within a failed or cancelled Deployment that restores each changed Component to its observed pre-activation state.
_Avoid_: Rollback Deployment

**Shared Content**:
Environment data that persists across Releases, such as configuration, uploads, or logs.
_Avoid_: Release files

**Release Retention**:
The policy that removes eligible Component Releases while protecting the current, previous healthy, referenced, and in-progress versions.
_Avoid_: Cleanup
