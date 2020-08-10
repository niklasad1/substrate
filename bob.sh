#!/usr/bin/env bash

rm -rf /tmp/bob

./target/debug/substrate \
  --base-path /tmp/bob \
  --chain local \
  --execution Native \
  --bob \
  --node-key bb5b9c4a9a3723d08d49c6071d96b6037af7eb22d6c05f0554c9fbcb6cd043bb \
  --port 30334 \
  --ws-port 9945 \
  --rpc-port 9934 \
  --validator \
  --bootnodes /ip4/127.0.0.1/tcp/30333/p2p/12D3KooWJzEnsvTvmV4hpL73JRvAuwfZ87xXNck2GCK52pGP9RP1 \
  --offchain-worker Never \
  -lsgx=trace
