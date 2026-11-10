#!/bin/sh
# Cargo runner: fill the crash report symbol table, then flash and open the serial console
set -e
elf=$(realpath "$1")
# Built from its own directory, so this crate's target and build-std settings do not apply
(cd "$(dirname "$0")/../symtab" && cargo run --release --quiet -- "$elf")
exec espflash flash --monitor --chip esp32s3 "$elf"
