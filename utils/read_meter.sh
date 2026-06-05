#!/bin/sh

# Set notify interval to 1s
blescan write --name INA219 --characteristic 00000005-9b04-4347-98ff-57e8f7803509::1_u32
blescan notify --name INA219 --characteristic 00000003-9b04-4347-98ff-57e8f7803509 \
                             --decode '00000003-9b04-4347-98ff-57e8f7803509::struct<f32,f32>' \
                             --json |\
    jq -c '{ "V": .notification.decoded[0], "mA": .notification.decoded[1]}'

