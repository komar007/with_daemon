#!/bin/sh

# For whatever reason, somewhere during cargo msrv, Cargo.lock with version = 4 is created
# and then a perfectly valid old version of rust fails, because it is too new...
rm -f Cargo.lock

cargo check --lib
