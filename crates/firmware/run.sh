#!/bin/sh
# Cargo runner: fill the crash report symbol table, then flash and open the serial console
set -e
elf=$(realpath "$1")
dir=$(dirname "$0")
# Built from its own directory, so this crate's target and build-std settings do not apply
(cd "$dir/../symtab" && cargo run --release --quiet -- "$elf")
# Erasing otadata boots the flashed slot, ota_0, even after an update selected ota_1
exec espflash flash --monitor --chip esp32s3 --partition-table "$dir/partitions.csv" \
  --erase-parts otadata "$elf"
