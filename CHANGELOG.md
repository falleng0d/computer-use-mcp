# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
## [0.3.0](https://github.com/falleng0d/computer-use-mcp/compare/v0.2.0...v0.3.0) - 2026-10-04

### Added

- publish the host binary and its crates to crates.io

- *(computer-use-mcp)* add start_chrome_devtools and stop_chrome_devtools

- *(computerd)* install chrome-devtools-mcp and mcpc with an idle wrapper

- *(computer-use-mcp)* transfer files and folders with file_transfer

- *(computerd)* copy and paste between the host and an unlocked viewer

- *(computerd)* add a viewer top bar with Lock and Unlock

- *(computerd)* install LibreOffice Writer, Calc, and Impress

- *(computerd)* search Google's Web view from the address bar

- *(computerd)* add a dock and restyle the desktop


### Fixed

- *(computerd)* keep long DevTools calls alive and bound every mcpc call

- *(computer-use-mcp)* fail an upload when the computer stops reading

- *(computer-use-mcp)* keep long transfers alive and fail cleanly on cut streams

- *(computerd)* release Cmd and Ctrl after a viewer paste and match layouts by character

- *(computerd)* ignore events from a replaced viewer connection

- *(computerd)* open the LibreOffice Start Center and skip crash recovery


### Maintenance

- *(computerd)* tidy the image build and note the DevTools shell timeout

- *(Justfile)* add `rm` recipe to stop and remove Docker container

- add .gitattributes file


### Tests

- *(computer-transfer)* satisfy clippy on Linux-only tests

## [0.2.0](https://github.com/falleng0d/computer-use-mcp/compare/v0.1.0...v0.2.0) - 2026-10-04

### Added

- *(computer-use-mcp)* accept duration units for shell timeouts

- *(computer-use-mcp)* follow the Docker CLI context to pick the endpoint

- *(computer-use-mcp)* upgrade a stopped computer to a newer image

- *(computerd)* share browser logins across screens

- *(computerd)* let agents open pages and apps on their screen

- *(computer-use-mcp)* open the viewer when a session's screen opens

- *(computerd)* let the user watch and use screens in a browser or VNC client

- *(computerd)* end sessions whose owner is gone or that sit idle

- *(computerd)* let agents list, read, and write files with list_files, read_file, and write_file

- *(computerd)* let agents run shell commands with shell and set_cwd

- *(computer-use-mcp)* let agents act on their own screen with computer_act

- *(computer-use-mcp)* let agents see their own screen with computer_observe

- *(computer-use-mcp)* start the computer from start_computer

- *(computerd)* install the agreed software on the computer


### Build

- *(computerd)* wrap collapsed RUN lines in the Dockerfile


### Changed

- *(computer-use-mcp)* make items private or pub(crate)

- *(computer-use-mcp)* move Docker and RFB test helpers out of server.rs

- *(computerd)* make items private or pub(crate)

- *(computerd)* split screen.rs and keep signals in one module

- *(computerd)* drop the old screen-N profile migration


### Fixed

- *(computer-use-mcp)* never start a computer that already runs

- *(computer-use-mcp)* join a computer another process is starting

- *(computer-use-mcp)* report a broken Docker config and refuse TLS only for tcp endpoints

- *(computer-use-mcp)* recreate only our own exited computer

- *(computerd)* check browser URLs on every path and tighten app launching

- *(computer-use-mcp)* report an opened screen even when its first call failed

- *(computerd)* harden viewer access and detect vanished viewers

- *(computerd)* end sessions without waiting on busy screens

- *(computerd)* refuse pipes and read-only files and write through dangling symlinks

- *(computerd)* stop running commands when a session ends or the computer shuts down

- *(computerd)* keep long typing in budget and release buttons after a failed batch

- *(computerd)* keep screens from leaking, hanging, or staying dead

- *(computer-use-mcp)* survive concurrent starts and bad session ids

- *(computerd)* open Chromium maximized on its screen

- *(computerd)* harden shared logins and share browser settings

- *(computerd)* create the X socket folder so concurrent screens can start

- *(computerd)* drop PIP_USER so venvs work and pin the AWS CLI signer


### Tests

- *(computer-use-mcp)* serve the cookie test page from a threaded server

- *(computer-use-mcp)* pick test port blocks below the Windows ephemeral range

- *(computer-use-mcp)* wait for the screen close before checking for Chromium

## [0.1.0](https://github.com/falleng0d/computer-use-mcp/releases/tag/v0.1.0) - 2026-10-03

### Build

- set up Rust workspace, CI, and release pipeline

