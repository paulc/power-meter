#!/bin/sh

BLESCAN=~/.local/bin/blescan
STDIN_SSE=~/.local/bin/stdin-sse
JQ=jq

# Set notify interval to 1s
$BLESCAN write --name INA219 --characteristic 00000005-9b04-4347-98ff-57e8f7803509::1_u32
$BLESCAN notify --name INA219 --characteristic 00000003-9b04-4347-98ff-57e8f7803509 --decode 00000003-9b04-4347-98ff-57e8f7803509::u64 --json | \
    $JQ --unbuffered --compact-output 'select(.notification) | {value: .notification.value}' | \
    $STDIN_SSE --endpoint /update --event-type update --cors --debug --index "$(dirname $0)/ina219_reading.html"

# $BLESCAN poll --name INA219 --characteristic 00000003-9b04-4347-98ff-57e8f7803509 --decode 00000003-9b04-4347-98ff-57e8f7803509::u64 --interval 1 --json | \
#     $JQ --unbuffered --compact-output 'paths(scalars) as $p | select(getpath($p) and ($p[-1]=="value")) | {value: getpath($p)}' | \
#     $STDIN_SSE --endpoint /update --event-type update --cors --debug --index "$(dirname $0)/ina219_reading.html"
