# Building SpotiPod

This guide explains how to set up the development environment, run SpotiPod, and build the GUI executable from source.

> **Current verification status:** SpotiPod has been run locally on macOS using the development command below. A release build has not yet been verified, and Windows/Linux builds have not been tested. Cross-platform support should therefore be considered unverified.

## Prerequisites

- **macOS** — the currently tested development platform.
- **Rust stable toolchain** with Cargo. The project uses Rust Edition 2024, which requires Rust 1.85.0 or newer.
- **Xcode Command Line Tools**, including the macOS compiler and linker tools.

Install the Xcode Command Line Tools if they are not already installed:

```bash
xcode-select --install
```

Check that the tools are available:
```bash
xcode-select -p
```

Install Rust using the official rustup installer if needed. Then verify the installation:
```bash
rustc --version
cargo --version
```

Make sure the reported Rust version is at least 1.85.0.

## Clone the repository

```bash
git clone https://github.com/AsQqqq/SpotiPOD.git
cd SpotiPOD
```

The repository is a Cargo workspace containing two crates:
```
├── Cargo.toml
├── Cargo.lock
├── apps/
│   └── spotipod-gui/
│       ├── Cargo.toml
│       ├── src/
│       └── ui/
└── crates/
    └── spotipod-core/
        ├── Cargo.toml
        ├── build.rs
        ├── proto/
        └── src/
```

- spotipod-core contains the application core, including Spotify connectivity and playback-related functionality.
- spotipod-gui provides the desktop GUI, built with Tao and Wry.

## Build and run in development

From the repository root, run:
```bash
cargo run -p spotipod-gui
```

This builds the required workspace dependencies and launches the GUI application.

To check that the workspace compiles without launching the application:
```bash
cargo check --workspace
```

## Build a release executable

To create an optimized release build of the GUI target:
```bash
cargo build --release -p spotipod-gui
```

The executable is expected at:
```bash
target/release/spotipod-gui
```

The workspace defines a release profile with thin link-time optimization, a single codegen unit, and symbol stripping.

Note: A release build has not yet been verified by the project maintainer. This command builds the Rust executable; it does not by itself create a signed, notarized, or packaged macOS .app bundle or installer.

## GitHub Actions draft releases

The `.github/workflows/release.yml` workflow runs only when a commit is pushed to `main`. It reads the release version from `[workspace.package].version` in the root `Cargo.toml`, and reads the enabled build platforms from `release.toml`:

```toml
[platforms]
macos = true
windows = false
linux = false
```

Set a platform to `true` to build it. macOS produces a universal Intel/Apple Silicon executable archive; Windows and Linux produce x64 executable archives. The workflow skips the build if the configured version matches the latest published (non-draft, non-prerelease) GitHub release.

After all selected platform builds succeed, the workflow creates a Git tag named exactly after the configured version and attaches the archives to a **draft** GitHub release. Review and publish that draft manually. These archives are unsigned executables, not platform installers. Windows and Linux builds are not yet verified by the project maintainer.

### In-app updates

On macOS, SpotiPod checks GitHub for the latest published stable release at startup. When it finds a newer version, an **Update** item appears at the top of the main menu. You can also check from **Settings → Check for Updates**. Choosing **Install Update** downloads the macOS universal archive, verifies its GitHub SHA-256 digest when available, and restarts SpotiPod with the new executable. The directory containing the running executable must be writable. Draft releases are not visible to the updater; publish the draft before users can receive it. In-app installation is currently macOS-only; Windows and Linux support is on the README roadmap.

## Reproducible builds with Cargo.lock

The repository includes Cargo.lock. To ask Cargo to use the locked dependency versions and fail if the lockfile would need updating, add --locked:
```bash
cargo check --workspace --locked
cargo run --locked -p spotipod-gui
cargo build --release --locked -p spotipod-gui
```

## Cleaning build artifacts

To remove generated build artifacts:
```bash
cargo clean
```

Cargo will recreate the target/ directory on the next build.

## Platform notes

### macOS
macOS is the only platform currently tested locally. Wry uses the native WebKit webview on macOS, so a separate WebView runtime installation is generally not required.

### Windows and Linux
The project uses Tao and Wry, which are designed for cross-platform desktop applications. However, SpotiPod has not been tested on Windows or Linux. Additional platform-specific dependencies, configuration, or code changes may be needed before those builds work.

## Troubleshooting

- cargo or rustc is not found: Install Rust with rustup and ensure its Cargo bin directory is on your PATH.
- macOS compiler or linker errors: Install or update the Xcode Command Line Tools with xcode-select --install.
- Dependency or compilation errors: Update to a current stable Rust toolchain with rustup update stable, then retry the build.
- Stale build artifacts: Run cargo clean and build again.

If a problem persists, include the full command you ran and the complete error output when reporting it.

## Project information

- Workspace: Cargo
- Rust edition: 2024
- GUI crate: spotipod-gui
- Core crate: spotipod-core
- License: PolyForm Noncommercial License 1.0.0 (see LICENSE)
- Repository: https://github.com/AsQqqq/SpotiPOD