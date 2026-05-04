#!/bin/sh

BLESCAN=~/Temp/blescan/target/release/blescan
STDIN_SSE=~/Temp/stdin-sse/target/release/stdin-sse
JQ=jq

$BLESCAN poll --name POWER_METER --characteristic 00000003-9b04-4347-98ff-57e8f7803509 --decode 00000003-9b04-4347-98ff-57e8f7803509::u64 --interval 1 --json | \
    $JQ --unbuffered --compact-output 'paths(scalars) as $p | select(getpath($p) and ($p[-1]=="value")) | {value: getpath($p)}' | \
    $STDIN_SSE --endpoint /update --event-type update --debug --index "$(dirname $0)/ina219_reading.html"
