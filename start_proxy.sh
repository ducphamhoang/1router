#!/bin/bash
cd /home/ducph/duc/1router
exec ./target/release/1router >> /home/ducph/duc/1router/proxy.log 2>&1
