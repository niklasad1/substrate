#!/usr/bin/env bash

rm -rf /tmp/alice

./target/debug/substrate \
  --base-path /tmp/alice \
  --chain local \
  --alice \
  --node-key ab5b9c4a9a3723d08d49c6071d96b6037af7eb22d6c05f0554c9fbcb6cd043a5 \
  --execution Native \
  --port 30333 \
  --ws-port 9944 \
  --rpc-port 9933 \
  --validator \
  --rpc-methods Unsafe \
  -lsgx=trace, txpool=trace, offchain=trace
