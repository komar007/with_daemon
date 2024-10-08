# Changelog of `with_daemon`

## [0.2.0] - 2024-09-14

### 🚀 Features

- [**breaking**] Basic daemon control with shutdown capability
- [**breaking**] Meaningful error result instead of string
- Removed unneeded deps

### 📚 Documentation

- Fixed 2 typos
- Updated example

### ⚙️ Miscellaneous Tasks

- Added release build to PR checks
- Updated upload-artifact from v2 to v4

## [0.1.0] - 2024-09-09

### 🚀 Features

- Extracted an initial version of client/daemon framework
- [**breaking**] Turned framework into a separate crate: with_daemon
- Kept daemon's stdout and stderr connected to the starting client
- [**breaking**] Simplified return type, just exitting the forked child process now

### 🐛 Bug Fixes

- Fixed a bug at first start (couldn't remove sock file)
- Fixed type deamon -> daemon

### 📚 Documentation

- Framework: added basic documentation
- Added examples
- Updated licenses
- Updated Cargo.toml metadata
- Added README
- Added changelog

### ⚙️ Miscellaneous Tasks

- Added crates-publish action

### PoC

- Client timeout + introduced clap for arg parsing

<!-- generated by git-cliff -->
