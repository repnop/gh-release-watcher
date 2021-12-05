# `gh-release-watcher`

This simple binary allows you to automatically watch and download releases from
repos you specify from within the config file.

## Config Format

The configuration file uses `TOML` and an example configuration file can be
found at [example_config.toml](example_config.toml).

## Running

Requires a Rust toolchain to be installed. Simply clone this repo, `cd` into it
and run `cargo run --release -- <path to config>`. I recommend making some kind
of `systemd` service for the binary so it'll start on boot.

## TODO Features

- Config watching & reloading