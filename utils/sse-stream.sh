#!/bin/sh

# Set notify interval to 1s
blescan write --name INA219 --characteristic 00000005-9b04-4347-98ff-57e8f7803509::1_u32
# Run SSE server
blescan notify --name INA219 --characteristic 00000003-9b04-4347-98ff-57e8f7803509 --decode 00000003-9b04-4347-98ff-57e8f7803509::u64 --json | \
    jq --unbuffered --compact-output 'select(.notification) | {value: .notification.value}' | \
    stdin_sse --endpoint /update --event-type update --cors --debug --index "$(dirname $0)/ina219_reading.html"

